use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
    time::{Duration, Instant},
};

use decibri_decode::{DecodeError, FlacStreamDecoder, FlacStreamInfo, StreamSource};
use md5::{Digest, Md5};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::MediaError;

const CODEC_DEADLINE: Duration = Duration::from_secs(1_800);
const STREAMINFO_PREFIX: [u8; 8] = *b"fLaC\0\0\0\x22";
const STREAMINFO_SIZE: usize = 42;
const MD5_OFFSET: usize = 26;
const COUNT_OFFSET: usize = 21;
const MAX_SAMPLES: u64 = (1 << 36) - 1;
const READ_BUFFER_BYTES: usize = 64 * 1024;
const PCM_BATCH_SAMPLES: usize = 128 * 1024;
const MAX_FRAMES_PER_BATCH: usize = 32;
static CODEC_WORKERS: Semaphore = Semaphore::const_new(2);

struct Work<'a> {
    cancel: &'a CancellationToken,
    deadline: Instant,
}

impl Work<'_> {
    fn check(&self) -> Result<(), MediaError> {
        if self.cancel.is_cancelled() {
            return Err(MediaError::cancelled());
        }
        if Instant::now() >= self.deadline {
            return Err(MediaError::new(
                "flac_timeout",
                "FLAC processing exceeded its time limit.",
                true,
            ));
        }
        Ok(())
    }
}

async fn run(
    cancel: &CancellationToken,
    work: impl FnOnce(Work<'_>) -> Result<(), MediaError> + Send + 'static,
) -> Result<(), MediaError> {
    let permit = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(MediaError::cancelled()),
        permit = CODEC_WORKERS.acquire() => permit.expect("codec semaphore never closes"),
    };
    let token = cancel.child_token();
    let _cancel_on_drop = token.clone().drop_guard();
    super::blocking(move || {
        let _permit = permit;
        let state = Work {
            cancel: &token,
            deadline: Instant::now() + CODEC_DEADLINE,
        };
        state.check()?;
        work(state)
    })
    .await
}

pub(super) async fn repair(
    input: &Path,
    output: &Path,
    cancel: &CancellationToken,
) -> Result<(), MediaError> {
    let input = input.to_owned();
    let output = output.to_owned();
    run(cancel, move |work| repair_file(&input, &output, work)).await
}

pub(super) async fn verify(path: &Path, cancel: &CancellationToken) -> Result<(), MediaError> {
    let path = path.to_owned();
    run(cancel, move |work| {
        let mut file = File::open(path).map_err(MediaError::disk)?;
        scan(&mut file, &work, true).map(|_| ())
    })
    .await
}

fn repair_file(input: &Path, output: &Path, work: Work<'_>) -> Result<(), MediaError> {
    // The caller owns a separate, already-created private stage.
    let input_path = input.canonicalize().map_err(MediaError::disk)?;
    let output_path = output.canonicalize().map_err(MediaError::disk)?;
    if input_path == output_path {
        return Err(invalid(false, "A separate repair stage is required."));
    }
    let mut source = File::open(input_path).map_err(MediaError::disk)?;
    let mut stage = OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(true)
        .open(output_path)
        .map_err(MediaError::disk)?;
    // Verify the bytes actually copied into the private stage before patching
    // STREAMINFO. Every original metadata block and frame stays in place.
    let mut bytes = [0; 64 * 1024];
    loop {
        work.check()?;
        let count = source.read(&mut bytes).map_err(MediaError::disk)?;
        if count == 0 {
            break;
        }
        stage.write_all(&bytes[..count]).map_err(MediaError::disk)?;
    }
    stage.rewind().map_err(MediaError::disk)?;
    let prefix = scan(&mut stage, &work, false)?;
    work.check()?;
    stage
        .seek(SeekFrom::Start(COUNT_OFFSET as u64))
        .map_err(MediaError::disk)?;
    stage
        .write_all(&prefix[COUNT_OFFSET..])
        .map_err(MediaError::disk)?;
    stage.flush().map_err(MediaError::disk)?;
    work.check()
}

