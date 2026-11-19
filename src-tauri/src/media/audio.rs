use std::{
    ffi::OsStr,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use lofty::{
    config::ParseOptions,
    file::AudioFile,
    mpeg::{Layer, MpegFile},
};
use serde::Serialize;
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;

use super::output_store::{CheckCancelled, Receipt, Stage, safe_directory};
use super::{
    ArtworkCache, AudioFormat, AudioReplacement, DeliveredQuality, LyricsMode, Media, MediaError,
    OutputOptions, ReleaseLayout, TransferProgress, blocking, cancellable, codec, copy_stage,
    existing_root, lock, metadata, plan_output,
};
use crate::qobuz::{Credentials, Lyrics, LyricsSource, QobuzClient, ResolvedTrack, StreamOffer};
use crate::recovery::{RetryBudget, TransferContext};

const AUDIO_LIMIT: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Clone, Copy)]
pub struct AudioRequest<'a> {
    pub client: &'a QobuzClient,
    pub credentials: &'a Credentials,
    pub track: &'a ResolvedTrack,
    pub options: &'a OutputOptions,
    pub root: &'a Path,
    pub pinned: Option<&'a ReleaseLayout>,
    pub sequence: usize,
    pub transfer: &'a TransferContext,
    pub replacement: Option<&'a AudioReplacement>,
    #[cfg(target_os = "macos")]
    pub require_full_duration: bool,
    pub artwork_cache: &'a ArtworkCache,
    pub cover_basename: &'a str,
}

pub struct AudioOutput {
    #[cfg(target_os = "macos")]
    pub is_preview: bool,
    pub finalized_at: Option<i64>,
    pub path: PathBuf,
    pub layout: ReleaseLayout,
    pub quality: DeliveredQuality,
    pub warning: Option<MediaError>,
    pub lyrics_timed: Option<bool>,
    pub lyrics_source: Option<LyricsSource>,
    pub reused: bool,
    pub cover_path: Option<PathBuf>,
    pub cover_warning: Option<MediaError>,
    pub cover_embedded: bool,
}

#[derive(Default)]
struct Enrichment {
    lyrics: Option<Lyrics>,
    lyrics_warning: Option<MediaError>,
    cover_path: Option<PathBuf>,
    cover_warning: Option<MediaError>,
    cover_embedded: bool,
}

pub struct LyricsOutput {
    pub path: PathBuf,
    pub timed: bool,
    pub source: LyricsSource,
}

#[derive(Eq, Hash, PartialEq)]
pub(super) struct StagedKey {
    track: String,
    root: PathBuf,
    tier: u8,
    preview_only: bool,
}

pub(super) struct StagedAudio {
    pub file: Stage,
    pub owner: u64,
    properties: AudioProperties,
    tier: u8,
}

