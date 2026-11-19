mod assets;
mod audio;
mod codec;
mod metadata;
mod output_store;
mod playlist;
mod template;
mod thumbnail;

pub use assets::{ArtworkCache, ArtworkResolution, BookletSource, resolve_booklets, validate_cover_filename};
pub use audio::AudioRequest;
pub use metadata::MetadataField;
pub use output_store::{AudioReplacement, RECEIPT_SCHEMA, migrate_replacement_receipts};
pub use template::{plan_output, preview_output};

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex as AsyncMutex, MutexGuard, OwnedMutexGuard, Semaphore},
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::qobuz::{QobuzError, ResolvedTrack};
use crate::recovery::TransferContext;
use output_store::{CheckCancelled, OutputStore, Stage};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaError {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
    pub path: Option<PathBuf>,
}

impl MediaError {
    fn new(code: &'static str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            path: None,
        }
    }

    pub(crate) fn cancelled() -> Self {
        Self::new("cancelled", "Work was cancelled.", true)
    }

    fn disk(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::StorageFull
            || cfg!(windows) && matches!(error.raw_os_error(), Some(39 | 112))
        {
            return Self::new(
                "disk_full",
                "The output drive is full. Free disk space and retry.",
                true,
            );
        }
        let message = match error.kind() {
            std::io::ErrorKind::NotFound => {
                "The output location is missing. Choose a working destination and retry."
            }
            std::io::ErrorKind::PermissionDenied => {
                "The output location denied access. Check its permissions and retry."
            }
            _ => "A file could not be written or read in the output location.",
        };
        Self::new("disk", message, true)
    }

    fn at_path(mut self, path: &Path) -> Self {
        if self.path.is_none() {
            self.path = Some(path.to_owned());
        }
        self
    }
}