fn invalid(verification: bool, detail: impl std::fmt::Display) -> MediaError {
    if verification {
        MediaError::new(
            "flac_verification",
            format!("The finalized FLAC could not be verified. {detail}"),
            true,
        )
    } else {
        MediaError::new(
            "flac_source",
            format!("The downloaded FLAC could not be repaired. {detail}"),
            false,
        )
    }
}

fn io_error(error: io::Error, verification: bool) -> MediaError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        invalid(verification, "The FLAC stream is truncated.")
    } else {
        MediaError::disk(error)
    }
}

fn set_sample_count(prefix: &mut [u8; STREAMINFO_SIZE], count: u64) {
    // The high four bits of the first byte belong to other stream properties.
    let bytes = count.to_be_bytes();
    prefix[COUNT_OFFSET] = (prefix[COUNT_OFFSET] & 0xf0) | bytes[3];
    prefix[COUNT_OFFSET + 1..MD5_OFFSET].copy_from_slice(&bytes[4..]);
}

fn decoder_batch(data: &[u8], frame_budget: usize) -> &[u8] {
    // Every new native frame starts with FF F8 or FF F9. Treat every match as
    // a candidate, including matches inside metadata or a frame body. Decibri
    // validates the actual frames and retains at most one incomplete frame,
    // which also covers a sync code split across buffer reads.
    let end = data
        .windows(2)
        .enumerate()
        .filter_map(|(offset, bytes)| {
            (bytes[0] == 0xff && bytes[1] & 0xfe == 0xf8).then_some(offset)
        })
        .nth(frame_budget)
        .unwrap_or(data.len());
    &data[..end]
}