#[derive(Clone, Copy)]
enum AudioProperties {
    Flac(FlacInfo),
    Mp3 {
        sample_rate: u32,
        #[cfg(target_os = "macos")]
        duration_ms: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FlacInfo {
    sample_rate: u32,
    channels: u8,
    bit_depth: u8,
    samples: u64,
    checksum: [u8; 16],
}

impl FlacInfo {
    fn known_checksum(self) -> bool {
        self.checksum != [0; 16]
    }

    fn matches_pcm(self, new: Self) -> bool {
        self.sample_rate == new.sample_rate
            && self.channels == new.channels
            && self.bit_depth == new.bit_depth
            && (self.samples == 0 || self.samples == new.samples)
            && new.known_checksum()
            && (!self.known_checksum() || self.checksum == new.checksum)
    }
}

fn bad_audio() -> MediaError {
    MediaError::new(
        "audio_format",
        "The transferred file did not match the expected audio container.",
        true,
    )
}

fn read_flac(path: &Path) -> Result<FlacInfo, MediaError> {
    let mut input = File::open(path).map_err(MediaError::disk)?;
    let mut stream_info = [0_u8; 42];
    input
        .read_exact(&mut stream_info)
        .map_err(|_| bad_audio())?;
    if &stream_info[..4] != b"fLaC"
        || stream_info[4] & 0x7f != 0
        || stream_info[5..8] != [0, 0, 34]
        || input.metadata().map_err(MediaError::disk)?.len() <= stream_info.len() as u64
    {
        return Err(bad_audio());
    }
    let properties = u64::from_be_bytes(
        stream_info[18..26]
            .try_into()
            .expect("STREAMINFO properties"),
    );
    let sample_rate = (properties >> 44) as u32;
    let channels = ((properties >> 41) & 0x7) as u8 + 1;
    let bit_depth = ((properties >> 36) & 0x1f) as u8 + 1;
    let samples = properties & ((1_u64 << 36) - 1);
    let checksum = stream_info[26..42].try_into().expect("STREAMINFO checksum");
    if sample_rate == 0 || bit_depth < 4 {
        return Err(bad_audio());
    }
    Ok(FlacInfo {
        sample_rate,
        channels,
        bit_depth,
        samples,
        checksum,
    })
}

async fn inspect(path: &Path, format: AudioFormat) -> Result<AudioProperties, MediaError> {
    let path = path.to_owned();
    blocking(move || match format {
        AudioFormat::Flac => read_flac(&path).map(AudioProperties::Flac),
        AudioFormat::Mp3 => {
            let mut file = File::open(path).map_err(MediaError::disk)?;
            let audio =
                MpegFile::read_from(&mut file, ParseOptions::new()).map_err(|_| bad_audio())?;
            let properties = audio.properties();
            if properties.duration().is_zero()
                || *properties.layer() != Layer::Layer3
                || properties.sample_rate() == 0
            {
                return Err(bad_audio());
            }
            Ok(AudioProperties::Mp3 {
                sample_rate: properties.sample_rate(),
                #[cfg(target_os = "macos")]
                duration_ms: properties.duration().as_millis().min(u128::from(u64::MAX)) as u64,
            })
        }
    })
    .await
}

#[cfg(target_os = "macos")]
fn verify_full_duration(track: &ResolvedTrack, properties: AudioProperties) -> Result<(), MediaError> {
    let expected = track.track["duration"].as_f64().filter(|s| s.is_finite() && *s > 0.0);
    let actual = match properties {
        AudioProperties::Flac(info) => info.samples as f64 / f64::from(info.sample_rate),
        AudioProperties::Mp3 { duration_ms, .. } => duration_ms as f64 / 1_000.0,
    };
    if !expected.is_some_and(|seconds| actual > 0.0 && (actual - seconds).abs() <= (seconds * 0.02).max(2.0)) {
        return Err(MediaError::new("full_track_duration", "The audio duration could not be verified against the full track. No full-track file was saved.", false));
    }
    Ok(())
}

fn actual_tier(depth: u32, hz: f64) -> Option<u8> {
    match (depth, hz) {
        (16, 44.1) => Some(2),
        (16..=24, 44.1..=96.0) => Some(3),
        (16..=24, 96.0..=192.0) => Some(4),
        _ => None,
    }
}

fn offered_quality(offer: &StreamOffer) -> Option<DeliveredQuality> {
    if offer.tier == 1 {
        return Some(DeliveredQuality {
            tier: 1,
            format: AudioFormat::Mp3,
            bit_depth: None,
            sample_rate: Some(offer.sample_rate.unwrap_or(44.1)),
        });
    }
    let (depth, rate) = (offer.bit_depth?, offer.sample_rate?);
    let tier = actual_tier(depth, rate)?;
    (tier <= offer.tier).then_some(DeliveredQuality {
        tier,
        format: AudioFormat::Flac,
        bit_depth: Some(depth),
        sample_rate: Some(rate),
    })
}

fn delivered_quality(
    properties: AudioProperties,
    offer_tier: u8,
) -> Result<DeliveredQuality, MediaError> {
    match properties {
        AudioProperties::Mp3 { sample_rate, .. } if offer_tier == 1 => Ok(DeliveredQuality {
            tier: 1,
            format: AudioFormat::Mp3,
            bit_depth: None,
            sample_rate: Some(f64::from(sample_rate) / 1_000.0),
        }),
        AudioProperties::Flac(info) => {
            let depth = u32::from(info.bit_depth);
            let rate = f64::from(info.sample_rate) / 1_000.0;
            let tier = actual_tier(depth, rate)
                .filter(|tier| *tier <= offer_tier)
                .ok_or_else(|| {
                    MediaError::new(
                        "audio_quality",
                        "The delivered FLAC properties were outside the offered quality range.",
                        true,
                    )
                })?;
            Ok(DeliveredQuality {
                tier,
                format: AudioFormat::Flac,
                bit_depth: Some(depth),
                sample_rate: Some(rate),
            })
        }
        _ => Err(bad_audio()),
    }
}

fn audio_receipt(
    request: AudioRequest<'_>,
    quality: &DeliveredQuality,
    embedded: bool,
    cover_embedded: bool,
) -> Result<Receipt, MediaError> {
    let options = request.options;
    #[derive(Serialize)]
    struct Variant<'a> {
        quality: &'a DeliveredQuality,
        tags: std::collections::BTreeMap<&'a str, bool>,
        reencode_flac: bool,
        embedded: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        lyrics_missing: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cover: Option<(super::ArtworkResolution, bool)>,
    }
    let defaults = metadata::metadata_catalog();
    let mut tags: std::collections::BTreeMap<_, _> = options
        .tags
        .iter()
        .filter_map(|(key, &enabled)| {
            let default = defaults
                .iter()
                .find(|field| &field.id == key)
                .is_some_and(|field| field.default_enabled);
            (enabled != default).then_some((key.as_str(), enabled))
        })
        .collect();
    // Earlier receipts encoded ReplayGain as on by default. Keep that comparison
    // baseline so existing receipts can only match the same effective option.
    tags.remove("replaygain");
    if options.tags.get("replaygain") != Some(&true) {
        tags.insert("replaygain", false);
    }
    let variant = Variant {
        quality,
        tags,
        // Preserve the existing receipt fingerprint for the default processing path.
        reencode_flac: false,
        embedded,
        lyrics_missing: (options.lyrics_mode == LyricsMode::Embed && !embedded).then_some(true),
        cover: (options.download_cover_art && options.embed_cover_art)
            .then_some((options.artwork, cover_embedded)),
    };
    let variant = serde_json::to_string(&variant).map_err(|_| {
        MediaError::new(
            "file_index",
            "The output settings could not be indexed.",
            true,
        )
    })?;
    Ok(Receipt {
        kind: if request.credentials.access_mode.is_preview() { "audio_preview" } else { "audio" },
        identity: request.track.id.clone(),
        variant,
    })
}

async fn negotiate(
    request: AudioRequest<'_>,
    budget: &RetryBudget,
    cancel: &CancellationToken,
) -> Result<StreamOffer, MediaError> {
    cancellable(cancel, async {
        request
            .client
            .negotiate_stream(
                &request.track.id,
                request.options.quality,
                request.options.allow_fallback,
                request.credentials,
                budget,
            )
            .await
            .map_err(Into::into)
    })
    .await
}

#[cfg(target_os = "macos")]
fn ensure_same_offer(previous: &StreamOffer, refreshed: &StreamOffer) -> Result<(), MediaError> {
    if previous.is_preview != refreshed.is_preview || previous.tier != refreshed.tier || previous.requires_full_duration != refreshed.requires_full_duration {
        return Err(MediaError::new("media_offer_changed", "Qobuz changed the audio offer. Retry the track to select its output again.", true));
    }
    Ok(())
}

fn lyrics_unavailable() -> MediaError {
    MediaError::new(
        "lyrics_unavailable",
        "Lyrics unavailable. No usable lyrics were found for this track.",
        false,
    )
}

async fn lookup_lyrics(
    request: AudioRequest<'_>,
    cancel: &CancellationToken,
) -> Result<Lyrics, MediaError> {
    cancellable(cancel, async {
        request
            .client
            .resolve_lyrics(request.track, request.credentials)
            .await
            .map_err(|error| {
                MediaError::new(
                    "lyrics_failed",
                    format!("Lyrics failed. {}", error.message),
                    true,
                )
            })?
            .ok_or_else(lyrics_unavailable)
    })
    .await
}

impl Media {
    pub async fn offer_audio_replacement(&self, path: &Path, track: &str, cancel: &CancellationToken) -> Result<AudioReplacement, MediaError> {
        self.store.offer_replacement(path, track, cancel).await
    }

    async fn output_path(
        &self,
        request: AudioRequest<'_>,
        quality: &DeliveredQuality,
        cancel: &CancellationToken,
    ) -> Result<(PathBuf, ReleaseLayout), MediaError> {
        let (path, layout) = plan_output(
            request.root,
            request.options,
            request.track,
            quality,
            request.pinned,
            request.sequence,
        )?;
        let root = request
            .pinned
            .map_or(request.root, |layout| layout.album_root.as_path());
        let parent = path.parent().ok_or_else(bad_audio)?;
        if !tokio::fs::metadata(parent)
            .await
            .is_ok_and(|info| info.is_dir())
        {
            self.wait_for_drive(&path, request.transfer, cancel).await?;
        }
        let directory = safe_directory(root, parent)
            .await
            .map_err(|error| error.at_path(&path))?;
        let filename = if request.credentials.access_mode.is_preview() {
            let stem = path.file_stem().and_then(OsStr::to_str).ok_or_else(bad_audio)?;
            directory.join(format!("{stem} - Preview.mp3"))
        } else {
            directory.join(path.file_name().ok_or_else(bad_audio)?)
        };
        Ok((filename, layout))
    }

    pub async fn download_audio(
        &self,
        request: AudioRequest<'_>,
        cancel: &CancellationToken,
        progress: impl Fn(TransferProgress) + Send + Sync,
    ) -> Result<AudioOutput, MediaError> {
        let target = request
            .pinned
            .map_or(request.root, |layout| layout.album_root.as_path());
        let result = self.download_audio_inner(request, cancel, progress).await;
        result.map_err(|error| {
            if error.code == "disk_full" {
                request
                    .transfer
                    .disk_full(error.path.as_deref().unwrap_or(target));
                return error.at_path(target);
            }
            error
        })
    }