impl From<QobuzError> for MediaError {
    fn from(error: QobuzError) -> Self {
        Self::new(error.code, error.message, error.retryable)
    }
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MediaError {}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LyricsMode {
    Off,
    Embed,
    #[default]
    External,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OutputOptions {
    pub quality: u8,
    pub folder_template: String,
    pub filename_template: String,
    pub lyrics_mode: LyricsMode,
    pub artwork: ArtworkResolution,
    pub tags: BTreeMap<String, bool>,
    pub limited_to_album_releases: bool,
    pub allow_fallback: bool,
    pub booklet_only: bool,
    pub create_playlist: bool,
    pub smart_discography: bool,
    pub download_cover_art: bool,
    pub embed_cover_art: bool,
    pub cover_filename: String,
    pub safe_mode: bool,
}

impl Default for OutputOptions {
    fn default() -> Self {
        Self {
            quality: 4,
            folder_template: "%album artist%/[(%year%)] %album name% [[Q%target quality%]][[%bit depth%-Bit %sampling rate%kHz]]".into(),
            filename_template: "%track number%. %artist% - %track title%".into(),
            lyrics_mode: LyricsMode::default(),
            artwork: ArtworkResolution::Original,
            tags: BTreeMap::new(),
            limited_to_album_releases: false,
            allow_fallback: true,
            booklet_only: false,
            create_playlist: false,
            smart_discography: true,
            download_cover_art: true,
            embed_cover_art: true,
            cover_filename: "cover".into(),
            safe_mode: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    Mp3,
    Flac,
}

impl AudioFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeliveredQuality {
    pub tier: u8,
    pub format: AudioFormat,
    pub bit_depth: Option<u32>,
    /// Sample rate in kHz.
    pub sample_rate: Option<f64>,
}

impl DeliveredQuality {
    pub fn requested(tier: u8) -> Self {
        let (format, bit_depth, sample_rate) = match tier {
            1 => (AudioFormat::Mp3, None, Some(44.1)),
            2 => (AudioFormat::Flac, Some(16), Some(44.1)),
            3 => (AudioFormat::Flac, Some(24), Some(96.0)),
            _ => (AudioFormat::Flac, Some(24), Some(192.0)),
        };
        Self {
            tier,
            format,
            bit_depth,
            sample_rate,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReleaseLayout {
    pub album_root: PathBuf,
    pub disc_dirs: BTreeMap<u32, PathBuf>,
    pub cover_dir: Option<PathBuf>,
}

impl ReleaseLayout {
    pub fn disc_dir(&self, disc: u32) -> &Path {
        self.disc_dirs
            .get(&disc)
            .map_or(self.album_root.as_path(), PathBuf::as_path)
    }
}

#[derive(Clone, Copy)]
pub struct TransferProgress {
    pub attempt: u64,
    pub phase: TransferPhase,
    pub received: u64,
    pub total: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferPhase {
    Connecting,
    Transferring,
    Processing,
}

pub struct Media {
    client: Client,
    store: OutputStore,
    track_locks: [AsyncMutex<()>; 32],
    path_locks: Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    processing: Semaphore,
    cover_failures: Mutex<HashMap<assets::CoverKey, MediaError>>,
    retained: Mutex<HashMap<audio::StagedKey, audio::StagedAudio>>,
    fields: Mutex<BTreeMap<String, MetadataField>>,
    thumbnails: thumbnail::ThumbnailCache,
    next_transfer_attempt: AtomicU64,
}

impl Media {
    pub fn new(client: Client, db_path: PathBuf, portable_root: PathBuf) -> Self {
        Self {
            client,
            store: OutputStore::new(db_path, portable_root),
            track_locks: std::array::from_fn(|_| AsyncMutex::new(())),
            path_locks: Mutex::new(HashMap::new()),
            processing: Semaphore::new(4),
            cover_failures: Mutex::new(HashMap::new()),
            retained: Mutex::new(HashMap::new()),
            fields: Mutex::new(BTreeMap::new()),
            thumbnails: thumbnail::ThumbnailCache::default(),
            next_transfer_attempt: AtomicU64::new(1),
        }
    }

    pub fn observe_track_metadata(&self, track: &ResolvedTrack) {
        let mut observed = self.fields.lock().unwrap();
        for field in metadata::catalog_track(track) {
            if observed.len() >= 512 && !observed.contains_key(&field.id) {
                continue;
            }
            let previous = observed
                .entry(field.id.clone())
                .or_insert_with(|| field.clone());
            previous.available = previous
                .available
                .zip(field.available)
                .map(|(old, new)| old || new)
                .or(field.available);
        }
    }

    pub fn metadata_catalog(&self) -> Vec<MetadataField> {
        let observed = self.fields.lock().unwrap();
        let mut fields = metadata::metadata_catalog();
        for field in &mut fields {
            if let Some(seen) = observed.get(&field.id) {
                field.available = seen.available;
            }
        }
        fields.extend(
            observed
                .values()
                .filter(|field| field.id.contains('/'))
                .cloned(),
        );
        fields
    }

    pub async fn cleanup_stages(&self) -> Result<(), MediaError> {
        let retained = std::mem::take(&mut *self.retained.lock().unwrap());
        for stage in retained.into_values() {
            self.store.discard(stage.file).await;
        }
        self.store.cleanup_stages().await
    }

    pub async fn discard_retained_jobs(&self, removed: &[u64]) {
        let owners = removed
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        let removed = {
            let mut retained = self.retained.lock().unwrap();
            retained
                .extract_if(|_, source| owners.contains(&source.owner))
                .map(|(_, source)| source)
                .collect::<Vec<_>>()
        };
        for source in removed {
            self.store.discard(source.file).await;
        }
    }

    async fn lock_path(
        &self,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<OwnedMutexGuard<()>, MediaError> {
        let key = path.to_string_lossy();
        let key = if cfg!(windows) {
            key.to_lowercase()
        } else {
            key.into_owned()
        };
        let mutex = {
            let mut paths = self.path_locks.lock().unwrap();
            if paths.len() >= 256 {
                paths.retain(|_, lock| lock.strong_count() > 0);
            }
            match paths.get(&key).and_then(Weak::upgrade) {
                Some(mutex) => mutex,
                None => {
                    let mutex = Arc::new(AsyncMutex::new(()));
                    paths.insert(key, Arc::downgrade(&mutex));
                    mutex
                }
            }
        };
        cancellable(cancel, async { Ok(mutex.lock_owned().await) }).await
    }

    async fn stream(
        &self,
        url: &Url,
        stage: &Stage,
        limit: u64,
        transfer: &TransferContext,
        cancel: &CancellationToken,
        progress: &(impl Fn(TransferProgress) + Send + Sync),
    ) -> Result<(), MediaError> {
        let mut restarted = false;
        loop {
            let attempt = self.next_transfer_attempt.fetch_add(1, Ordering::Relaxed);
            let result = self
                .stream_once(url, stage, limit, transfer, cancel, progress, attempt)
                .await;
            let Err(error) = result else {
                return Ok(());
            };
            if cancel.is_cancelled() {
                return Err(MediaError::cancelled());
            }
            // The current provider paths have no verified range contract.
            // A new attempt invalidates telemetry from the discarded payload.
            progress(TransferProgress {
                attempt: self.next_transfer_attempt.fetch_add(1, Ordering::Relaxed),
                phase: TransferPhase::Connecting,
                received: 0,
                total: None,
            });
            if error.code != "media_paused" {
                if restarted && error.code == "media_access" {
                    return Err(MediaError::new(
                        "media_pause_expired",
                        "The media URL needs refreshing after pause.",
                        true,
                    ));
                }
                return Err(error);
            }
            tokio::fs::OpenOptions::new()
                .truncate(true)
                .write(true)
                .open(stage.path())
                .await
                .map_err(MediaError::disk)?;
            if !transfer.wait_for_media(cancel).await {
                return Err(MediaError::cancelled());
            }
            restarted = true;
        }
    }

    async fn stream_once(
        &self,
        url: &Url,
        stage: &Stage,
        limit: u64,
        transfer: &TransferContext,
        cancel: &CancellationToken,
        progress: &(impl Fn(TransferProgress) + Send + Sync),
        attempt: u64,
    ) -> Result<(), MediaError> {
        let network = || {
            MediaError::new(
                "media_network",
                "The media transfer was interrupted. Check your connection and retry.",
                true,
            )
        };
        let invalid = || {
            MediaError::new(
                "media_response",
                "The media server returned an incomplete or unexpected response.",
                true,
            )
        };
        if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
            return Err(MediaError::new(
                "media_url",
                "The media server returned an unsupported asset URL.",
                false,
            ));
        }
        progress(TransferProgress {
            attempt,
            phase: TransferPhase::Connecting,
            received: 0,
            total: None,
        });
        let mut current_url = url.clone();
        let mut redirects = 0;
        let mut throttled = false;
        let (mut response, permit, response_host) = loop {
            let host = current_url.host_str().ok_or_else(invalid)?;
            self.wait_for_drive(stage.path(), transfer, cancel).await?;
            let permit = transfer
                .acquire(host, cancel)
                .await
                .ok_or_else(MediaError::cancelled)?;
            let response = read_media(cancel, &permit.pause, async {
                tokio::time::timeout(
                    Duration::from_secs(30),
                    self.client
                        .get(current_url.clone())
                        .header(reqwest::header::ACCEPT_ENCODING, "identity")
                        .send(),
                )
                .await
                .map_err(|_| network())?
                .map_err(|_| network())
            })
            .await?;
            if response.status().is_redirection() {
                redirects += 1;
                if redirects > 10 {
                    return Err(invalid());
                }
                let target = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(invalid)?;
                let target = response.url().join(target).map_err(|_| invalid())?;
                if target.scheme() != "https"
                    || target.host_str().is_none()
                    || !target.username().is_empty()
                    || target.password().is_some()
                {
                    return Err(invalid());
                }
                current_url = target;
                continue;
            }
            let response_host = response.url().host_str().ok_or_else(invalid)?.to_owned();
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok());
                permit.rate_limited_host(&response_host, retry_after);
                current_url = response.url().clone();
                throttled = true;
                continue;
            }
            break (response, permit, response_host);
        };
        match response.status() {
            reqwest::StatusCode::OK => {}
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE => {
                return Err(MediaError::new(
                    "media_unavailable",
                    "The requested media was unavailable at the server.",
                    true,
                ));
            }
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
                return Err(MediaError::new(
                    if throttled {
                        "media_expired"
                    } else {
                        "media_access"
                    },
                    "The media server rejected access to this asset.",
                    true,
                ));
            }
            status if status.is_server_error() => return Err(network()),
            _ => return Err(invalid()),
        }
        let total = response.content_length();
        if total.is_some_and(|size| size == 0 || size > limit) {
            return Err(invalid());
        }
        let mut file = tokio::fs::OpenOptions::new()
            .truncate(true)
            .write(true)
            .open(stage.path())
            .await
            .map_err(MediaError::disk)?;
        let mut received = 0;
        progress(TransferProgress {
            attempt,
            phase: TransferPhase::Transferring,
            received,
            total,
        });
        loop {
            let chunk = read_media(cancel, &permit.pause, async {
                tokio::time::timeout(Duration::from_secs(30), response.chunk())
                    .await
                    .map_err(|_| network())?
                    .map_err(|_| network())
            })
            .await?;
            let Some(chunk) = chunk else {
                break;
            };
            received += chunk.len() as u64;
            if !chunk.is_empty() {
                transfer.received(chunk.len() as u64);
            }
            if received > limit || total.is_some_and(|size| received > size) {
                return Err(invalid());
            }
            file.write_all(&chunk).await.map_err(MediaError::disk)?;
            progress(TransferProgress {
                attempt,
                phase: TransferPhase::Transferring,
                received,
                total,
            });
        }
        if received == 0 || total.is_some_and(|size| received != size) {
            return Err(invalid());
        }
        cancel.check()?;
        permit.success_host(&response_host);
        drop(response);
        drop(permit);
        file.flush().await.map_err(MediaError::disk)?;
        progress(TransferProgress {
            attempt,
            phase: TransferPhase::Processing,
            received,
            total,
        });
        Ok(())
    }

    pub(crate) async fn wait_for_drive(
        &self,
        path: &Path,
        transfer: &TransferContext,
        cancel: &CancellationToken,
    ) -> Result<(), MediaError> {
        if transfer.wait_for_drive(path, cancel).await {
            return Ok(());
        }
        cancel.check()?;
        Err(MediaError::new(
            "disk_full",
            "The output drive is paused. Free disk space and retry.",
            true,
        )
        .at_path(path))
    }
}

async fn read_media<T>(
    cancel: &CancellationToken,
    pause: &CancellationToken,
    read: impl std::future::Future<Output = Result<T, MediaError>>,
) -> Result<T, MediaError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(MediaError::cancelled()),
        _ = pause.cancelled() => Err(MediaError::new("media_paused", "Media transfer paused.", true)),
        result = read => result,
    }
}

#[cfg(test)]
#[tokio::test]
async fn paused_response_read_yields_its_permit_and_does_not_consume_a_ready_chunk() {
    use futures_util::poll;
    use std::pin::pin;
    let recovery = Arc::new(crate::recovery::Recovery::default());
    let transfer = recovery.transfer(1, 0, 1, |_| {});
    let cancel = CancellationToken::new();
    let permit = transfer.acquire("example.test", &cancel).await.unwrap();
    let (send, recv) = tokio::sync::oneshot::channel::<&str>();
    let mut read = pin!(async {
        let permit = permit;
        read_media(&cancel, &permit.pause, async { Ok(recv.await.unwrap()) }).await
    });
    assert!(poll!(&mut read).is_pending());
    send.send("payload").unwrap();
    recovery.set_media_paused(true);
    assert_eq!(read.await.unwrap_err().code, "media_paused");
    recovery.set_media_paused(false);
    let mut resumed = pin!(transfer.acquire("example.test", &cancel));
    assert!(matches!(
        poll!(&mut resumed),
        std::task::Poll::Ready(Some(_))
    ));
}

async fn lock<'a, T: std::hash::Hash + ?Sized>(
    locks: &'a [AsyncMutex<()>; 32],
    key: &T,
    cancel: &CancellationToken,
) -> Result<MutexGuard<'a, ()>, MediaError> {
    use std::hash::{BuildHasher, RandomState};
    // Use a stable process-local key mapping so identical paths serialize commits.
    static HASHER: std::sync::OnceLock<RandomState> = std::sync::OnceLock::new();
    let hash = HASHER.get_or_init(RandomState::new).hash_one(key);
    cancellable(cancel, async {
        Ok(locks[hash as usize % locks.len()].lock().await)
    })
    .await
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, MediaError> + Send + 'static,
) -> Result<T, MediaError> {
    tokio::task::spawn_blocking(work).await.map_err(|_| {
        MediaError::new(
            "media_task",
            "The media processing step could not finish.",
            true,
        )
    })?
}

async fn cancellable<T>(
    cancel: &CancellationToken,
    work: impl Future<Output = Result<T, MediaError>>,
) -> Result<T, MediaError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(MediaError::cancelled()),
        result = work => result,
    }
}

async fn existing_root(root: &Path) -> Result<PathBuf, MediaError> {
    if !root.is_absolute() {
        return Err(MediaError::new(
            "output_location",
            "Choose an absolute output location.",
            false,
        ));
    }
    let root = tokio::fs::canonicalize(root)
        .await
        .map_err(MediaError::disk)?;
    if !tokio::fs::metadata(&root)
        .await
        .is_ok_and(|info| info.is_dir())
    {
        return Err(MediaError::new(
            "output_location",
            "The output location must be an existing folder.",
            true,
        ));
    }
    Ok(root)
}

async fn copy_stage(
    source: &Path,
    target: &Path,
    cancel: &CancellationToken,
) -> Result<(), MediaError> {
    let mut reader = tokio::fs::File::open(source)
        .await
        .map_err(MediaError::disk)?;
    let mut writer = tokio::fs::OpenOptions::new()
        .truncate(true)
        .write(true)
        .open(target)
        .await
        .map_err(MediaError::disk)?;
    cancellable(cancel, async {
        tokio::io::copy(&mut reader, &mut writer)
            .await
            .map_err(MediaError::disk)
    })
    .await?;
    writer.flush().await.map_err(MediaError::disk)
}
