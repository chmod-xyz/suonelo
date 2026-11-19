use std::{io::BufWriter, path::PathBuf, sync::mpsc};

use tokio::sync::oneshot;

use super::*;

#[test]
#[ignore = "run through scripts/measure-flac.ps1"]
fn generate_pcm() {
    let path = std::env::var_os("SUONELO_FLAC_PCM").unwrap();
    let seconds: u64 = std::env::var("SUONELO_FLAC_SECONDS")
        .unwrap()
        .parse()
        .unwrap();
    let mut output = BufWriter::new(File::create(path).unwrap());
    for frame in 0..192_000 * seconds {
        // Unequal, periodic stereo channels. Use signed 24-bit PCM including extrema.
        let left = ((frame * 697 % 4096) as i32 - 2048) * 4096;
        let right = ((frame * 149 % 8192) as i32 - 4096) * 2048 + 2047;
        output.write_all(&left.to_le_bytes()[..3]).unwrap();
        output.write_all(&right.to_le_bytes()[..3]).unwrap();
    }
    output.flush().unwrap();
}

#[tokio::test]
#[ignore = "run through scripts/measure-flac.ps1"]
async fn repair_and_verify() {
    let source = PathBuf::from(std::env::var_os("SUONELO_FLAC_INPUT").unwrap());
    let jobs: usize = std::env::var("SUONELO_FLAC_JOBS").unwrap().parse().unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for job in 0..jobs {
        let source = source.clone();
        tasks.spawn(async move {
            let stage = source.with_file_name(format!("native-{job}.flac"));
            File::create(&stage).unwrap();
            let cancel = CancellationToken::new();
            repair(&source, &stage, &cancel).await.unwrap();
            verify(&stage, &cancel).await.unwrap();
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
}

fn gated_worker(
    cancel: CancellationToken,
    started: oneshot::Sender<()>,
    gate: mpsc::Receiver<()>,
) -> tokio::task::JoinHandle<Result<(), MediaError>> {
    tokio::spawn(async move {
        run(&cancel, move |work| {
            started.send(()).unwrap();
            gate.recv_timeout(Duration::from_secs(10)).unwrap();
            work.check()
        })
        .await
    })
}

#[tokio::test]
#[ignore = "requires an isolated test process because it saturates the global codec semaphore"]
async fn worker_bound_and_queued_cancellation() {
    let active_cancel = CancellationToken::new();
    let (first_started, wait_first) = oneshot::channel();
    let (first_release, first_gate) = mpsc::channel();
    let first = gated_worker(active_cancel.clone(), first_started, first_gate);
    let (second_started, wait_second) = oneshot::channel();
    let (second_release, second_gate) = mpsc::channel();
    let second = gated_worker(CancellationToken::new(), second_started, second_gate);

    tokio::time::timeout(Duration::from_secs(5), async {
        wait_first.await.unwrap();
        wait_second.await.unwrap();
    })
    .await
    .unwrap();
    assert_eq!(CODEC_WORKERS.available_permits(), 0);

    let queued_cancel = CancellationToken::new();
    let queued = run(&queued_cancel, |_| panic!("cancelled queued job started"));
    tokio::pin!(queued);
    assert!(tokio::time::timeout(Duration::from_millis(50), &mut queued)
        .await
        .is_err());

    queued_cancel.cancel();
    let queued_error = tokio::time::timeout(Duration::from_secs(1), queued)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(queued_error.code, MediaError::cancelled().code);

    active_cancel.cancel();
    assert_eq!(CODEC_WORKERS.available_permits(), 0);
    first_release.send(()).unwrap();
    assert_eq!(
        first.await.unwrap().unwrap_err().code,
        MediaError::cancelled().code
    );
    assert_eq!(CODEC_WORKERS.available_permits(), 1);

    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codec-24-192000-2.flac");
    verify(&fixture, &CancellationToken::new()).await.unwrap();
    second_release.send(()).unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(CODEC_WORKERS.available_permits(), 2);
}