    async fn download_audio_inner(
        &self,
        request: AudioRequest<'_>,
        cancel: &CancellationToken,
        progress: impl Fn(TransferProgress) + Send + Sync,
    ) -> Result<AudioOutput, MediaError> {
        cancel.check()?;
        if let Some(error) = &request.track.unavailable {
            return Err(error.clone().into());
        }
        self.observe_track_metadata(request.track);
        let root = existing_root(
            request
                .pinned
                .map_or(request.root, |layout| layout.album_root.as_path()),
        )
        .await?;
        // The account mode selects the Windows lock before offer negotiation.
        #[cfg(not(target_os = "macos"))]
        let key = StagedKey {
            track: request.track.id.clone(),
            root,
            tier: request.options.quality,
            preview_only: request.credentials.access_mode.is_preview(),
        };
        #[cfg(not(target_os = "macos"))]
        let _track_lock = lock(&self.track_locks, &key, cancel).await?;

        let mut budget = request.transfer.budget();
        budget.defer_no_response = request.transfer.is_sequential();
        let offer = negotiate(request, &budget, cancel).await?;

        #[cfg(not(target_os = "macos"))]
        return self
            .download_offered_audio(request, offer, key, &budget, cancel, &progress)
            .await;

        #[cfg(target_os = "macos")]
        {
            // Scope preview behavior to this offer. The saved account access mode
            // and quality preference remain unchanged for other tracks.
            let mut credentials = request.credentials.clone();
            let mut options = request.options.clone();
            if offer.is_preview {
                credentials.access_mode = crate::qobuz::AccessMode::Preview;
                options.quality = 1;
            }
            let request = AudioRequest {
                credentials: &credentials,
                options: &options,
                require_full_duration: offer.requires_full_duration,
                replacement: if offer.is_preview {
                    None
                } else {
                    request.replacement
                },
                ..request
            };
            let key = StagedKey {
                track: request.track.id.clone(),
                root,
                tier: request.options.quality,
                preview_only: offer.is_preview,
            };
            self.download_offered_audio(request, offer, key, &budget, cancel, &progress)
                .await
                .map_err(|mut error| {
                    if credentials.access_mode.is_preview() && error.code == "output_conflict" {
                        // A preview conflict never authorizes full-audio replacement.
                        error.code = "preview_output_conflict";
                    }
                    error
                })
        }
    }

    async fn download_offered_audio(
        &self,
        request: AudioRequest<'_>,
        offer: StreamOffer,
        key: StagedKey,
        budget: &RetryBudget,
        cancel: &CancellationToken,
        progress: &(impl Fn(TransferProgress) + Send + Sync),
    ) -> Result<AudioOutput, MediaError> {
        #[cfg(target_os = "macos")]
        let _track_lock = lock(&self.track_locks, &key, cancel).await?;
        let observed_quality = offered_quality(&offer);
        let path_quality = observed_quality.clone().or_else(|| {
            let templates = [
                &request.options.folder_template,
                &request.options.filename_template,
            ];
            (!templates.iter().any(|template| {
                template.contains("%bit depth%") || template.contains("%sampling rate%")
            }))
            .then(|| DeliveredQuality::requested(offer.tier))
        });
        let reservation = if let Some(quality) = path_quality {
            let (path, layout) = self.output_path(request, &quality, cancel).await?;
            let guard = self.lock_path(&path, cancel).await?;
            let existing_quality = if observed_quality.is_some()
                || !tokio::fs::try_exists(&path)
                    .await
                    .map_err(MediaError::disk)?
            {
                observed_quality
            } else {
                Some(delivered_quality(
                    inspect(&path, quality.format).await.map_err(|_| {
                        MediaError::new(
                            "output_conflict",
                            "The existing file could not be verified for this output.",
                            false,
                        )
                        .at_path(&path)
                    })?,
                    offer.tier,
                )?)
            };
            if let Some(quality) = existing_quality {
                if let Some(enrichment) =
                    self.verify_audio(request, &path, &quality, cancel).await?
                {
                    drop(guard);
                    return Ok(Self::finish_audio(
                        path,
                        layout,
                        quality,
                        true,
                        #[cfg(target_os = "macos")]
                        request.credentials.access_mode.is_preview(),
                        enrichment,
                    ));
                }
            }
            Some((path, guard))
        } else {
            None
        };

        let retained = self.retained.lock().unwrap().remove(&key);
        let mut source = if let Some(source) = retained {
            let same_offer = source.tier == offer.tier
                && offered_quality(&offer).is_none_or(|quality| {
                    delivered_quality(source.properties, source.tier)
                        .is_ok_and(|delivered| delivered == quality)
                });
            if same_offer {
                source
            } else {
                self.store.discard(source.file).await;
                self.fetch_audio(request, offer, &key.root, budget, cancel, progress)
                    .await?
            }
        } else {
            self.fetch_audio(request, offer, &key.root, budget, cancel, progress)
                .await?
        };
        source.owner = request.transfer.attempt_key().0;
        let result = self
            .process_audio(request, &source, reservation, cancel)
            .await;
        if result.as_ref().is_err_and(|error| {
            matches!(
                error.code,
                "flac_timeout" | "flac_verification" | "tags" | "disk" | "disk_full"
            ) && error.retryable
                && !cancel.is_cancelled()
        }) {
            let discarded = {
                let mut retained = self.retained.lock().unwrap();
                if cancel.is_cancelled() {
                    Some(source)
                } else {
                    retained.insert(key, source)
                }
            };
            if let Some(source) = discarded {
                self.store.discard(source.file).await;
            }
        } else {
            self.store.discard(source.file).await;
        }
        result
    }

    async fn fetch_audio(
        &self,
        request: AudioRequest<'_>,
        mut offer: StreamOffer,
        root: &Path,
        budget: &RetryBudget,
        cancel: &CancellationToken,
        progress: &(impl Fn(TransferProgress) + Send + Sync),
    ) -> Result<StagedAudio, MediaError> {
        let mut refreshed_after_pause = false;
        loop {
            let format = if offer.tier == 1 {
                AudioFormat::Mp3
            } else {
                AudioFormat::Flac
            };
            let suffix = format!(".{}", format.extension());
            self.wait_for_drive(root, request.transfer, cancel).await?;
            let file = self.store.stage(root, &suffix).await?;
            match self
                .stream(
                    &offer.url,
                    &file,
                    AUDIO_LIMIT,
                    request.transfer,
                    cancel,
                    progress,
                )
                .await
            {
                Ok(()) => match inspect(file.path(), format).await {
                    Ok(properties) => {
                        return Ok(StagedAudio {
                            file,
                            owner: request.transfer.attempt_key().0,
                            properties,
                            tier: offer.tier,
                        });
                    }
                    Err(error) => {
                        self.store.discard(file).await;
                        return Err(error);
                    }
                },
                Err(error) => {
                    self.store.discard(file).await;
                    if error.code == "media_pause_expired" && !refreshed_after_pause {
                        refreshed_after_pause = true;
                        let refreshed = negotiate(request, budget, cancel).await?;
                        #[cfg(target_os = "macos")]
                        ensure_same_offer(&offer, &refreshed)?;
                        offer = refreshed;
                        continue;
                    }
                    if !matches!(
                        error.code,
                        "media_network" | "media_response" | "media_expired"
                    ) {
                        return Err(error);
                    }
                    if !cancellable(cancel, async { Ok(budget.wait(&error.message).await) }).await?
                    {
                        return Err(error);
                    }
                    let refreshed = negotiate(request, budget, cancel).await?;
                        #[cfg(target_os = "macos")]
                        ensure_same_offer(&offer, &refreshed)?;
                        offer = refreshed;
                }
            }
        }
    }