fn scan(
    file: &mut File,
    work: &Work<'_>,
    verification: bool,
) -> Result<[u8; STREAMINFO_SIZE], MediaError> {
    let decode_error = |error: DecodeError| invalid(verification, error);
    let mut file = BufReader::with_capacity(READ_BUFFER_BYTES, file);
    let mut prefix = [0; STREAMINFO_SIZE];
    file.read_exact(&mut prefix)
        .map_err(|error| io_error(error, verification))?;
    // Mask only the metadata header's 'last block' flag.
    let mut marker = [0; 8];
    marker.copy_from_slice(&prefix[..8]);
    marker[4] &= 0x7f;
    if marker != STREAMINFO_PREFIX {
        return Err(invalid(
            verification,
            "The FLAC STREAMINFO block is missing or invalid.",
        ));
    }
    let info = FlacStreamInfo::from_block(&prefix[8..]).map_err(decode_error)?;
    if info.bits_per_sample > 24 {
        return Err(invalid(
            verification,
            "FLAC repair and verification support up to 24 bits per sample.",
        ));
    }
    if verification && info.md5.is_none() {
        return Err(invalid(true, "The FLAC has no PCM checksum."));
    }

    // Make the decoder read to physical EOF even when STREAMINFO gives a
    // duration. Some decoders otherwise ignore surplus or damaged frames.
    let mut decoder_header = prefix;
    set_sample_count(&mut decoder_header, 0);
    let mut decoder = FlacStreamDecoder::new();
    decoder.push(&decoder_header).map_err(decode_error)?;

    let scale = (1u32 << (info.bits_per_sample - 1)) as f32;
    let width = usize::from(info.bits_per_sample.div_ceil(8));
    let channels = usize::from(info.spec.channels);
    let mut samples = Vec::new();
    let mut hasher = Md5::new();
    let mut count = 0;
    let mut pcm = Vec::new();
    // Use large reads to avoid repeatedly decoding part of a frame. Cap the
    // number of new frames per push so compressible audio still has bounded
    // PCM buffering and a cancellation checkpoint between batches. The fixed
    // frame ceiling also limits Decibri's buffer compactions on tiny frames.
    let max_frame_samples = usize::from(info.max_block_size) * channels;
    let frame_budget = (PCM_BATCH_SAMPLES / max_frame_samples).clamp(1, MAX_FRAMES_PER_BATCH);
    loop {
        work.check()?;
        let data = file.fill_buf().map_err(MediaError::disk)?;
        if data.is_empty() {
            break;
        }
        let consumed = decoder
            .push(decoder_batch(data, frame_budget))
            .map_err(decode_error)?;
        let frames = decoder
            .pull(&mut samples, usize::MAX)
            .map_err(decode_error)?;
        if consumed == 0 && frames == 0 {
            return Err(invalid(
                verification,
                "The FLAC decoder stopped before the end of the stream.",
            ));
        }
        file.consume(consumed);
        count += frames as u64;
        if count > MAX_SAMPLES || info.total_samples.is_some_and(|expected| count > expected) {
            return Err(invalid(
                verification,
                "The FLAC sample count does not match its header.",
            ));
        }
        // Normalization by a power of two and its inverse are exact for
        // signed PCM up to 24 bits. Hash the interleaved little-endian bytes.
        pcm.clear();
        for sample in samples.drain(..) {
            pcm.extend_from_slice(&((sample * scale) as i32).to_le_bytes()[..width]);
        }
        hasher.update(&pcm);
    }
    work.check()?;
    let remaining = decoder.finish(&mut samples).map_err(decode_error)?;
    if remaining != 0 || count == 0 || info.total_samples.is_some_and(|expected| count != expected)
    {
        return Err(invalid(
            verification,
            "The FLAC sample count does not match its header.",
        ));
    }
    let digest: [u8; 16] = hasher.finalize().into();
    if digest == [0; 16] || info.md5.is_some_and(|expected| digest != expected) {
        return Err(invalid(
            verification,
            "The FLAC PCM checksum does not match.",
        ));
    }
    set_sample_count(&mut prefix, count);
    prefix[MD5_OFFSET..].copy_from_slice(&digest);
    Ok(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/codec-24-192000-2.flac");

    #[test]
    fn decoder_batches_stop_before_the_next_sync_candidate() {
        let data = [0xff, 0xfa, 0xff, 0xf8, 0xff, 0xf9, 0xff, 0xf8, 0xff];
        let batch = decoder_batch(&data, 2);
        assert_eq!(batch, &data[..6]);
        assert_eq!(decoder_batch(&data[batch.len()..], 2), &data[6..]);
    }

    #[tokio::test]
    async fn verifies_dense_silent_frames_with_a_split_sync_code() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("silent.flac");
        let info = FlacStreamInfo::from_block(&FIXTURE[8..STREAMINFO_SIZE]).unwrap();
        let silence = vec![0.0; PCM_BATCH_SAMPLES * 4];
        let original = decibri_decode::FlacWriter::new(info.spec, 24)
            .with_level(0)
            .to_bytes(&silence)
            .unwrap();

        // A final PADDING block puts the first frame's FF at the end of a
        // read. The remaining frames fit in one read but exceed a PCM batch.
        let mut padded = original[..STREAMINFO_SIZE].to_vec();
        padded[4] &= 0x7f;
        let padding = (READ_BUFFER_BYTES - 1 - STREAMINFO_SIZE - 4) as u32;
        padded.push(0x81);
        padded.extend_from_slice(&padding.to_be_bytes()[1..]);
        padded.resize(READ_BUFFER_BYTES - 1, 0);
        padded.extend_from_slice(&original[STREAMINFO_SIZE..]);
        assert_eq!(padded[READ_BUFFER_BYTES - 1], 0xff);
        assert!(padded.len() < READ_BUFFER_BYTES * 2);
        std::fs::write(&path, &padded).unwrap();
        verify(&path, &CancellationToken::new()).await.unwrap();
    }

    #[tokio::test]
    async fn repairs_reference_fixtures_without_changing_frames() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.flac");
        let output = directory.path().join("stage.flac");
        File::create(&output).unwrap();
        let token = CancellationToken::new();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        for entry in std::fs::read_dir(fixtures).unwrap() {
            let path = entry.unwrap().path();
            if !path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("codec-")
                || path.extension().is_none_or(|extension| extension != "flac")
            {
                continue;
            }
            let original = std::fs::read(&path).unwrap();
            verify(&path, &token).await.unwrap();
            let mut missing = original.clone();
            missing[MD5_OFFSET..STREAMINFO_SIZE].fill(0);
            set_sample_count((&mut missing[..STREAMINFO_SIZE]).try_into().unwrap(), 0);
            std::fs::write(&input, &missing).unwrap();
            assert_eq!(
                verify(&input, &token).await.unwrap_err().code,
                "flac_verification"
            );
            repair(&input, &output, &token).await.unwrap();
            assert_eq!(std::fs::read(&output).unwrap(), original, "{path:?}");
            assert_eq!(std::fs::read(&input).unwrap(), missing);
            verify(&output, &token).await.unwrap();
        }
    }

    #[tokio::test]
    async fn rejects_bad_metadata_integrity_and_format() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.flac");
        let output = directory.path().join("stage.flac");
        File::create(&output).unwrap();
        let token = CancellationToken::new();
        let mut invalid_inputs = Vec::new();

        let mut bad_md5 = FIXTURE.to_vec();
        bad_md5[MD5_OFFSET] ^= 1;
        invalid_inputs.push(("incorrect checksum", bad_md5));

        let mut bad_length = FIXTURE.to_vec();
        bad_length[25] ^= 1;
        invalid_inputs.push(("incorrect sample count", bad_length));

        let mut bad_metadata = FIXTURE.to_vec();
        bad_metadata[7] = 33;
        invalid_inputs.push(("incorrect STREAMINFO length", bad_metadata));

        let mut bad_depth = FIXTURE.to_vec();
        bad_depth[21] &= 0x0f;
        bad_depth[21] |= 0xf0;
        invalid_inputs.push(("unsupported bit depth", bad_depth));

        let mut bad_channels = FIXTURE.to_vec();
        bad_channels[20] ^= 0x02;
        invalid_inputs.push(("frame channel count mismatch", bad_channels));

        let mut bad_crc = FIXTURE.to_vec();
        *bad_crc.last_mut().unwrap() ^= 1;
        invalid_inputs.push(("bad frame CRC", bad_crc));

        invalid_inputs.push(("truncated frame", FIXTURE[..FIXTURE.len() - 1].to_vec()));
        invalid_inputs.push(("trailing bytes", [FIXTURE, b"junk"].concat()));

        for (name, bytes) in invalid_inputs {
            std::fs::write(&input, bytes).unwrap();
            assert_eq!(
                repair(&input, &output, &token).await.unwrap_err().code,
                "flac_source",
                "{name}"
            );
            assert_eq!(
                verify(&input, &token).await.unwrap_err().code,
                "flac_verification",
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn cancellation_waits_for_the_worker_and_allows_another_job() {
        let cancel = CancellationToken::new();
        let (started, wait_for_start) = tokio::sync::oneshot::channel();
        let (stopped, wait_for_stop) = tokio::sync::oneshot::channel();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            run(&task_cancel, move |work| {
                started.send(()).unwrap();
                loop {
                    if let Err(error) = work.check() {
                        stopped.send(()).unwrap();
                        return Err(error);
                    }
                    std::thread::park_timeout(Duration::from_millis(1));
                }
            })
            .await
        });
        wait_for_start.await.unwrap();
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codec-24-192000-2.flac");
        verify(&fixture, &CancellationToken::new()).await.unwrap();
        cancel.cancel();
        assert_eq!(
            task.await.unwrap().unwrap_err().code,
            MediaError::cancelled().code
        );
        wait_for_stop.await.unwrap();
        assert_eq!(
            verify(&fixture, &cancel).await.unwrap_err().code,
            MediaError::cancelled().code
        );
    }

    #[test]
    fn expired_deadline_is_reported() {
        let token = CancellationToken::new();
        let state = Work {
            cancel: &token,
            deadline: Instant::now(),
        };
        assert_eq!(state.check().unwrap_err().code, "flac_timeout");
    }
}

#[cfg(test)]
mod resource_test;