    async fn process_audio(
        &self,
        request: AudioRequest<'_>,
        source: &StagedAudio,
        reservation: Option<(PathBuf, OwnedMutexGuard<()>)>,
        cancel: &CancellationToken,
    ) -> Result<AudioOutput, MediaError> {
        #[cfg(target_os = "macos")]
        if request.require_full_duration {
            verify_full_duration(request.track, source.properties)?;
        }
        let quality = delivered_quality(source.properties, source.tier)?;
        let (path, layout) = self.output_path(request, &quality, cancel).await?;
        let guard = match reservation {
            Some((expected, guard)) if expected == path => guard,
            reservation => {
                drop(reservation);
                self.lock_path(&path, cancel).await?
            }
        };
        if let Some(enrichment) = self.verify_audio(request, &path, &quality, cancel).await? {
            drop(guard);
            return Ok(Self::finish_audio(
                path,
                layout,
                quality,
                true,
                #[cfg(target_os = "macos")]
                request.credentials.access_mode.is_preview(),
                enrichment,
            ));
        }

        let mut enrichment = Enrichment::default();
        if request.options.lyrics_mode == LyricsMode::Embed {
            match lookup_lyrics(request, cancel).await {
                Ok(lyrics) => enrichment.lyrics = Some(lyrics),
                Err(error) if cancel.is_cancelled() => return Err(error),
                Err(error) => enrichment.lyrics_warning = Some(error),
            }
        }

        let directory = path.parent().ok_or_else(bad_audio)?;
        if request.options.download_cover_art && request.options.embed_cover_art {
            match self
                .artwork_for_embedding(
                    request.track,
                    layout.cover_dir.as_deref().unwrap_or(Path::new("")),
                    directory,
                    request.options.artwork,
                    request.cover_basename,
                    request.transfer,
                    cancel,
                    request.artwork_cache,
                )
                .await
            {
                Ok(cover) => {
                    enrichment.cover_path = Some(cover);
                    enrichment.cover_embedded = true;
                }
                Err(error) if cancel.is_cancelled() || error.code == "disk_full" => {
                    return Err(error);
                }
                Err(error) => enrichment.cover_warning = Some(error),
            }
        }
        self.wait_for_drive(&path, request.transfer, cancel).await?;
        let processing = cancellable(cancel, async {
            self.processing
                .acquire()
                .await
                .map_err(|_| MediaError::cancelled())
        })
        .await?;
        let repaired = self.repair_flac(source, directory, cancel).await?;
        let base = repaired.as_ref().map_or(source.file.path(), Stage::path);
        let result = self
            .tag_and_publish(
                request,
                &path,
                base,
                quality.clone(),
                repaired.is_some(),
                &mut enrichment,
                cancel,
            )
            .await;
        if let Some(repaired) = repaired {
            self.store.discard(repaired).await;
        }
        drop(processing);
        drop(guard);
        let reused = result?;
        Ok(Self::finish_audio(
            path,
            layout,
            quality,
            reused,
            #[cfg(target_os = "macos")]
            request.credentials.access_mode.is_preview(),
            enrichment,
        ))
    }

    async fn verify_audio(
        &self,
        request: AudioRequest<'_>,
        path: &Path,
        quality: &DeliveredQuality,
        cancel: &CancellationToken,
    ) -> Result<Option<Enrichment>, MediaError> {
        if let Some(approval) = request.replacement {
            self.store.verify_replacement(path, &request.track.id, approval, cancel).await?;
            return Ok(None);
        }
        let lyrics = if request.options.lyrics_mode == LyricsMode::Embed {
            &[true, false][..]
        } else {
            &[false][..]
        };
        let covers = if request.options.download_cover_art && request.options.embed_cover_art {
            &[true, false][..]
        } else {
            &[false][..]
        };
        let mut conflict = None;
        for &lyrics_embedded in lyrics {
            for &cover_embedded in covers {
                let receipt = audio_receipt(
                    request,
                    quality,
                    lyrics_embedded,
                    cover_embedded,
                )?;
                match self.store.verify(path, &receipt, cancel).await {
                    Ok(true) => {
                        #[cfg(target_os = "macos")]
                        if request.require_full_duration {
                            verify_full_duration(request.track, inspect(path, quality.format).await?)?;
                        }
                        let saved = if lyrics_embedded {
                            self.store
                                .audio_lyrics_note(path)
                                .await?
                                .and_then(|(source, timed)| {
                                    let source = match source.as_str() {
                                        "qobuz" => LyricsSource::Qobuz,
                                        "lrclib" => LyricsSource::Lrclib,
                                        _ => return None,
                                    };
                                    Some(Lyrics {
                                        source,
                                        timed,
                                        text: String::new(),
                                    })
                                })
                        } else {
                            None
                        };
                        return Ok(Some(Enrichment {
                            lyrics: saved,
                            lyrics_warning: (request.options.lyrics_mode == LyricsMode::Embed
                                && !lyrics_embedded)
                                .then(lyrics_unavailable),
                            cover_embedded,
                            ..Default::default()
                        }));
                    }
                    Ok(false) => return Ok(None),
                    Err(error) if error.code == "output_conflict" => conflict = Some(error),
                    Err(error) => return Err(error),
                }
            }
        }
        conflict.map_or(Ok(None), Err)
    }

    async fn repair_flac(
        &self,
        source: &StagedAudio,
        directory: &Path,
        cancel: &CancellationToken,
    ) -> Result<Option<Stage>, MediaError> {
        let AudioProperties::Flac(original) = source.properties else {
            return Ok(None);
        };
        if original.known_checksum() {
            return Ok(None);
        }
        let repaired = self.store.stage(directory, ".flac").await?;
        let prepared = async {
            codec::repair(source.file.path(), repaired.path(), cancel).await?;
            let AudioProperties::Flac(new) = inspect(repaired.path(), AudioFormat::Flac).await?
            else {
                return Err(bad_audio());
            };
            if !original.matches_pcm(new) {
                return Err(MediaError::new(
                    "flac_verification",
                    "FLAC repair changed the PCM properties or did not provide a checksum.",
                    true,
                ));
            }
            Ok(())
        }
        .await;
        match prepared {
            Ok(()) => Ok(Some(repaired)),
            Err(error) => {
                self.store.discard(repaired).await;
                Err(error)
            }
        }
    }

    async fn tag_and_publish(
        &self,
        request: AudioRequest<'_>,
        path: &Path,
        base: &Path,
        quality: DeliveredQuality,
        repaired: bool,
        enrichment: &mut Enrichment,
        cancel: &CancellationToken,
    ) -> Result<bool, MediaError> {
        let suffix = format!(".{}", quality.format.extension());
        let edited = self
            .store
            .stage(path.parent().ok_or_else(bad_audio)?, &suffix)
            .await?;
        let prepared = async {
            let expected = inspect(base, quality.format).await?;
            let mut variants = vec![(enrichment.lyrics.is_some(), enrichment.cover_embedded)];
            if enrichment.cover_embedded { variants.push((enrichment.lyrics.is_some(), false)); }
            if enrichment.lyrics.is_some() && enrichment.cover_embedded { variants.push((false, true)); }
            if enrichment.lyrics.is_some() { variants.push((false, false)); }
            let mut variants = variants.into_iter().peekable();
            while let Some((with_lyrics, with_cover)) = variants.next() {
                let lyrics = with_lyrics.then(|| enrichment.lyrics.clone()).flatten();
                let cover = with_cover.then(|| enrichment.cover_path.clone()).flatten();
                match tag_copy(request, base, &edited, quality.format, lyrics, cover, cancel).await {
                    Ok(()) => {
                        if !with_lyrics && enrichment.lyrics.take().is_some() {
                            enrichment.lyrics_warning = Some(MediaError::new("lyrics_failed", "Lyrics failed. Embedded lyrics could not be saved.", true));
                        }
                        if !with_cover && enrichment.cover_embedded {
                            enrichment.cover_embedded = false;
                            enrichment.cover_warning = Some(MediaError::new("artwork_embed", "The cover could not be embedded.", true).at_path(path));
                        }
                        break;
                    }
                    Err(error) if variants.peek().is_none() || cancel.is_cancelled() || error.code == "disk_full" => return Err(error),
                    Err(_) => {}
                }
            }
            if let AudioProperties::Flac(expected) = expected {
                let AudioProperties::Flac(final_info) = inspect(edited.path(), AudioFormat::Flac).await? else { return Err(bad_audio()); };
                if !expected.matches_pcm(final_info) {
                    return Err(MediaError::new("flac_verification", "The finalized FLAC did not retain its PCM checksum and properties after tagging.", true));
                }
                if repaired {
                    codec::verify(edited.path(), cancel).await?;
                }
            }
            self.wait_for_drive(path, request.transfer, cancel).await
        }.await;
        if let Err(error) = prepared {
            self.store.discard(edited).await;
            return Err(error);
        }
        let mut receipt = audio_receipt(
            request,
            &quality,
            enrichment.lyrics.is_some(),
            enrichment.cover_embedded,
        )?;
        if let Some(lyrics) = &enrichment.lyrics {
            let mut variant: serde_json::Value =
                serde_json::from_str(&receipt.variant).map_err(|_| bad_audio())?;
            let provider = match lyrics.source {
                LyricsSource::Qobuz => "qobuz",
                LyricsSource::Lrclib => "lrclib",
            };
            variant["_lyrics"] = serde_json::json!([provider, lyrics.timed]);
            receipt.variant = variant.to_string();
        }
        if let Some(approval) = request.replacement {
            self.store.publish_replacement(edited, path, &receipt, approval, cancel).await
        } else {
            self.store.publish(edited, path, &receipt, cancel).await
        }
    }

    fn finish_audio(
        path: PathBuf,
        layout: ReleaseLayout,
        quality: DeliveredQuality,
        reused: bool,
        #[cfg(target_os = "macos")]
        is_preview: bool,
        enrichment: Enrichment,
    ) -> AudioOutput {
        let finalized_at = (!reused).then(crate::search::activity::event_time);
        let lyrics_timed = enrichment.lyrics.as_ref().map(|lyrics| lyrics.timed);
        let lyrics_source = enrichment.lyrics.map(|lyrics| lyrics.source);
        AudioOutput {
            #[cfg(target_os = "macos")]
            is_preview,
            finalized_at,
            path,
            layout,
            quality,
            warning: enrichment.lyrics_warning,
            lyrics_timed,
            lyrics_source,
            reused,
            cover_path: enrichment.cover_path,
            cover_warning: enrichment.cover_warning,
            cover_embedded: enrichment.cover_embedded,
        }
    }

    pub async fn download_external_lyrics(
        &self,
        request: AudioRequest<'_>,
        audio_path: &Path,
        cancel: &CancellationToken,
    ) -> Result<LyricsOutput, MediaError> {
        let lyrics = lookup_lyrics(request, cancel).await?;
        let suffix = if lyrics.timed { ".lrc" } else { ".txt" };
        let directory = audio_path.parent().ok_or_else(bad_audio)?;
        let target = audio_path.with_extension(&suffix[1..]);
        let _path_lock = self.lock_path(&target, cancel).await?;
        let receipt = Receipt {
            kind: "lyrics",
            identity: request.track.id.clone(),
            variant: format!("{:?}:{suffix}", lyrics.source),
        };
        let result = async {
            if self.store.verify(&target, &receipt, cancel).await? {
                return Ok(());
            }
            self.wait_for_drive(&target, request.transfer, cancel)
                .await?;
            let stage = self.store.stage(directory, suffix).await?;
            if let Err(error) = tokio::fs::write(stage.path(), &lyrics.text).await {
                self.store.discard(stage).await;
                return Err(MediaError::disk(error));
            }
            self.store.publish(stage, &target, &receipt, cancel).await?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            if error.code == "disk_full" {
                request.transfer.disk_full(&target);
            }
            let code = if error.code == "output_conflict" {
                error.code
            } else {
                "lyrics_failed"
            };
            return Err(MediaError::new(
                code,
                format!("Lyrics failed. {}", error.message),
                error.retryable,
            )
            .at_path(&target));
        }
        Ok(LyricsOutput {
            path: target,
            timed: lyrics.timed,
            source: lyrics.source,
        })
    }
}

async fn tag_copy(
    request: AudioRequest<'_>,
    base: &Path,
    edited: &Stage,
    format: AudioFormat,
    lyrics: Option<Lyrics>,
    cover: Option<PathBuf>,
    cancel: &CancellationToken,
) -> Result<(), MediaError> {
    copy_stage(base, edited.path(), cancel).await?;
    let path = edited.path().to_owned();
    let track = request.track.clone();
    let options = request.options.clone();
    blocking(move || {
        metadata::write_tags(
            &path,
            format,
            &track,
            &options,
            lyrics.as_ref(),
            cover.as_deref(),
        )
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn token_preview_uses_distinct_path_receipt_and_reuse_without_changing_preferences() {
        let root = tempfile::tempdir().unwrap();
        let root_path = std::fs::canonicalize(root.path()).unwrap();
        let database = root_path.join("index.sqlite3");
        rusqlite::Connection::open(&database).unwrap().execute_batch(super::super::RECEIPT_SCHEMA).unwrap();
        let mut client = QobuzClient::new().unwrap();
        client.fixture = Some(std::sync::Arc::new(crate::qobuz::ProviderFixture {
            api: Box::new(|endpoint, _, _| {
                assert_eq!(endpoint,"track/getFileUrl");
                Ok(json!({"sample":true,"format_id":5,"sampling_rate":44.1,"url":"https://example.test/preview"}))
            }),
            player_text: Box::new(|_| panic!("manual pair must not discover")),
        }));
        let media = Media::new(client.http_client().clone(), database, root_path.clone());
        let track = ResolvedTrack {
            id: "11".into(), title: "Fixture".into(), album_id:"22".into(), disc:1, number:1,
            track: json!({"title":"Fixture"}), album: json!({"title":"Album","artist":{"name":"Artist"}}).into(), unavailable:None,
        };
        let options = OutputOptions { quality:4, download_cover_art:false, folder_template:"%album name%".into(), filename_template:"%track title%".into(), ..Default::default() };
        let credentials = Credentials::default();
        let artwork_cache = ArtworkCache::default();
        let transfer = std::sync::Arc::new(crate::recovery::Recovery::default()).transfer(1,1,1, |_| {});
        let request = AudioRequest { client:&client, credentials:&credentials, track:&track, options:&options, root:&root_path, pinned:None, sequence:1, transfer:&transfer, replacement:None, require_full_duration:false, artwork_cache:&artwork_cache, cover_basename:"cover" };
        let cancel = CancellationToken::new();
        let (full_path, _) = media.output_path(request,&DeliveredQuality::requested(1),&cancel).await.unwrap();
        std::fs::write(&full_path,b"existing full audio").unwrap();
        // Seed a transferred source to exercise publication without a live CDN.
        let file = media.store.stage(&root_path,".mp3").await.unwrap();
        let mut frame = vec![0;417]; frame[..4].copy_from_slice(&[0xff,0xfb,0x90,0x64]);
        std::fs::write(file.path(),frame.repeat(10)).unwrap();
        let properties = inspect(file.path(),AudioFormat::Mp3).await.unwrap();
        media.retained.lock().unwrap().insert(StagedKey { track:track.id.clone(), root:root_path.clone(),tier:1,preview_only:true }, StagedAudio {file,owner:1,properties,tier:1});
        let output = media.download_audio(request,&cancel, |_| {}).await.unwrap();
        assert!(output.is_preview && !output.reused);
        assert_eq!(output.quality.tier,1);
        assert!(output.path.ends_with("Fixture - Preview.mp3"));
        assert_eq!(std::fs::read(&full_path).unwrap(),b"existing full audio");
        assert_eq!(options.quality,4);
        assert_eq!(credentials.access_mode,crate::qobuz::AccessMode::Unknown);
        let reused = media.download_audio(request,&cancel, |_| {}).await.unwrap();
        assert!(reused.is_preview && reused.reused);
        assert_eq!(reused.path,output.path);
        assert!(media.offer_audio_replacement(&output.path,&track.id,&cancel).await.is_err());
        std::fs::write(&output.path,b"modified by operator").unwrap();
        let error=media.download_audio(request,&cancel, |_| {}).await.err().unwrap();
        assert_eq!(error.code,"preview_output_conflict");
        assert_eq!(std::fs::read(&output.path).unwrap(),b"modified by operator");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn legacy_full_mp3_is_published_and_reused_only_with_full_duration() {
        let root=tempfile::tempdir().unwrap();
        let root_path=std::fs::canonicalize(root.path()).unwrap();
        let db=root_path.join("index.sqlite3");
        rusqlite::Connection::open(&db).unwrap().execute_batch(super::super::RECEIPT_SCHEMA).unwrap();
        let mut client=QobuzClient::new().unwrap();
        client.fixture=Some(std::sync::Arc::new(crate::qobuz::ProviderFixture {
            api:Box::new(|_,_,_| Ok(json!({"format_id":5,"sampling_rate":44.1,"url":"https://example.test/full"}))),
            player_text:Box::new(|_| panic!("manual pair")),
        }));
        let media=Media::new(client.http_client().clone(),db,root_path.clone());
        let track=ResolvedTrack {id:"12".into(),title:"Full".into(),album_id:"22".into(),disc:1,number:1,track:json!({"title":"Full","duration":0.261}),album:json!({"title":"Album","artist":{"name":"Artist"}}).into(),unavailable:None};
        let options=OutputOptions {quality:1,download_cover_art:false,folder_template:"%album name%".into(),filename_template:"%track title%".into(),..Default::default()};
        let credentials=Credentials::default();
        let artwork=ArtworkCache::default();
        let transfer=std::sync::Arc::new(crate::recovery::Recovery::default()).transfer(1,1,1, |_| {});
        let request=AudioRequest {client:&client,credentials:&credentials,track:&track,options:&options,root:&root_path,pinned:None,sequence:1,transfer:&transfer,replacement:None,require_full_duration:false,artwork_cache:&artwork,cover_basename:"cover"};
        let cancel=CancellationToken::new();
        let mut frame=vec![0;417];frame[..4].copy_from_slice(&[0xff,0xfb,0x90,0x64]);
        for short in [false,true] {
            let current=if short {ResolvedTrack {id:"13".into(),title:"Short".into(),track:json!({"title":"Short","duration":180}),..track.clone()}} else {track.clone()};
            let file=media.store.stage(&root_path,".mp3").await.unwrap();
            std::fs::write(file.path(),frame.repeat(10)).unwrap();
            let properties=inspect(file.path(),AudioFormat::Mp3).await.unwrap();
            media.retained.lock().unwrap().insert(StagedKey {track:current.id.clone(),root:root_path.clone(),tier:1,preview_only:false},StagedAudio {file,owner:1,properties,tier:1});
            let current_request=AudioRequest {track:&current,..request};
            let result=media.download_audio(current_request,&cancel, |_| {}).await;
            if short {
                assert_eq!(result.err().unwrap().code,"full_track_duration");
                assert!(!root_path.join("Album/Short.mp3").exists());
            } else {
                let output=result.unwrap();
                assert!(!output.is_preview && !output.reused);
                assert!(output.path.ends_with("Full.mp3"));
                assert!(media.download_audio(current_request,&cancel, |_| {}).await.unwrap().reused);
                assert!(media.offer_audio_replacement(&output.path,&current.id,&cancel).await.is_ok());
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn full_duration_guard_rejects_short_or_unknown_flac_and_mp3() {
        let track=ResolvedTrack {id:"42".into(),title:"Full".into(),album_id:"22".into(),disc:1,number:1,track:json!({"duration":180}),album:json!({}).into(),unavailable:None};
        for duration_ms in [0,30_000,90_000,170_000,190_000] {
            assert!(verify_full_duration(&track,AudioProperties::Mp3 {sample_rate:44_100,duration_ms}).is_err());
        }
        assert!(verify_full_duration(&track,AudioProperties::Mp3 {sample_rate:44_100,duration_ms:180_000}).is_ok());
        let info=FlacInfo {sample_rate:44_100,channels:2,bit_depth:16,samples:180*44_100,checksum:[1;16]};
        assert!(verify_full_duration(&track,AudioProperties::Flac(info)).is_ok());
        assert!(verify_full_duration(&track,AudioProperties::Flac(FlacInfo {samples:30*44_100,..info})).is_err());
        let missing=ResolvedTrack {track:json!({}),..track};
        assert!(verify_full_duration(&missing,AudioProperties::Flac(info)).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refreshed_offer_cannot_change_reserved_output_kind_or_tier() {
        let offer = |is_preview,tier| StreamOffer { url:url::Url::parse("https://example.test/audio").unwrap(),tier,format_id:5,bit_depth:None,sample_rate:None,restrictions:vec![],is_preview,requires_full_duration:false };
        assert!(ensure_same_offer(&offer(true,1),&offer(true,1)).is_ok());
        assert!(ensure_same_offer(&offer(false,1),&offer(true,1)).is_err());
        assert!(ensure_same_offer(&offer(false,4),&offer(false,3)).is_err());
    }

    #[test]
    fn reads_pcm_checksum_from_native_streaminfo() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pcm.flac");
        let mut bytes = vec![0_u8; 43];
        bytes[..4].copy_from_slice(b"fLaC");
        bytes[7] = 34;
        let properties = (44_100_u64 << 44) | (1 << 41) | (15 << 36) | 88_200;
        bytes[18..26].copy_from_slice(&properties.to_be_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let unknown = read_flac(&path).unwrap();
        assert!(!unknown.known_checksum());
        bytes[26] = 1;
        std::fs::write(&path, &bytes).unwrap();
        let known = read_flac(&path).unwrap();
        assert!(known.known_checksum());
        assert!(unknown.matches_pcm(known));
        assert!(!known.matches_pcm(unknown));
        assert_eq!(known.sample_rate, 44_100);
        assert_eq!(known.channels, 2);
        assert_eq!(known.bit_depth, 16);
    }

    #[tokio::test]
    async fn selective_clear_discards_only_removed_owners_retained_stages() {
        let root = tempfile::tempdir().unwrap();
        let client = QobuzClient::new().unwrap();
        rusqlite::Connection::open(root.path().join("index.sqlite3"))
            .unwrap()
            .execute_batch(super::super::RECEIPT_SCHEMA)
            .unwrap();
        let media = Media::new(
            client.http_client().clone(),
            root.path().join("index.sqlite3"),
            root.path().to_path_buf(),
        );
        let mut paths = Vec::new();
        for owner in [11, 22] {
            let file = media.store.stage(root.path(), ".mp3").await.unwrap();
            std::fs::write(file.path(), b"retained source").unwrap();
            paths.push(file.path().to_path_buf());
            media.retained.lock().unwrap().insert(
                StagedKey {
                    track: owner.to_string(),
                    root: root.path().to_path_buf(),
                    tier: 1,
                    preview_only: false,
                },
                StagedAudio {
                    file,
                    owner,
                    properties: AudioProperties::Mp3 {
                        sample_rate: 44_100,
                        #[cfg(target_os = "macos")]
                        duration_ms: 1000,
                    },
                    tier: 1,
                },
            );
        }
        let finalized = root.path().join("saved.mp3");
        std::fs::write(&finalized, b"finalized").unwrap();
        media.discard_retained_jobs(&[11]).await;
        assert!(!paths[0].exists());
        assert!(paths[1].exists());
        assert_eq!(media.retained.lock().unwrap().len(), 1);
        assert_eq!(std::fs::read(finalized).unwrap(), b"finalized");
        media.discard_retained_jobs(&[22]).await;
        assert!(!paths[1].exists());
    }

    #[tokio::test]
    async fn repairs_unknown_checksum_and_preserves_pcm_and_disabled_tags() {
        let root = tempfile::tempdir().unwrap();
        let root_path = std::fs::canonicalize(root.path()).unwrap();
        let database = root_path.join("index.sqlite3");
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute_batch(super::super::RECEIPT_SCHEMA)
            .unwrap();
        let qobuz = QobuzClient::new().unwrap();
        let media = Media::new(qobuz.http_client().clone(), database.clone(), root_path.clone());
        let original = root_path.join("original.flac");
        // 128 mono PCM samples, sample[n] = n * 91, and a source TITLE comment.
        let fixture = include_bytes!("../../tests/fixtures/pipeline-pcm16-mono.flac");
        std::fs::write(&original, fixture).unwrap();
        let cancel = CancellationToken::new();
        let original_info = read_flac(&original).unwrap();
        let mut data = fixture.to_vec();
        data[26..42].fill(0);
        let file = media.store.stage(&root_path, ".flac").await.unwrap();
        std::fs::write(file.path(), &data).unwrap();
        let source = StagedAudio {
            owner: 1,
            properties: inspect(file.path(), AudioFormat::Flac).await.unwrap(),
            file,
            tier: 2,
        };
        let track = ResolvedTrack {
            id: "11".into(),
            title: "Qobuz title".into(),
            album_id: "22".into(),
            disc: 2,
            number: 3,
            track: json!({"title": "Qobuz title"}),
            album:
                json!({"title": "Test album", "artist": {"name": "Test artist"}, "media_count": 2})
                    .into(),
            unavailable: None,
        };
        let mut options = OutputOptions {
            quality: 2,
            download_cover_art: false,
            folder_template: "%album artist%/%album name% [(%year%)]".into(),
            filename_template: "%track number% - %track title%".into(),
            ..Default::default()
        };
        options.tags.insert("title".into(), false);
        let credentials = Credentials::default();
        let artwork_cache = ArtworkCache::default();
        let transfer =
            std::sync::Arc::new(crate::recovery::Recovery::default()).transfer(1, 1, 1, |_| {});
        let request = AudioRequest {
            replacement: None,
            #[cfg(target_os = "macos")]
            require_full_duration: false,
            artwork_cache: &artwork_cache,
            cover_basename: &options.cover_filename,
            client: &qobuz,
            credentials: &credentials,
            track: &track,
            options: &options,
            root: &root_path,
            pinned: None,
            sequence: 1,
            transfer: &transfer,
        };
        let (path, _) = media
            .output_path(request, &DeliveredQuality::requested(2), &cancel)
            .await
            .unwrap();
        let stopped = CancellationToken::new();
        stopped.cancel();
        let error = media
            .process_audio(request, &source, None, &stopped)
            .await
            .err()
            .expect("a cancelled job must not be published");
        assert_eq!(error.code, "cancelled");
        assert!(!path.exists());

        let known_stage = media.store.stage(&root_path, ".flac").await.unwrap();
        std::fs::write(known_stage.path(), fixture).unwrap();
        let known_source = StagedAudio {
            owner: 1,
            properties: AudioProperties::Flac(original_info),
            file: known_stage,
            tier: 2,
        };
        assert!(
            media
                .repair_flac(&known_source, &root_path, &cancel)
                .await
                .unwrap()
                .is_none()
        );
        media.store.discard(known_source.file).await;

        // Retain a valid STREAMINFO header but truncate the audio frame.
        std::fs::write(source.file.path(), &data[..data.len() - 4]).unwrap();
        let error = media
            .process_audio(request, &source, None, &cancel)
            .await
            .err()
            .expect("a truncated FLAC must not be published");
        assert_eq!(error.code, "flac_source");
        assert!(!error.retryable);
        assert!(!path.exists());

        // A bad nonzero digest survives metadata editing and fails final verification.
        let mut corrupt = fixture.to_vec();
        corrupt[26] ^= 1;
        std::fs::write(&original, corrupt).unwrap();
        let error = media
            .tag_and_publish(
                request,
                &path,
                &original,
                DeliveredQuality::requested(2),
                true,
                &mut Enrichment::default(),
                &cancel,
            )
            .await
            .expect_err("a corrupt final stage must not be published");
        assert_eq!(error.code, "flac_verification");
        assert!(error.retryable);
        assert!(!path.exists());
        let connection = rusqlite::Connection::open(&database).unwrap();
        for (query, expected) in [
            ("SELECT COUNT(*) FROM media_receipts", 0),
            ("SELECT COUNT(*) FROM media_stages", 1),
        ] {
            assert_eq!(
                connection.query_row(query, [], |row| row.get::<_, i64>(0)).unwrap(),
                expected
            );
        }
        drop(connection);

        std::fs::write(source.file.path(), &data).unwrap();
        let output = media
            .process_audio(request, &source, None, &cancel)
            .await
            .unwrap();
        assert_eq!(std::fs::read(source.file.path()).unwrap(), data);
        assert!(
            output.path.ends_with(
                Path::new("Test artist")
                    .join("Test album")
                    .join("Disc 2")
                    .join("03 - Qobuz title.flac")
            )
        );
        assert_eq!(read_flac(&output.path).unwrap(), original_info);
        let mut file = File::open(&output.path).unwrap();
        let audio = lofty::flac::FlacFile::read_from(&mut file, ParseOptions::new()).unwrap();
        let tag = audio.vorbis_comments().unwrap();
        assert_eq!(tag.get("TITLE"), Some("Source title"));
        assert_eq!(tag.get("ALBUM"), Some("Test album"));
        let receipt = audio_receipt(request, &output.quality, false, false).unwrap();
        let mut variant: serde_json::Value = serde_json::from_str(&receipt.variant).unwrap();
        assert_eq!(variant["reencode_flac"], false);
        // Reuse a receipt from a profile that previously enabled recompression.
        variant["reencode_flac"] = json!(true);
        let connection = rusqlite::Connection::open(&database).unwrap();
        assert_eq!(
            connection
                .execute(
                    "UPDATE media_receipts SET variant = ?1 WHERE kind = 'audio'",
                    [variant.to_string()],
                )
                .unwrap(),
            1
        );
        drop(connection);
        assert!(
            media
                .process_audio(request, &source, None, &cancel)
                .await
                .unwrap()
                .reused
        );
        let mut with_cover = options.clone();
        with_cover.download_cover_art = true;
        with_cover.filename_template = "%track number% - %track title% - artwork".into();
        let cover_request = AudioRequest {
            replacement: None,
            #[cfg(target_os = "macos")]
            require_full_duration: false,
            options: &with_cover,
            ..request
        };
        let (_, cover_layout) = media
            .output_path(cover_request, &DeliveredQuality::requested(2), &cancel)
            .await
            .unwrap();
        let directory = cover_layout.cover_dir.as_deref().unwrap();
        std::fs::create_dir_all(directory).unwrap();
        let cover = directory.join("cover.png");
        let cover_stage = media.store.stage(directory, ".png").await.unwrap();
        use base64::Engine;
        let image = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5u0AAAAASUVORK5CYII=").unwrap();
        std::fs::write(cover_stage.path(), image).unwrap();
        let cover_receipt = Receipt {
            kind: "artwork",
            identity: track.album_id.clone(),
            variant: "Original".into(),
        };
        media
            .store
            .publish(cover_stage, &cover, &cover_receipt, &cancel)
            .await
            .unwrap();
        let output = media
            .process_audio(cover_request, &source, None, &cancel)
            .await
            .unwrap();
        assert!(output.cover_embedded);
        assert_eq!(output.cover_path, Some(cover));
        assert!(output.cover_warning.is_none());
        assert_eq!(read_flac(&output.path).unwrap(), original_info);
        let mut output_file = File::open(&output.path).unwrap();
        let output_audio =
            lofty::flac::FlacFile::read_from(&mut output_file, ParseOptions::new()).unwrap();
        use lofty::ogg::OggPictureStorage;
        assert_eq!(output_audio.pictures().len(), 1);
        assert!(
            media
                .process_audio(cover_request, &source, None, &cancel)
                .await
                .unwrap()
                .cover_embedded
        );
        let mut fallback = with_cover.clone();
        fallback.quality = 4;
        fallback.cover_filename = "Front".into();
        fallback.folder_template = "[%bit depth% Bit]/Disc %disc number%/%album name%".into();
        let staged_cache = ArtworkCache::default();
        let staged = staged_cache.seed_test_png(&media, &root_path).await;
        let disc_one = ResolvedTrack {
            id: "12".into(),
            disc: 1,
            ..track.clone()
        };
        let fallback_request = AudioRequest {
            artwork_cache: &staged_cache,
            cover_basename: &fallback.cover_filename,
            options: &fallback,
            track: &disc_one,
            ..cover_request
        };
        let first = media
            .process_audio(fallback_request, &source, None, &cancel)
            .await
            .unwrap();
        let second = media
            .process_audio(
                AudioRequest {
                    track: &track,
                    pinned: Some(&first.layout),
                    ..fallback_request
                },
                &source,
                None,
                &cancel,
            )
            .await
            .unwrap();
        assert!(
            first
                .path
                .parent()
                .unwrap()
                .ends_with("16 Bit/Disc 1/Test album")
        );
        assert!(
            second
                .path
                .parent()
                .unwrap()
                .ends_with("16 Bit/Disc 2/Test album")
        );
        let target = first.layout.cover_dir.as_deref().unwrap();
        assert!(target.ends_with("16 Bit/Test album [22]"));
        assert!(!root_path.join("24 Bit").exists());
        assert!(!target.exists());
        for output in [&first, &second] {
            assert!(output.cover_embedded && output.cover_warning.is_none());
            let mut audio = File::open(&output.path).unwrap();
            let audio = lofty::flac::FlacFile::read_from(&mut audio, ParseOptions::new()).unwrap();
            assert_eq!(audio.pictures().len(), 1);
            assert!(!output.path.parent().unwrap().join("Front.png").exists());
        }
        let saved = media
            .download_artwork(
                &disc_one,
                target,
                fallback.artwork,
                &fallback.cover_filename,
                &transfer,
                &cancel,
                Some(&staged_cache),
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(
            saved.path,
            std::fs::canonicalize(target.join("Front.png")).unwrap()
        );
        assert_eq!(
            std::fs::read(saved.path).unwrap(),
            std::fs::read(staged).unwrap()
        );
        assert_eq!(std::fs::read_dir(target).unwrap().count(), 1);

        let sidecar_options = OutputOptions {
            embed_cover_art: false,
            filename_template: "%track title% sidecar only".into(),
            ..fallback.clone()
        };
        let sidecar_only = media
            .process_audio(
                AudioRequest {
                    options: &sidecar_options,
                    pinned: Some(&first.layout),
                    ..fallback_request
                },
                &source,
                None,
                &cancel,
            )
            .await
            .unwrap();
        assert!(!sidecar_only.cover_embedded && sidecar_only.cover_warning.is_none());
        let mut audio = File::open(&sidecar_only.path).unwrap();
        let audio = lofty::flac::FlacFile::read_from(&mut audio, ParseOptions::new()).unwrap();
        assert!(audio.pictures().is_empty());
        assert!(
            media
                .download_artwork(
                    &track,
                    target,
                    fallback.artwork,
                    &fallback.cover_filename,
                    &transfer,
                    &cancel,
                    None,
                    |_| {},
                )
                .await
                .unwrap()
                .reused
        );
        media.discard_artwork(staged_cache).await;
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;

            let retry_options = OutputOptions {
                filename_template: "%track title% retry".into(),
                ..options.clone()
            };
            let retry_request = AudioRequest {
                options: &retry_options,
                ..request
            };
            let offer = || StreamOffer {
                url: url::Url::parse("file:///unexpected-refetch.flac").unwrap(),
                tier: 2,
                format_id: 6,
                bit_depth: Some(16),
                sample_rate: Some(44.1),
                restrictions: Vec::new(),
            };
            let key = || StagedKey {
                track: track.id.clone(),
                root: root_path.clone(),
                tier: 2,
                preview_only: false,
            };
            let locked = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(source.file.path())
                .unwrap();
            let source_path = source.file.path().to_owned();
            media.retained.lock().unwrap().insert(key(), source);
            let budget = transfer.budget();
            let error = media
                .download_offered_audio(retry_request, offer(), key(), &budget, &cancel, &|_| {})
                .await
                .err()
                .expect("the locked repair source should cause a retryable disk error");
            assert_eq!(error.code, "disk");
            assert!(error.retryable);
            assert_eq!(media.retained.lock().unwrap()[&key()].file.path(), source_path);
            drop(locked);
            let output = media
                .download_offered_audio(retry_request, offer(), key(), &budget, &cancel, &|_| {})
                .await
                .unwrap();
            assert_eq!(read_flac(&output.path).unwrap(), original_info);
            assert!(!source_path.exists());
        }
        #[cfg(not(windows))]
        media.store.discard(source.file).await;
        media.cleanup_stages().await.unwrap();
    }
}
