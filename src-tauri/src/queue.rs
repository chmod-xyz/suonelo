use crate::{
    app::App,
    error::{AppError, Result},
    media::{
        self, ArtworkCache, AudioReplacement, AudioRequest, DeliveredQuality, LyricsMode,
        ReleaseLayout, TransferPhase, TransferProgress,
    },
    qobuz::{
        CanonicalResource, Credentials, LyricsSource, ResolvedResource, ResolvedTrack, ResourceKind,
    },
    recovery::{
        CONCURRENT_MEDIA_LIMIT, RecoveryDto, RecoveryNotice, SEQUENTIAL_MEDIA_LIMIT,
        TransferContext,
    },
    search::activity::{Credit, event_time},
    storage::{DownloadMode, MaxConnections, Settings},
};
use futures_util::{Stream, StreamExt, stream};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::ipc::Channel;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Downloading,
    Retrying,
    Failed,
    Unavailable,
    Incomplete,
    Completed,
    Cancelled,
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Audio,
    Artwork,
    Booklet,
    Lyrics,
    Playlist,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FilePhase {
    Queued,
    Connecting,
    Transferring,
    Processing,
    Retrying,
    Completed,
    Failed,
    Cancelled,
    Unavailable,
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobMode {
    Audio,
    Booklet,
}

impl JobMode {
    fn from_settings(settings: &Settings) -> Self {
        if settings.output.booklet_only {
            Self::Booklet
        } else {
            Self::Audio
        }
    }

    fn primary_file(self) -> FileKind {
        match self {
            Self::Audio => FileKind::Audio,
            Self::Booklet => FileKind::Booklet,
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDto {
    pub id: u64,
    pub kind: FileKind,
    pub title: String,
    pub album: Option<String>,
    pub disc: Option<u32>,
    pub status: Status,
    pub progress: Option<u8>,
    pub generation: u64,
    pub attempt: u64,
    pub phase: FilePhase,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
    pub final_size_bytes: Option<u64>,
    pub quality: Option<String>,
    pub path: Option<PathBuf>,
    pub conflict_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_token: Option<String>,
    pub reason: Option<String>,
    pub reused: bool,
    pub independent_retry: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobDto {
    pub id: u64,
    pub url: String,
    pub title: String,
    pub subtitle: String,
    pub mode: JobMode,
    pub generation: u64,
    pub progress: Option<u8>,
    pub clearable: bool,
    pub thumbnail: Option<String>,
    pub resource_kind: ResourceKind,
    pub status: Status,
    pub reason: Option<String>,
    pub drive_blocked: bool,
    pub files: Vec<FileDto>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Confirmation {
    pub id: u64,
    pub plan_revision: u64,
    pub track_count: usize,
    pub unit: &'static str,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueSnapshot {
    pub revision: u64,
    pub jobs: Vec<JobDto>,
    pub confirmations: Vec<Confirmation>,
    pub recovery: RecoveryDto,
    pub bytes_per_second: u64,
    pub can_clear: bool,
    pub protected_count: usize,
    pub effective_mode: DownloadMode,
    pub requested_mode: DownloadMode,
    pub live_limit: Option<MaxConnections>,
    pub paused: bool,
    pub can_pause: bool,
}

#[derive(Clone, Serialize)]
#[serde(untagged)]
pub enum QueueEvent {
    Snapshot(QueueSnapshot),
    #[serde(rename_all = "camelCase")]
    Progress {
        revision: u64,
        job_id: u64,
        file_id: u64,
        generation: u64,
        attempt: u64,
        progress: Option<u8>,
        phase: FilePhase,
        received_bytes: u64,
        total_bytes: Option<u64>,
        final_size_bytes: Option<u64>,
        job_progress: Option<u8>,
    },
    #[serde(rename_all = "camelCase")]
    Speed {
        revision: u64,
        bytes_per_second: u64,
    },
}

#[derive(Serialize)]
pub struct ClearReport {
    pub removed: usize,
    pub protected: usize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Waiting,
    Planning,
    Held,
    Eligible,
    Active,
    Finished,
}

#[derive(Clone)]
enum Extra {
    Artwork {
        track: usize,
        directory: PathBuf,
        basename: String,
        final_target: bool,
    },
    Booklet {
        track: usize,
        directory: PathBuf,
        source: media::BookletSource,
    },
    Lyrics {
        track: usize,
        audio: PathBuf,
    },
    Playlist,
}

pub struct Job {
    pub link: CanonicalResource,
    dto: JobDto,
    settings: Settings,
    phase: Phase,
    batch: u64,
    generation: u64,
    cancel: CancellationToken,
    running: bool,
    plan: Option<Arc<ResolvedResource>>,
    pinned: BTreeMap<String, ReleaseLayout>,
    paths: BTreeMap<usize, PathBuf>,
    extras: BTreeMap<u64, Extra>,
    asset_roots: BTreeMap<u64, PathBuf>,
    only_file: Option<u64>,
    retrying: bool,
    drive_retry: BTreeSet<PathBuf>,
    active_drive_blocks: BTreeSet<PathBuf>,
    blocked_destination: Option<PathBuf>,
    playlist_path: Option<PathBuf>,
    playlist_reserved: bool,
    artwork_failures: BTreeMap<(String, PathBuf), media::MediaError>,
    missing_embedded: BTreeSet<usize>,
    quality_override: Option<u8>,
    activity_at: Option<i64>,
    search_coverage: Option<Arc<HashMap<String, String>>>,
    search_origin: bool,
    replacements: HashMap<u64, AudioReplacement>,
    approved_replacement: Option<(u64, AudioReplacement)>,
}

impl Job {
    fn snapshot(&self) -> JobDto {
        let mut dto = self.dto.clone();
        dto.generation = self.generation;
        dto.progress = release_progress(self);
        dto.clearable = self.phase != Phase::Active;
        dto
    }
}

#[derive(Default)]
struct RateWindow {
    samples: VecDeque<(Instant, u64)>,
}

impl RateWindow {
    fn record(&mut self, now: Instant, bytes: u64) {
        self.samples.push_back((now, bytes));
    }

    fn bytes_per_second(&mut self, now: Instant) -> u64 {
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= Duration::from_secs(1))
        {
            self.samples.pop_front();
        }
        self.samples
            .iter()
            .fold(0_u64, |sum, (_, bytes)| sum.saturating_add(*bytes))
    }
}

struct Batch {
    jobs: Vec<u64>,
    confirmation: Option<Confirmation>,
    planning: bool,
}

#[derive(Default)]
pub struct Queue {
    pub jobs: BTreeMap<u64, Job>,
    batches: BTreeMap<u64, Batch>,
    next_id: u64,
    next_batch: u64,
    revision: u64,
    channels: Vec<Channel<QueueEvent>>,
    last_start: Option<Instant>,
    recovery: RecoveryDto,
    rate: RateWindow,
    bytes_per_second: u64,
    effective_mode: DownloadMode,
    requested_mode: DownloadMode,
    saved_limit: MaxConnections,
    admitted: BTreeSet<u64>,
    user_paused: bool,
}

impl Queue {
    pub fn new(settings: &Settings) -> Self {
        Self {
            effective_mode: settings.download_mode,
            requested_mode: settings.download_mode,
            saved_limit: settings.max_connections,
            ..Self::default()
        }
    }

    pub fn settings_saved(
        &mut self,
        settings: &Settings,
        apply_live: bool,
        recovery: &crate::recovery::Recovery,
    ) {
        let policy_changed = self.requested_mode != settings.download_mode
            || self.saved_limit != settings.max_connections;
        self.requested_mode = settings.download_mode;
        self.saved_limit = settings.max_connections;
        if self.admitted.is_empty() {
            self.effective_mode = self.requested_mode;
        }
        if apply_live {
            for job in self
                .jobs
                .values_mut()
                .filter(|job| job.phase != Phase::Finished)
            {
                job.settings.max_connections = self.saved_limit;
            }
            recovery.update_media_limits(self.saved_limit.limit());
        }
        if policy_changed || apply_live {
            self.changed();
        }
    }

    fn admission_head(&self) -> Option<u64> {
        self.jobs
            .values()
            .find(|job| job.phase != Phase::Finished)
            .map(|job| job.dto.id)
    }

    pub fn set_user_pause(&mut self, paused: bool, recovery: &crate::recovery::Recovery) {
        let next = paused && self.admission_head().is_some();
        if next != self.user_paused {
            self.user_paused = next;
            recovery.set_media_paused(next);
            self.changed();
        }
    }

    pub fn reset_idle_pause(&mut self, recovery: &crate::recovery::Recovery) {
        self.set_user_pause(self.user_paused, recovery);
    }

    fn finish_admission(&mut self, id: u64, recovery: &crate::recovery::Recovery) {
        recovery.finish_media(id);
        self.admitted.remove(&id);
        if self.admitted.is_empty() {
            self.effective_mode = self.requested_mode;
        }
    }

    fn live_limit(&self) -> Option<MaxConnections> {
        let mut queued = self
            .jobs
            .values()
            .filter(|job| job.phase != Phase::Finished);
        if self.effective_mode == DownloadMode::Sequential {
            let head = self
                .admitted
                .first()
                .and_then(|id| self.jobs.get(id))
                .or_else(|| queued.next());
            return Some(head.map_or(self.saved_limit, |job| job.settings.max_connections));
        }
        let limit = queued
            .next()
            .map_or(self.saved_limit, |job| job.settings.max_connections);
        queued
            .all(|job| job.settings.max_connections == limit)
            .then_some(limit)
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            revision: self.revision,
            jobs: self.jobs.values().map(Job::snapshot).collect(),
            confirmations: self
                .batches
                .values()
                .filter_map(|batch| batch.confirmation.clone())
                .collect(),
            recovery: self.recovery,
            bytes_per_second: self.bytes_per_second,
            can_clear: self.jobs.values().any(|job| job.phase != Phase::Active),
            protected_count: self
                .jobs
                .values()
                .filter(|job| job.phase == Phase::Active)
                .count(),
            effective_mode: self.effective_mode,
            requested_mode: self.requested_mode,
            live_limit: self.live_limit(),
            paused: self.user_paused,
            can_pause: self.admission_head().is_some(),
        }
    }

    pub fn subscribe(&mut self, channel: Channel<QueueEvent>) {
        if channel.send(QueueEvent::Snapshot(self.snapshot())).is_ok() {
            self.channels.push(channel);
        }
    }

    pub fn changed(&mut self) {
        self.revision += 1;
        if !self.channels.is_empty() {
            let snapshot = QueueEvent::Snapshot(self.snapshot());
            self.channels
                .retain(|channel| channel.send(snapshot.clone()).is_ok());
        }
    }

    fn progress(&mut self, job_id: u64, generation: u64, file_id: u64, progress: TransferProgress) {
        let Some(job) = self
            .jobs
            .get_mut(&job_id)
            .filter(|job| job.generation == generation && !job.cancel.is_cancelled())
        else {
            return;
        };
        let Some(file) = job.dto.files.iter_mut().find(|file| file.id == file_id) else {
            return;
        };
        if !matches!(file.status, Status::Downloading | Status::Retrying)
            || progress.attempt < file.attempt
        {
            return;
        }
        let phase = match progress.phase {
            TransferPhase::Connecting => FilePhase::Connecting,
            TransferPhase::Transferring => FilePhase::Transferring,
            TransferPhase::Processing => FilePhase::Processing,
        };
        let amount = progress.total.map(|total| {
            if total == 0 {
                0
            } else {
                (progress.received.saturating_mul(100) / total).min(99) as u8
            }
        });
        if file.attempt == progress.attempt
            && file.phase == phase
            && file.received_bytes == progress.received
            && file.total_bytes == progress.total
            && file.progress == amount
        {
            return;
        }
        if file.attempt == progress.attempt && progress.received < file.received_bytes {
            return;
        }
        file.status = Status::Downloading;
        file.phase = phase;
        file.progress = amount;
        file.generation = generation;
        file.attempt = progress.attempt;
        file.received_bytes = progress.received;
        file.total_bytes = progress.total;
        file.final_size_bytes = None;
        let file = file.clone();
        let job_progress = release_progress(job);
        self.revision += 1;
        let event = QueueEvent::Progress {
            revision: self.revision,
            job_id,
            file_id,
            generation,
            attempt: file.attempt,
            progress: amount,
            phase,
            received_bytes: file.received_bytes,
            total_bytes: file.total_bytes,
            final_size_bytes: file.final_size_bytes,
            job_progress,
        };
        self.channels
            .retain(|channel| channel.send(event.clone()).is_ok());
    }

    fn refresh_rate(&mut self, now: Instant) {
        let bytes_per_second = self.rate.bytes_per_second(now);
        if self.bytes_per_second == bytes_per_second {
            return;
        }
        self.bytes_per_second = bytes_per_second;
        self.revision += 1;
        let event = QueueEvent::Speed {
            revision: self.revision,
            bytes_per_second,
        };
        self.channels
            .retain(|channel| channel.send(event.clone()).is_ok());
    }

    pub fn enqueue(&mut self, links: Vec<CanonicalResource>, settings: &Settings, reason: &str) {
        self.enqueue_with_quality(links, settings, reason, None);
    }

    pub fn enqueue_with_quality(
        &mut self,
        links: Vec<CanonicalResource>,
        settings: &Settings,
        reason: &str,
        quality_override: Option<u8>,
    ) {
        self.insert(links, settings, reason, quality_override);
        self.changed();
    }

    pub fn enqueue_search(
        &mut self,
        links: Vec<CanonicalResource>,
        settings: &Settings,
        reason: &str,
        releases: HashSet<String>,
        track_parents: HashMap<String, String>,
    ) {
        let last = self.next_id;
        self.insert(links, settings, reason, None);
        for (_, job) in self.jobs.range_mut((std::ops::Bound::Excluded(last), std::ops::Bound::Unbounded)) {
            job.search_origin = true;
        }
        let parents = Arc::new(track_parents);
        for job in self.jobs.values_mut() {
            if job.link.key.kind == ResourceKind::Album && releases.contains(&job.link.key.id)
                && job.settings.output == settings.output && job.settings.output_location == settings.output_location
            {
                job.search_coverage = Some(parents.clone());
            }
        }
        self.apply_search_coverage();
        self.changed();
    }

    fn insert(
        &mut self,
        links: Vec<CanonicalResource>,
        settings: &Settings,
        reason: &str,
        quality_override: Option<u8>,
    ) {
        if links.is_empty() {
            return;
        }
        let mut settings = settings.clone();
        if let Some(quality) = quality_override {
            settings.output.quality = quality;
        }
        self.next_batch += 1;
        let batch = self.next_batch;
        let ids = links
            .into_iter()
            .map(|link| {
                self.next_id += 1;
                let id = self.next_id;
                self.jobs.insert(
                    id,
                    Job {
                        dto: JobDto {
                            id,
                            url: link.original_url.clone(),
                            title: link.original_url.clone(),
                            subtitle: String::new(),
                            mode: JobMode::from_settings(&settings),
                            generation: 0,
                            progress: Some(0),
                            clearable: true,
                            thumbnail: None,
                            resource_kind: link.key.kind,
                            status: Status::Queued,
                            reason: Some(reason.into()),
                            drive_blocked: false,
                            files: Vec::new(),
                        },
                        link,
                        settings: settings.clone(),
                        phase: Phase::Waiting,
                        batch,
                        generation: 0,
                        cancel: CancellationToken::new(),
                        running: false,
                        plan: None,
                        pinned: BTreeMap::new(),
                        paths: BTreeMap::new(),
                        extras: BTreeMap::new(),
                        asset_roots: BTreeMap::new(),
                        only_file: None,
                        retrying: false,
                        drive_retry: BTreeSet::new(),
                        active_drive_blocks: BTreeSet::new(),
                        blocked_destination: None,
                        playlist_path: None,
                        playlist_reserved: false,
                        artwork_failures: BTreeMap::new(),
                        missing_embedded: BTreeSet::new(),
                        quality_override,
                        activity_at: Some(event_time()),
                        search_coverage: None,
                        search_origin: false,
                        replacements: HashMap::new(),
                        approved_replacement: None,
                    },
                );
                id
            })
            .collect();
        self.batches.insert(
            batch,
            Batch {
                jobs: ids,
                confirmation: None,
                planning: false,
            },
        );
    }

    fn apply_search_coverage(&mut self) -> bool {
        let mut removed = BTreeSet::new();
        for parent in self.jobs.values() {
            let Some(parents) = &parent.search_coverage else { continue; };
            if !matches!(parent.phase, Phase::Eligible | Phase::Active)
                || parent.only_file.is_some() || parent.dto.mode != JobMode::Audio
            {
                continue;
            }
            let Some(plan) = &parent.plan else { continue; };
            for (id, child) in &self.jobs {
                if child.link.key.kind != ResourceKind::Track
                    || !matches!(child.phase, Phase::Waiting | Phase::Planning | Phase::Held | Phase::Eligible)
                    || child.dto.files.iter().any(|file| file.status == Status::Completed)
                    || child.settings.output_location != parent.settings.output_location
                    || child.settings.output != parent.settings.output
                    || !child.pinned.is_empty()
                {
                    continue;
                }
                let track_id = &child.link.key.id;
                if parents.get(track_id).is_some_and(|album| album != &parent.link.key.id) { continue; }
                if plan.tracks.iter().any(|track| &track.id == track_id && track.unavailable.is_none()) {
                    removed.insert(*id);
                }
            }
        }
        let changed = !removed.is_empty();
        let mut batches = BTreeSet::new();
        for id in removed {
            if let Some(job) = self.jobs.remove(&id) {
                job.cancel.cancel();
                batches.insert(job.batch);
            }
        }
        for batch in batches { self.reconsider(batch); }
        changed
    }

    pub fn retry(
        &mut self,
        id: u64,
        settings: &Settings,
        reason: &str,
        selected_file: Option<u64>,
    ) -> Result<()> {
        let job = self.jobs.get_mut(&id).ok_or_else(missing_job)?;
        if selected_file.is_none()
            && matches!(job.phase, Phase::Active | Phase::Eligible)
            && job.dto.drive_blocked
        {
            job.drive_retry
                .extend(job.blocked_destination.iter().cloned());
            job.drive_retry
                .extend(job.active_drive_blocks.iter().cloned());
            return Ok(());
        }
        if job.running || job.phase != Phase::Finished {
            return Err(AppError::invalid(
                "queue",
                "Wait for the current attempt to finish before retrying.",
            ));
        }
        if let Some(file) = selected_file {
            if !job.dto.files.iter().any(|item| {
                item.id == file && item.independent_retry && item.status != Status::Completed
            }) {
                return Err(AppError::invalid(
                    "queue",
                    "This item has no independent retry action.",
                ));
            }
        } else if job.dto.status == Status::Completed {
            return Err(AppError::invalid(
                "queue",
                "Use Download again to start a fresh attempt.",
            ));
        }
        self.next_batch += 1;
        job.batch = self.next_batch;
        job.generation += 1;
        job.phase = Phase::Waiting;
        job.settings = compatible_settings(job, settings);
        job.cancel = CancellationToken::new();
        job.only_file = selected_file;
        job.retrying = true;
        job.replacements.clear();
        job.approved_replacement = None;
        job.drive_retry
            .extend(job.blocked_destination.iter().cloned());
        job.artwork_failures.clear();
        job.dto.status = Status::Queued;
        job.dto.reason = Some(reason.into());
        for file in &mut job.dto.files {
            file.replacement_token = None;
            let selected = selected_file.map_or(
                !matches!(file.status, Status::Completed | Status::Unavailable),
                |id| id == file.id,
            );
            if selected {
                file.status = Status::Queued;
                file.phase = FilePhase::Queued;
                file.progress = Some(0);
                file.generation = job.generation;
                file.attempt = 0;
                file.received_bytes = 0;
                file.total_bytes = None;
                file.final_size_bytes = None;
                file.reason = None;
                file.conflict_path = None;
            }
        }
        self.batches.insert(
            job.batch,
            Batch {
                jobs: vec![id],
                confirmation: None,
                planning: false,
            },
        );
        self.changed();
        Ok(())
    }

    pub fn approve_replacement(
        &mut self, job_id: u64, file_id: u64, token: &str, settings: &Settings, reason: &str,
    ) -> Result<()> {
        let job = self.jobs.get(&job_id).ok_or_else(missing_job)?;
        let approval = job.replacements.get(&file_id).filter(|approval| approval.token == token
            && job.search_origin && job.settings.output == settings.output
            && job.settings.output_location == settings.output_location)
            .ok_or_else(|| AppError::invalid("queue", "The replacement offer is stale. Retry the track to verify the current file."))?
            .clone();
        self.retry(job_id, settings, reason, Some(file_id))?;
        self.jobs.get_mut(&job_id).expect("retried job").approved_replacement = Some((file_id, approval));
        Ok(())
    }

    pub fn release_folder_evidence(&self, job_id: u64) -> Result<Vec<(PathBuf, PathBuf)>> {
        // Lexical only: filesystem canonicalization remains outside the queue lock.
        fn recorded_contains(path: &std::path::Path, root: &std::path::Path) -> bool {
            #[cfg(windows)]
            {
                use std::path::{Component, Prefix};
                let mut path = path.components();
                let mut root = root.components();
                let same_prefix = match (path.next(), root.next()) {
                    (Some(Component::Prefix(a)), Some(Component::Prefix(b))) => {
                        match (a.kind(), b.kind()) {
                            (
                                Prefix::Disk(a) | Prefix::VerbatimDisk(a),
                                Prefix::Disk(b) | Prefix::VerbatimDisk(b),
                            ) => a.eq_ignore_ascii_case(&b),
                            (
                                Prefix::UNC(a, s) | Prefix::VerbatimUNC(a, s),
                                Prefix::UNC(b, t) | Prefix::VerbatimUNC(b, t),
                            ) => {
                                a.as_encoded_bytes()
                                    .eq_ignore_ascii_case(b.as_encoded_bytes())
                                    && s.as_encoded_bytes()
                                        .eq_ignore_ascii_case(t.as_encoded_bytes())
                            }
                            (a, b) => a == b,
                        }
                    }
                    (a, b) => a == b,
                };
                same_prefix && root.all(|part| path.next() == Some(part))
            }
            #[cfg(not(windows))]
            {
                path.starts_with(root)
            }
        }

        let job = self.jobs.get(&job_id).ok_or_else(missing_job)?;
        if job.phase != Phase::Finished
            || job.running
            || !matches!(job.dto.status, Status::Completed | Status::Incomplete)
        {
            return Err(AppError::invalid(
                "queue",
                "This release is not completed or incomplete.",
            ));
        }
        job.dto
            .files
            .iter()
            .filter(|file| file.status == Status::Completed)
            .filter(|file| match job.extras.get(&file.id) {
                Some(Extra::Artwork {
                    track, directory, ..
                }) => job
                    .plan
                    .as_ref()
                    .and_then(|plan| job.pinned.get(&plan.tracks[*track].album_id))
                    .is_none_or(|layout| {
                        *directory == layout.album_root
                            || layout.disc_dirs.values().any(|disc| disc == directory)
                    }),
                _ => true,
            })
            .map(|file| {
                let path = file.path.clone().ok_or_else(|| {
                    AppError::invalid("queue", "A saved output has no recorded location.")
                })?;
                let root = job
                    .asset_roots
                    .get(&file.id)
                    .cloned()
                    .or_else(|| {
                        let roots = job
                            .pinned
                            .values()
                            .filter(|layout| recorded_contains(&path, &layout.album_root))
                            .collect::<Vec<_>>();
                        (roots.len() == 1).then(|| roots[0].album_root.clone())
                    })
                    .ok_or_else(|| {
                        AppError::invalid(
                            "queue",
                            "No unambiguous saved release folder is recorded.",
                        )
                    })?;
                Ok((path, root))
            })
            .collect()
    }

    pub fn conflict_path(&self, job_id: u64, file_id: u64) -> Result<PathBuf> {
        self.jobs
            .get(&job_id)
            .and_then(|job| job.dto.files.iter().find(|file| file.id == file_id))
            .and_then(|file| file.conflict_path.clone())
            .ok_or_else(|| AppError::invalid("queue", "This file has no recorded conflict path."))
    }

    pub fn download_again(&mut self, id: u64, settings: &Settings, reason: &str) -> Result<()> {
        let job = self.jobs.get(&id).ok_or_else(missing_job)?;
        if job.phase != Phase::Finished || job.running {
            return Err(AppError::invalid("queue", "This job has not finished."));
        }
        self.enqueue(vec![job.link.clone()], settings, reason);
        Ok(())
    }

    pub fn cancel(&mut self, id: u64, remove: bool) -> Result<()> {
        let job = self.jobs.get_mut(&id).ok_or_else(missing_job)?;
        if !remove && job.phase == Phase::Finished {
            return Err(AppError::invalid("queue", "This job has already finished."));
        }
        let batch = job.batch;
        cancel(job);
        if remove {
            self.jobs.remove(&id);
        }
        self.reconsider(batch);
        self.changed();
        Ok(())
    }

    pub fn clear_downloads(&mut self) -> ClearReport {
        let before = self.jobs.len();
        let mut affected = BTreeSet::new();
        self.jobs.retain(|_, job| {
            if job.phase == Phase::Active {
                return true;
            }
            job.cancel.cancel();
            affected.insert(job.batch);
            false
        });
        self.batches.retain(|id, batch| {
            if affected.contains(id) {
                batch.confirmation = None;
            }
            batch
                .jobs
                .retain(|job_id| self.jobs.get(job_id).is_some_and(|job| job.batch == *id));
            !batch.jobs.is_empty()
        });
        for id in affected {
            self.reconsider(id);
        }
        let report = ClearReport {
            removed: before - self.jobs.len(),
            protected: self.jobs.len(),
        };
        if report.removed > 0 {
            self.changed();
        }
        report
    }

    pub fn pending(&self) -> bool {
        !self.admitted.is_empty()
            || self
                .jobs
                .values()
                .any(|job| job.phase != Phase::Finished || job.running)
    }

    pub fn active(&self) -> bool {
        !self.admitted.is_empty()
            || self
                .jobs
                .values()
                .any(|job| matches!(job.phase, Phase::Planning | Phase::Active))
    }

    pub fn account_changed(&mut self, reason: &str, cancel_active: bool) {
        for job in self.jobs.values_mut() {
            if cancel_active {
                job.approved_replacement = None;
                job.replacements.clear();
                for file in &mut job.dto.files { file.replacement_token = None; }
            }
            match job.phase {
                Phase::Active if cancel_active => cancel(job),
                Phase::Waiting | Phase::Held | Phase::Eligible | Phase::Planning => {
                    if job.phase == Phase::Planning {
                        job.cancel.cancel();
                        job.cancel = CancellationToken::new();
                        job.generation += 1;
                        job.running = false;
                    }
                    job.phase = Phase::Waiting;
                    job.dto.status = Status::Queued;
                    job.dto.reason = Some(reason.into());
                    if !job.retrying {
                        job.plan = None;
                        job.dto.files.clear();
                        job.extras.clear();
                    }
                }
                _ => (),
            }
        }
        for batch in self.batches.values_mut() {
            batch.confirmation = None;
        }
        self.changed();
    }

    pub fn apply(&mut self, ids: Option<Vec<u64>>, settings: &Settings) {
        let mut changed_batches = BTreeSet::new();
        for (id, job) in &mut self.jobs {
            if !matches!(job.phase, Phase::Active | Phase::Finished)
                && ids.as_ref().is_none_or(|ids| ids.contains(id))
            {
                let mut next = if job.retrying {
                    compatible_settings(job, settings)
                } else {
                    settings.clone()
                };
                next.output.quality = settings.output.quality;
                job.quality_override = None;
                if !job.retrying
                    && (JobMode::from_settings(&next) != job.dto.mode
                        || next.output.limited_to_album_releases
                            != job.settings.output.limited_to_album_releases
                        || next.output.smart_discography != job.settings.output.smart_discography)
                {
                    changed_batches.insert(job.batch);
                    job.cancel.cancel();
                    job.cancel = CancellationToken::new();
                    job.generation += 1;
                    job.running = false;
                    job.plan = None;
                    job.extras.clear();
                    job.dto.files.clear();
                    job.dto.mode = JobMode::from_settings(&next);
                    job.phase = Phase::Waiting;
                    job.dto.status = Status::Queued;
                    job.dto.reason = Some("Waiting to refresh the submission plan.".into());
                }
                job.settings = next;
            }
        }
        for batch_id in changed_batches {
            if let Some(batch) = self.batches.get_mut(&batch_id) {
                batch.confirmation = None;
                for id in &batch.jobs {
                    if let Some(job) = self
                        .jobs
                        .get_mut(id)
                        .filter(|job| job.batch == batch_id && job.phase == Phase::Eligible)
                    {
                        job.phase = Phase::Held;
                        job.dto.reason = Some("Waiting for the submission plan.".into());
                    }
                }
            }
        }
        self.changed();
    }

    pub fn decide(&mut self, id: u64, plan_revision: u64, decision: bool) -> Result<()> {
        let batch = self.batches.get_mut(&id).ok_or_else(stale_plan)?;
        if !batch
            .confirmation
            .as_ref()
            .is_some_and(|plan| plan.plan_revision == plan_revision)
        {
            return Err(stale_plan());
        }
        batch.confirmation = None;
        for job_id in &batch.jobs {
            let Some(job) = self.jobs.get_mut(job_id).filter(|job| job.batch == id) else {
                continue;
            };
            if decision {
                if job.phase == Phase::Held {
                    job.phase = Phase::Eligible;
                    job.dto.reason = Some("Waiting to start.".into());
                }
            } else {
                cancel(job);
            }
        }
        self.changed();
        Ok(())
    }

    pub fn shutdown(&mut self) {
        for job in self.jobs.values_mut() {
            job.cancel.cancel();
        }
        self.jobs.clear();
        self.batches.clear();
        self.changed();
    }

    fn reconsider(&mut self, batch_id: u64) {
        let Some(batch) = self.batches.get_mut(&batch_id) else {
            return;
        };
        if batch.planning {
            return;
        }
        let jobs = &batch.jobs;
        let held = || {
            jobs.iter()
                .filter_map(|id| self.jobs.get(id))
                .filter(|job| job.batch == batch_id && job.phase == Phase::Held)
        };
        if jobs.iter().filter_map(|id| self.jobs.get(id)).any(|job| {
            job.batch == batch_id && matches!(job.phase, Phase::Waiting | Phase::Planning)
        }) {
            return;
        }
        let tracks = held()
            .filter(|job| job.dto.mode == JobMode::Audio)
            .map(|job| {
                job.dto
                    .files
                    .iter()
                    .filter(|file| {
                        file.kind == FileKind::Audio
                            && file.status != Status::Completed
                            && job.only_file.is_none_or(|id| id == file.id)
                    })
                    .count()
            })
            .sum::<usize>();
        let mut pdfs = BTreeSet::new();
        for job in held().filter(|job| job.dto.mode == JobMode::Booklet) {
            if let Some(plan) = &job.plan {
                for (id, extra) in &job.extras {
                    if let Extra::Booklet { track, source, .. } = extra
                        && job.only_file.is_none_or(|selected| *id == selected)
                        && job
                            .dto
                            .files
                            .iter()
                            .any(|file| file.id == *id && file.status != Status::Completed)
                    {
                        pdfs.insert((plan.tracks[*track].album_id.as_str(), source.id.as_str()));
                    }
                }
            }
        }
        let (count, unit) = if tracks >= 100 || pdfs.len() < 100 {
            (tracks, "tracks")
        } else {
            (pdfs.len(), "pdfs")
        };
        batch.confirmation = if count >= 100 {
            Some(Confirmation {
                id: batch_id,
                plan_revision: self.revision + 1,
                track_count: count,
                unit,
            })
        } else {
            for id in jobs {
                if let Some(job) = self
                    .jobs
                    .get_mut(id)
                    .filter(|j| j.batch == batch_id && j.phase == Phase::Held)
                {
                    job.phase = Phase::Eligible;
                    job.dto.reason = Some("Waiting to start.".into());
                }
            }
            None
        };
    }
}

fn cancel(job: &mut Job) {
    job.cancel.cancel();
    job.phase = Phase::Finished;
    job.dto.status = Status::Cancelled;
    job.dto.reason = Some("Cancelled.".into());
    for file in &mut job.dto.files {
        if matches!(
            file.status,
            Status::Downloading | Status::Queued | Status::Retrying
        ) {
            file.status = Status::Cancelled;
            file.phase = FilePhase::Cancelled;
            file.reason = Some("Cancelled.".into());
        }
    }
}

fn compatible_settings(job: &Job, current: &Settings) -> Settings {
    let mut next = current.clone();
    next.output.booklet_only = job.dto.mode == JobMode::Booklet;
    next.output.limited_to_album_releases = job.settings.output.limited_to_album_releases;
    next.output.smart_discography = job.settings.output.smart_discography;
    if let Some(quality) = job.quality_override {
        next.output.quality = quality;
    }
    next
}

fn missing_job() -> AppError {
    AppError::invalid("queue", "This job is no longer in the queue.")
}
fn stale_plan() -> AppError {
    AppError::new(
        "stale_plan",
        "queue",
        "The submission changed. Review the current count.",
        true,
    )
}

struct PlanTask {
    id: u64,
    generation: u64,
    key: crate::qobuz::ResourceKey,
    existing: Option<Arc<ResolvedResource>>,
    limited_to_album_releases: bool,
    smart_discography: bool,
    cancel: CancellationToken,
}

struct RunTask {
    id: u64,
    generation: u64,
    settings: Settings,
    root: PathBuf,
    plan: Arc<ResolvedResource>,
    cancel: CancellationToken,
    audio: Vec<usize>,
    only_file: Option<u64>,
    account_epoch: u64,
    search_origin: bool,
    kind: ResourceKind,
    replacement: Option<(u64, AudioReplacement)>,
    mode: DownloadMode,
}

pub async fn scheduler(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            _ = app.stop.cancelled() => break,
            _ = tick.tick() => {
                let scheduled = app.clone();
                let _ = tokio::task::spawn_blocking(move || schedule(&scheduled)).await;
            },
        }
    }
}

fn schedule(app: &Arc<App>) {
    let mut state = app.state();
    state.queue.refresh_rate(Instant::now());
    state.queue.reset_idle_pause(app.qobuz.recovery());
    let recovery = app.qobuz.recovery_status(state.account.as_ref());
    if state.queue.recovery != recovery {
        state.queue.recovery = recovery;
        state.queue.changed();
    }
    for job in state.queue.jobs.values_mut() {
        for path in std::mem::take(&mut job.drive_retry) {
            app.qobuz.recovery().release_drive(&path);
        }
    }
    if state.closing {
        return;
    }
    let Some(account) = state.account.clone() else {
        return;
    };
    let api_wait = app.qobuz.api_waiting_reason(&account);
    let mut waiting_changed = false;
    for job in state
        .queue
        .jobs
        .values_mut()
        .filter(|job| job.phase == Phase::Planning)
    {
        let reason = api_wait
            .as_deref()
            .unwrap_or("Resolving Qobuz links and counting tracks…");
        let status = if api_wait.is_some() {
            Status::Queued
        } else {
            Status::Downloading
        };
        if job.dto.status != status || job.dto.reason.as_deref() != Some(reason) {
            job.dto.status = status;
            job.dto.reason = Some(reason.into());
            waiting_changed = true;
        }
    }
    if waiting_changed {
        state.queue.changed();
    }
    let interval =
        Duration::try_from_secs_f64(state.settings.interval_seconds).unwrap_or(Duration::MAX);
    let epoch = state.account_epoch;
    let queue = &mut state.queue;
    if queue.apply_search_coverage() {
        queue.changed();
    }
    let preparing = queue
        .batches
        .values()
        .filter(|batch| batch.planning)
        .count();
    let next_batch = (preparing < 2)
        .then(|| {
            queue
                .batches
                .iter()
                .find(|(id, batch)| {
                    !batch.planning
                        && batch.jobs.iter().any(|job_id| {
                            queue
                                .jobs
                                .get(job_id)
                                .is_some_and(|job| job.batch == **id && job.phase == Phase::Waiting)
                        })
                })
                .map(|(id, _)| *id)
        })
        .flatten();
    if let Some(id) = next_batch {
        let batch = queue.batches.get_mut(&id).unwrap();
        batch.planning = true;
        let mut tasks = Vec::new();
        for job_id in &batch.jobs {
            let Some(job) = queue
                .jobs
                .get_mut(job_id)
                .filter(|job| job.batch == id && job.phase == Phase::Waiting)
            else {
                continue;
            };
            job.phase = Phase::Planning;
            job.running = true;
            job.dto.status = Status::Downloading;
            job.dto.reason = Some("Resolving Qobuz links and counting tracks…".into());
            tasks.push(PlanTask {
                id: *job_id,
                generation: job.generation,
                key: job.link.key.clone(),
                existing: job.plan.clone(),
                limited_to_album_releases: job.settings.output.limited_to_album_releases,
                smart_discography: job.settings.output.smart_discography,
                cancel: job.cancel.clone(),
            });
        }
        queue.changed();
        app.tasks.spawn(resolve_batch(
            app.clone(),
            account.clone(),
            epoch,
            id,
            tasks,
        ));
    }
    let mode = queue.effective_mode;
    let sequential = mode == DownloadMode::Sequential;
    if queue.user_paused
        || mode != queue.requested_mode
        || (sequential && !queue.admitted.is_empty())
    {
        return;
    }
    let head = queue.admission_head();
    if queue
        .last_start
        .is_some_and(|last| last.elapsed() < interval)
    {
        return;
    }
    let mut changed = false;
    let mut task = None;
    let active_releases: HashSet<_> = queue.jobs.values()
        .filter(|job| job.phase == Phase::Active || job.running && job.phase == Phase::Finished)
        .filter_map(|job| job.plan.as_ref())
        .flat_map(|plan| plan.tracks.iter().map(|track| track.album_id.clone()))
        .collect();
    for job in queue
        .jobs
        .values_mut()
        .filter(|job| job.phase == Phase::Eligible && (!sequential || head == Some(job.dto.id)))
    {
        if job.search_coverage.is_some() && active_releases.contains(&job.link.key.id) {
            let reason = "Waiting for an overlapping transfer to finish.";
            changed |= job.dto.reason.as_deref() != Some(reason);
            job.dto.reason = Some(reason.into());
            continue;
        }
        let candidate = job.settings.destination(&app.root);
        let pinned = job.plan.as_ref().and_then(|plan| {
            let planned = plan
                .tracks
                .iter()
                .enumerate()
                .filter(|(index, _)| {
                    job.dto.files.iter().any(|file| {
                        file.kind == FileKind::Audio
                            && file.id == *index as u64 + 1
                            && file.status != Status::Completed
                            && job.only_file.is_none_or(|id| file.id == id)
                    })
                })
                .map(|(_, track)| job.pinned.get(&track.album_id));
            let pinned = planned.collect::<Option<Vec<_>>>()?;
            pinned
                .iter()
                .all(|layout| layout.album_root.is_dir())
                .then(|| {
                    pinned
                        .first()
                        .copied()
                        .or_else(|| job.pinned.values().next())
                        .map(|layout| layout.album_root.clone())
                })
                .flatten()
        });
        let Some(root) = candidate.filter(|root| root.is_dir()).or(pinned) else {
            let reason =
                "The saved destination is missing. Choose a folder and apply it to pending jobs.";
            changed |= job.dto.reason.as_deref() != Some(reason);
            job.dto.reason = Some(reason.into());
            continue;
        };
        let audio = job
            .dto
            .files
            .iter()
            .filter(|file| {
                file.kind == FileKind::Audio
                    && file.status != Status::Completed
                    && job.only_file.is_none_or(|id| id == file.id)
            })
            .map(|file| file.id as usize - 1)
            .collect::<Vec<_>>();
        let destinations = write_destinations(job, &root, &audio);
        let blocked = destinations
            .iter()
            .map(|path| {
                app.qobuz
                    .recovery()
                    .drive_blocked(path)
                    .map(|drive| (path.to_owned(), drive))
            })
            .collect::<Option<Vec<_>>>()
            .and_then(|blocked| blocked.into_iter().next());
        if let Some((path, drive)) = blocked {
            let reason = format!("Drive {drive} is full. Free space and choose Retry.");
            changed |= job.dto.reason.as_deref() != Some(reason.as_str()) || !job.dto.drive_blocked;
            job.dto.drive_blocked = true;
            job.dto.reason = Some(reason);
            job.blocked_destination = Some(path);
            continue;
        }
        if job.dto.drive_blocked {
            job.dto.drive_blocked = false;
            changed = true;
        }
        let Some(plan) = job.plan.clone() else {
            continue;
        };
        job.phase = Phase::Active;
        job.running = true;
        job.dto.status = Status::Downloading;
        job.dto.reason = None;
        job.blocked_destination = None;
        configure_lyrics(job, &plan);
        job.playlist_reserved = job.link.key.kind == ResourceKind::Playlist
            && job.dto.mode == JobMode::Audio
            && job.only_file.is_none()
            && job.settings.output.create_playlist
            && !audio.is_empty()
            && !job
                .extras
                .values()
                .any(|extra| matches!(extra, Extra::Playlist));
        let mut settings = job.settings.clone();
        if account.access_mode.is_preview() {
            settings.output.quality = 1;
        }
        let start = RunTask {
            id: job.dto.id,
            generation: job.generation,
            settings,
            root,
            plan,
            cancel: job.cancel.clone(),
            audio,
            only_file: job.only_file,
            account_epoch: epoch,
            search_origin: job.search_origin,
            kind: job.link.key.kind,
            replacement: job.approved_replacement.take(),
            mode,
        };
        if start.only_file.is_none()
            || !start.audio.is_empty()
            || start.only_file.is_some_and(|id| {
                matches!(
                    job.extras.get(&id),
                    Some(Extra::Artwork { .. } | Extra::Booklet { .. })
                )
            })
        {
            plan_assets(job, &start);
        }
        queue.admitted.insert(start.id);
        app.qobuz.recovery().admit_media(
            start.id,
            sequential,
            start.settings.max_connections.limit(),
        );
        task = Some(start);
        queue.last_start = Some(Instant::now());
        changed = true;
        break;
    }
    if changed {
        queue.changed();
    }
    if let Some(task) = task {
        app.tasks.spawn(run_job(app.clone(), account, task));
    }
}

fn write_destinations(job: &Job, root: &Path, audio: &[usize]) -> Vec<PathBuf> {
    let mut destinations = BTreeSet::new();
    if let Some(plan) = &job.plan {
        for index in audio {
            destinations.insert(
                job.pinned
                    .get(&plan.tracks[*index].album_id)
                    .map_or(root, |layout| layout.album_root.as_path())
                    .to_owned(),
            );
        }
        for (id, extra) in &job.extras {
            if job.only_file.is_some_and(|selected| selected != *id)
                || !job
                    .dto
                    .files
                    .iter()
                    .any(|file| file.id == *id && file.status != Status::Completed)
            {
                continue;
            }
            let path = match extra {
                Extra::Artwork { track, .. } | Extra::Booklet { track, .. } => job
                    .pinned
                    .get(&plan.tracks[*track].album_id)
                    .map_or(root, |layout| layout.album_root.as_path()),
                Extra::Lyrics { audio, .. } => audio.parent().unwrap_or(root),
                Extra::Playlist => job
                    .playlist_path
                    .as_deref()
                    .and_then(Path::parent)
                    .unwrap_or(root),
            };
            destinations.insert(path.to_owned());
        }
    }
    if job.link.key.kind == ResourceKind::Playlist && job.settings.output.create_playlist {
        destinations.insert(
            job.playlist_path
                .as_deref()
                .and_then(Path::parent)
                .unwrap_or(root)
                .to_owned(),
        );
    }
    if destinations.is_empty() {
        destinations.insert(root.to_owned());
    }
    destinations.into_iter().collect()
}

async fn resolve_batch(
    app: Arc<App>,
    account: Credentials,
    epoch: u64,
    batch_id: u64,
    tasks: Vec<PlanTask>,
) {
    for task in tasks {
        let outcome = if task.cancel.is_cancelled() {
            None
        } else if let Some(plan) = task.existing {
            Some(Ok(plan))
        } else {
            tokio::select! {
                _ = task.cancel.cancelled() => None,
                result = async {
                    let mut resource = app.qobuz.resolve_resource_for_job(&task.key, &account, task.limited_to_album_releases, task.smart_discography).await?;
                    if let Some(url) = resource.thumbnail_url.as_deref() {
                        resource.thumbnail = app.media.thumbnail(url).await;
                    }
                    Ok::<_, crate::qobuz::QobuzError>(resource)
                } => Some(result.map(Arc::new)),
            }
        };
        if outcome.as_ref().is_some_and(|outcome| {
            outcome
                .as_ref()
                .is_err_and(|error| error.code == "invalid_token")
        }) {
            app.reject_runtime_account(epoch);
        }
        let mut state = app.state();
        let current_epoch = state.account_epoch;
        let queue = &mut state.queue;
        let Some(job) = queue
            .jobs
            .get_mut(&task.id)
            .filter(|job| job.generation == task.generation)
        else {
            continue;
        };
        job.running = false;
        if job.phase != Phase::Planning {
            continue;
        }
        if current_epoch != epoch {
            job.phase = Phase::Waiting;
            job.dto.status = Status::Queued;
            continue;
        }
        match outcome {
            Some(Ok(plan)) => {
                if let Some(at) = job.activity_at {
                    let credits = plan.tracks.iter()
                        .filter_map(|track| Credit::from_track(track, job.link.key.kind))
                        .collect::<HashSet<_>>();
                    if app.storage.record_search_activity(&credits, 1, at).is_ok() {
                        job.activity_at = None;
                    }
                }
                if job.plan.is_none() {
                    for track in plan.tracks.iter().take(10) {
                        app.media.observe_track_metadata(track);
                    }
                    job.dto.title = plan.title.clone();
                    job.dto.thumbnail = plan.thumbnail.clone();
                    let label = match job.link.key.kind {
                        ResourceKind::Album => "Album",
                        ResourceKind::Track => "Track",
                        ResourceKind::Playlist => "Playlist",
                        ResourceKind::Artist => "Artist",
                        ResourceKind::Label => "Label",
                    };
                    job.dto.subtitle = format!(
                        "{label} · {} {}",
                        plan.tracks.len(),
                        if plan.tracks.len() == 1 {
                            "track"
                        } else {
                            "tracks"
                        }
                    );
                    match job.dto.mode {
                        JobMode::Audio => {
                            job.dto.files = plan
                                .tracks
                                .iter()
                                .enumerate()
                                .map(|(index, track)| {
                                    item(
                                        index as u64 + 1,
                                        FileKind::Audio,
                                        track.title.clone(),
                                        track,
                                        Some(track.disc),
                                    )
                                })
                                .collect();
                        }
                        JobMode::Booklet => {
                            plan_booklet_rows(job, &plan);
                            job.dto.subtitle = format!("{label} · {} PDFs", job.dto.files.len());
                        }
                    }
                    job.plan = Some(plan.clone());
                }
                job.phase = Phase::Held;
                job.dto.status = Status::Queued;
                job.dto.reason = Some("Waiting for the submission plan.".into());
                if job.dto.mode == JobMode::Booklet && job.dto.files.is_empty() {
                    job.phase = Phase::Finished;
                    job.dto.status = Status::Failed;
                    job.dto.reason =
                        Some("Qobuz returned no downloadable booklets for this resource.".into());
                }
            }
            Some(Err(error)) => {
                job.phase = Phase::Finished;
                job.dto.status = Status::Failed;
                job.dto.reason = Some(error.message);
            }
            None => cancel(job),
        }
        queue.changed();
    }
    let mut state = app.state();
    let queue = &mut state.queue;
    if let Some(batch) = queue.batches.get_mut(&batch_id) {
        batch.planning = false;
    }
    queue.reconsider(batch_id);
    queue.apply_search_coverage();
    queue.changed();
}

fn item(
    id: u64,
    kind: FileKind,
    title: String,
    track: &ResolvedTrack,
    disc: Option<u32>,
) -> FileDto {
    FileDto {
        id,
        kind,
        title,
        disc,
        status: Status::Queued,
        progress: Some(0),
        generation: 0,
        attempt: 0,
        phase: FilePhase::Queued,
        received_bytes: 0,
        total_bytes: None,
        final_size_bytes: None,
        quality: None,
        path: None,
        conflict_path: None,
        replacement_token: None,
        reason: None,
        reused: false,
        independent_retry: matches!(
            kind,
            FileKind::Audio | FileKind::Artwork | FileKind::Booklet | FileKind::Playlist
        ),
        album: track
            .album
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    }
}

fn next_file(job: &Job) -> u64 {
    job.dto
        .files
        .iter()
        .map(|file| file.id)
        .max()
        .unwrap_or_default()
        + 1
}

fn plan_booklet_rows(job: &mut Job, plan: &ResolvedResource) {
    let mut albums = BTreeSet::new();
    for (index, track) in plan.tracks.iter().enumerate() {
        if track.unavailable.is_some() || !albums.insert(&track.album_id) {
            continue;
        }
        for source in media::resolve_booklets(&track.album, job.settings.output.safe_mode) {
            let id = next_file(job);
            job.dto.files.push(item(
                id,
                FileKind::Booklet,
                source.name.clone(),
                track,
                None,
            ));
            job.extras.insert(
                id,
                Extra::Booklet {
                    track: index,
                    directory: PathBuf::new(),
                    source,
                },
            );
        }
    }
}

fn configure_lyrics(job: &mut Job, plan: &ResolvedResource) {
    if job.dto.mode == JobMode::Booklet
        || job.only_file.is_some_and(|id| job.extras.contains_key(&id))
    {
        return;
    }
    if job.settings.output.lyrics_mode != LyricsMode::External {
        job.dto
            .files
            .retain(|file| file.kind != FileKind::Lyrics || file.status == Status::Completed);
        let ids: BTreeSet<_> = job.dto.files.iter().map(|file| file.id).collect();
        job.extras.retain(|id, _| ids.contains(id));
        return;
    }
    for (index, track) in plan.tracks.iter().enumerate() {
        if job.only_file.is_some_and(|id| id != index as u64 + 1) {
            continue;
        }
        let audio = job
            .dto
            .files
            .iter()
            .find(|file| file.id == index as u64 + 1)
            .unwrap();
        if audio.status == Status::Completed {
            continue;
        }
        let title = format!("Lyrics · {}", track.title);
        if lyric_file(job, index).is_none() {
            let id = next_file(job);
            job.dto
                .files
                .push(item(id, FileKind::Lyrics, title, track, Some(track.disc)));
            job.extras.insert(
                id,
                Extra::Lyrics {
                    track: index,
                    audio: PathBuf::new(),
                },
            );
        }
    }
}

fn lyric_file(job: &Job, index: usize) -> Option<u64> {
    job.extras.iter().find_map(|(id, extra)| {
        matches!(extra, Extra::Lyrics { track, .. } if *track == index).then_some(*id)
    })
}

fn update_job<R>(app: &App, task: &RunTask, operation: impl FnOnce(&mut Job) -> R) -> Option<R> {
    let mut state = app.state();
    let queue = &mut state.queue;
    let job = queue
        .jobs
        .get_mut(&task.id)
        .filter(|job| job.generation == task.generation)?;
    let value = operation(job);
    queue.changed();
    Some(value)
}

fn update_file(job: &mut Job, id: u64, operation: impl FnOnce(&mut FileDto)) {
    if let Some(file) = job.dto.files.iter_mut().find(|file| file.id == id) {
        file.generation = job.generation;
        operation(file);
    }
}

fn progress<'a>(
    app: &'a App,
    task: &'a RunTask,
    id: u64,
) -> impl Fn(TransferProgress) + Send + Sync + 'a {
    let last = Mutex::new(None::<(Instant, u64, TransferPhase)>);
    move |progress| {
        if task.cancel.is_cancelled() {
            return;
        }
        let mut last = last.lock().unwrap();
        if last.is_some_and(|(last, attempt, phase)| {
            last.elapsed() < Duration::from_millis(170)
                && attempt == progress.attempt
                && phase == progress.phase
        }) {
            return;
        }
        *last = Some((Instant::now(), progress.attempt, progress.phase));
        app.state()
            .queue
            .progress(task.id, task.generation, id, progress);
    }
}

async fn run_file_stage<T, F: Future<Output = std::result::Result<T, media::MediaError>>>(
    app: &App,
    task: &RunTask,
    id: u64,
    transfer: &TransferContext,
    mut work: impl FnMut() -> F,
) -> std::result::Result<T, media::MediaError> {
    loop {
        if !transfer.wait_for_media(&task.cancel).await {
            return Err(media::MediaError::cancelled());
        }
        let result = work().await;
        let Err(error) = &result else {
            return result;
        };
        if error.code != "disk_full" || task.cancel.is_cancelled() {
            return result;
        }
        let destination = error.path.as_deref().unwrap_or(&task.root);
        transfer.disk_full(destination);
        let Some(drive) = app.qobuz.recovery().drive_blocked(destination) else {
            return result;
        };
        let reason = format!("Drive {drive} is full. Free space and choose Retry.");
        update_job(app, task, |job| {
            job.active_drive_blocks.insert(destination.to_owned());
            job.blocked_destination = Some(destination.to_owned());
            job.dto.drive_blocked = true;
            job.dto.reason = Some(reason.clone());
            update_file(job, id, |file| {
                file.status = Status::Retrying;
                file.phase = FilePhase::Retrying;
                file.reason = Some(reason);
            });
        });
        if !transfer
            .wait_for_drive_release(destination, &task.cancel)
            .await
        {
            return Err(media::MediaError::cancelled());
        }
        update_job(app, task, |job| {
            job.active_drive_blocks.remove(destination);
            job.blocked_destination = job.active_drive_blocks.first().cloned();
            if job.active_drive_blocks.is_empty() {
                job.dto.drive_blocked = false;
                job.dto.reason = None;
            }
            update_file(job, id, |file| {
                file.status = Status::Downloading;
                file.phase = FilePhase::Connecting;
                file.reason = None;
            });
        });
    }
}

async fn run_job(app: Arc<App>, account: Credentials, task: RunTask) {
    let groups = release_groups(&task.plan);
    let group_limit = if task.mode == DownloadMode::Sequential {
        1
    } else {
        CONCURRENT_MEDIA_LIMIT
    };
    stream::iter(groups)
        .for_each_concurrent(group_limit, |(album, tracks)| {
            run_release(&app, &task, &account, album, tracks)
        })
        .await;
    write_playlist(&app, &task).await;
    if task.cancel.is_cancelled() {
        app.media.discard_retained_jobs(&[task.id]).await;
    }
    let mut state = app.state();
    let queue = &mut state.queue;
    queue.finish_admission(task.id, app.qobuz.recovery());
    if let Some(job) = queue
        .jobs
        .get_mut(&task.id)
        .filter(|job| job.generation == task.generation)
    {
        job.running = false;
        job.active_drive_blocks.clear();
        job.dto.drive_blocked = false;
        if task.cancel.is_cancelled() {
            cancel(job);
            queue.reset_idle_pause(app.qobuz.recovery());
            queue.changed();
            return;
        }
        job.phase = Phase::Finished;
        let primary = job
            .dto
            .files
            .iter()
            .filter(|file| file.kind == job.dto.mode.primary_file());
        let total = primary.clone().count();
        let finished = primary
            .filter(|file| file.status == Status::Completed)
            .count();
        job.dto.status = aggregate(finished, total);
        job.dto.reason = if finished == 0 {
            Some(if job.dto.mode == JobMode::Booklet {
                if total == 0 {
                    "Qobuz returned no downloadable booklets for this resource.".into()
                } else {
                    "No booklets could be finalized. Expand this row for details.".into()
                }
            } else if total == 0 {
                "This resource has no downloadable audio tracks.".into()
            } else {
                "No audio tracks could be finalized. Expand this row for details.".into()
            })
        } else {
            None
        };
    }
    queue.reset_idle_pause(app.qobuz.recovery());
    queue.changed();
}

fn aggregate(finished: usize, total: usize) -> Status {
    if finished == 0 {
        Status::Failed
    } else if finished < total {
        Status::Incomplete
    } else {
        Status::Completed
    }
}

fn release_progress(job: &Job) -> Option<u8> {
    let count = job.dto.files.len() + usize::from(job.playlist_reserved);
    if count == 0 {
        return Some(0);
    }
    let sum: usize = job
        .dto
        .files
        .iter()
        .map(|file| match file.status {
            Status::Completed => 100,
            Status::Failed | Status::Cancelled | Status::Unavailable => 0,
            _ => usize::from(file.progress.unwrap_or(0).min(99)),
        })
        .sum();
    let average = (sum / count) as u8;
    let finalized = job.phase == Phase::Finished
        && job.dto.status == Status::Completed
        && !job.playlist_reserved
        && job
            .dto
            .files
            .iter()
            .all(|file| file.status == Status::Completed);
    Some(if finalized { average } else { average.min(99) })
}

async fn download_audio(
    app: &Arc<App>,
    task: &RunTask,
    account: &Credentials,
    index: usize,
    artwork_cache: &ArtworkCache,
    first_pass: bool,
) -> bool {
    let id = index as u64 + 1;
    let track = &task.plan.tracks[index];
    if task.cancel.is_cancelled() {
        return false;
    }
    let transfer = transfer_context(app, task, id);
    if !transfer.wait_for_media(&task.cancel).await {
        return false;
    }
    let (pinned, cover_basename) = update_job(app, task, |job| {
        update_file(job, id, |file| {
            file.status = Status::Downloading;
            file.phase = FilePhase::Connecting;
            file.reason = None;
        });
        let pinned = job.pinned.get(&track.album_id).cloned();
        let basename = pinned.as_ref().and_then(|layout| {
            job.extras.values().find_map(|extra| match extra {
                Extra::Artwork {
                    track: cover_track,
                    directory,
                    basename,
                    final_target: true,
                } if task.plan.tracks[*cover_track].album_id == track.album_id
                    && layout.cover_dir.as_ref() == Some(directory) =>
                {
                    Some(basename.clone())
                }
                _ => None,
            })
        });
        (pinned, basename)
    })
    .unwrap_or_default();
    let request = AudioRequest {
        #[cfg(target_os = "macos")]
        require_full_duration: false,
        client: &app.qobuz,
        credentials: account,
        track,
        options: &task.settings.output,
        root: &task.root,
        pinned: pinned.as_ref(),
        transfer: &transfer,
        sequence: fallback_sequence(&task.plan, index),
        replacement: task
            .replacement
            .as_ref()
            .filter(|(file_id, _)| *file_id == id && !account.access_mode.is_preview())
            .map(|(_, approval)| approval),
        artwork_cache,
        cover_basename: cover_basename
            .as_deref()
            .unwrap_or(&task.settings.output.cover_filename),
    };
    let result = run_file_stage(app, task, id, &transfer, || {
        app.media
            .download_audio(request, &task.cancel, progress(app, task, id))
    })
    .await;
    if first_pass
        && result
            .as_ref()
            .is_err_and(|error| error.code == "stream_no_response")
        && !task.cancel.is_cancelled()
    {
        update_job(app, task, |job| {
            update_file(job, id, |file| {
                file.status = Status::Retrying;
                file.phase = FilePhase::Retrying;
                file.reason =
                    Some("No response from Qobuz. Retrying after the other tracks.".into());
            })
        });
        return true;
    }
    if let Ok(output) = &result
        && let Some(at) = output.finalized_at
        && let Some(credit) = Credit::from_track(track, task.kind)
    {
        // Successful publication produced a verified file and receipt. The
        // profile cutoff still rejects finalization events older than a reset.
        let _ = app.storage.record_search_activity([&credit], 2, at);
    }
    let replacement = if let Err(error) = &result
        && error.code == "output_conflict" && task.search_origin && !account.access_mode.is_preview()
        && let Some(path) = &error.path
    {
        app.media.offer_audio_replacement(path, &track.id, &task.cancel).await.ok()
    } else {
        None
    };
    let audio_size = match &result {
        Ok(output) => final_size(&output.path).await,
        Err(_) => None,
    };
    if result
        .as_ref()
        .is_err_and(|error| error.code == "invalid_token")
    {
        app.reject_runtime_account(task.account_epoch);
    }
    update_job(app, task, |job| {
        let token = replacement.as_ref().map(|offer| offer.token.clone());
        if let Some(offer) = replacement {
            job.replacements.insert(id, offer);
        }
        update_file(job, id, |file| file.replacement_token = token);
        match result {
            Ok(output) => {
                #[cfg(not(target_os = "macos"))]
                let is_preview = account.access_mode.is_preview();
                #[cfg(target_os = "macos")]
                let is_preview = output.is_preview;
                let quality = if is_preview {
                    "MP3 preview".into()
                } else {
                    quality_label(&output.quality)
                };
                let lyrics_id = lyric_file(job, index);
                let warning = output.warning;
                let lyrics_quality = output
                    .lyrics_source
                    .map(|source| lyrics_label(source, output.lyrics_timed.unwrap_or(false)));
                update_file(job, id, |file| {
                    complete(file, output.path.clone(), Some(quality), audio_size);
                    file.reused = output.reused;
                    if lyrics_id.is_none() {
                        file.reason = warning.as_ref().map(|warning| warning.message.clone());
                        if task.settings.output.lyrics_mode == LyricsMode::Embed
                            && let Some(provider) = &lyrics_quality
                        {
                            let previous = file
                                .reason
                                .take()
                                .map(|reason| format!("{reason} "))
                                .unwrap_or_default();
                            file.reason = Some(format!("{previous}Lyrics · {provider}"));
                        }
                    }
                    if let Some(warning) = &output.cover_warning {
                        let previous = file
                            .reason
                            .take()
                            .map(|reason| format!("{reason} "))
                            .unwrap_or_default();
                        file.reason = Some(format!("{previous}{}", warning.message));
                    }
                });
                if task.settings.output.download_cover_art
                    && task.settings.output.embed_cover_art
                    && !output.cover_embedded
                {
                    job.missing_embedded.insert(index);
                    update_file(job, id, |file| {
                        let previous = file
                            .reason
                            .take()
                            .map(|reason| format!("{reason} "))
                            .unwrap_or_default();
                        file.reason = Some(format!("{previous}Embedded artwork is missing."));
                    });
                } else if output.cover_embedded {
                    job.missing_embedded.remove(&index);
                }
                let cover_dir = output.layout.cover_dir.clone().unwrap_or_default();
                let cover_key = (track.album_id.clone(), cover_dir);
                if output.cover_path.is_none()
                    && let Some(warning) = output.cover_warning
                {
                    job.artwork_failures.insert(cover_key, warning);
                } else {
                    job.artwork_failures.remove(&cover_key);
                }
                job.paths.insert(index, output.path.clone());
                job.pinned.insert(track.album_id.clone(), output.layout);
                refresh_cover_warnings(job, &task.plan, &track.album_id);
                if let Some(lyrics_id) = lyrics_id {
                    job.extras.insert(
                        lyrics_id,
                        Extra::Lyrics {
                            track: index,
                            audio: output.path,
                        },
                    );
                }
            }
            Err(error) if !task.cancel.is_cancelled() => {
                let unavailable = error.code == "preview_unavailable";
                update_file(job, id, |file| file_error(file, error));
                if unavailable {
                    update_file(job, id, |file| {
                        file.status = Status::Unavailable;
                        file.phase = FilePhase::Unavailable;
                        file.independent_retry = false;
                    });
                }
                if let Some(lyrics) = lyric_file(job, index) {
                    update_file(job, lyrics, |file| {
                        fail(
                            file,
                            "Lyrics could not be saved because the audio is unfinished.".into(),
                        )
                    });
                }
            }
            Err(_) => (),
        }
    });
    false
}

fn transfer_context(app: &Arc<App>, task: &RunTask, id: u64) -> TransferContext {
    let owner = app.clone();
    let job_id = task.id;
    let generation = task.generation;
    app.qobuz.recovery().transfer(
        job_id,
        generation,
        task.settings.max_connections.limit(),
        move |notice| {
            let mut state = owner.state();
            let queue = &mut state.queue;
            let Some(job) = queue
                .jobs
                .get_mut(&job_id)
                .filter(|job| job.generation == generation && !job.cancel.is_cancelled())
            else {
                return;
            };
            if let RecoveryNotice::NetworkBytes { bytes } = notice {
                queue.rate.record(Instant::now(), bytes);
                return;
            }
            update_file(job, id, |file| match notice {
                RecoveryNotice::Transferring => {
                    if file.status == Status::Retrying {
                        file.progress = Some(0);
                    }
                    file.status = Status::Downloading;
                    if matches!(file.phase, FilePhase::Queued | FilePhase::Retrying) {
                        file.phase = FilePhase::Connecting;
                    }
                    file.reason = None;
                }
                RecoveryNotice::Retrying { reason } => {
                    file.status = Status::Retrying;
                    file.phase = FilePhase::Retrying;
                    file.reason = Some(reason);
                }
                RecoveryNotice::RateLimited { seconds } => {
                    file.status = Status::Retrying;
                    file.phase = FilePhase::Retrying;
                    file.reason = Some(format!(
                        "This file host is limiting requests. Retrying in {seconds}s."
                    ));
                }
                RecoveryNotice::NetworkBytes { .. } => {
                    unreachable!("byte notices are handled before file state")
                }
            });
            queue.changed();
        },
    )
}

fn fallback_sequence(plan: &ResolvedResource, index: usize) -> usize {
    let track = &plan.tracks[index];
    if track.number > 0 {
        return track.number as usize;
    }
    let original = if track.id.is_empty() {
        index
    } else {
        plan.tracks[..=index]
            .iter()
            .position(|candidate| candidate.id == track.id)
            .unwrap_or(index)
    };
    plan.tracks[..=original]
        .iter()
        .filter(|candidate| candidate.album_id == track.album_id && candidate.disc == track.disc)
        .count()
}

fn release_groups(plan: &ResolvedResource) -> Vec<(&str, Vec<usize>)> {
    let mut by_album = HashMap::new();
    let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
    for (index, track) in plan.tracks.iter().enumerate() {
        let album = track.album_id.as_str();
        let next = groups.len();
        let group = *by_album.entry(album).or_insert(next);
        if group == next {
            groups.push((album, Vec::new()));
        }
        groups[group].1.push(index);
    }
    groups
}

fn release_extras(app: &App, task: &RunTask, album: &str, kind: FileKind) -> Vec<(u64, Extra)> {
    let state = app.state();
    let Some(job) = state
        .queue
        .jobs
        .get(&task.id)
        .filter(|job| job.generation == task.generation)
    else {
        return Vec::new();
    };
    job.dto
        .files
        .iter()
        .filter_map(|file| {
            if file.kind != kind || file.status != Status::Queued {
                return None;
            }
            let extra = job.extras.get(&file.id)?;
            let track = match extra {
                Extra::Artwork { track, .. } | Extra::Booklet { track, .. } => *track,
                Extra::Lyrics { track, audio } if !audio.as_os_str().is_empty() => *track,
                _ => return None,
            };
            let dependency = match kind {
                FileKind::Artwork => task
                    .audio
                    .iter()
                    .any(|index| task.plan.tracks[*index].album_id == album),
            FileKind::Lyrics => task.audio.binary_search(&track).is_ok(),
                _ => false,
            };
            (task.plan.tracks[track].album_id == album
                && task.only_file.is_none_or(|id| id == file.id || dependency))
            .then(|| (file.id, extra.clone()))
        })
        .collect()
}

async fn run_release(
    app: &Arc<App>,
    task: &RunTask,
    account: &Credentials,
    album: &str,
    tracks: Vec<usize>,
) {
    let artwork = ArtworkCache::default();
    for (id, extra) in release_extras(app, task, album, FileKind::Artwork) {
        if let Extra::Artwork {
            track,
            ref directory,
            ref basename,
            final_target: false,
        } = extra
        {
            if task.cancel.is_cancelled() {
                break;
            }
            let transfer = transfer_context(app, task, id);
            if !transfer.wait_for_media(&task.cancel).await {
                break;
            }
            update_job(app, task, |job| {
                update_file(job, id, |file| {
                    file.status = Status::Downloading;
                    file.phase = FilePhase::Connecting;
                })
            });
            let result = run_file_stage(app, task, id, &transfer, || {
                app.media.prestage_artwork(
                    &task.plan.tracks[track],
                    directory,
                    &task.root,
                    task.settings.output.artwork,
                    basename,
                    &transfer,
                    &task.cancel,
                    &artwork,
                    progress(app, task, id),
                )
            })
            .await;
            update_job(app, task, |job| {
                update_file(job, id, |file| match result {
                    Ok(()) => file.status = Status::Queued,
                    Err(error) if !task.cancel.is_cancelled() => file_error(file, error),
                    Err(_) => (),
                });
            });
        } else {
            download_extra(app, task, account, id, extra, Some(&artwork)).await;
        }
    }

    let mut audio = tracks
        .into_iter()
        .filter(|index| task.audio.binary_search(index).is_ok());
    let limit = if task.mode == DownloadMode::Sequential {
        SEQUENTIAL_MEDIA_LIMIT
    } else {
        CONCURRENT_MEDIA_LIMIT
    };
    let mut deferred = Vec::new();
    // The first successful audio pins the layout before other tracks write to it.
    while !task.cancel.is_cancelled() {
        let pinned = app
            .state()
            .queue
            .jobs
            .get(&task.id)
            .is_some_and(|job| job.pinned.contains_key(album));
        if pinned {
            break;
        }
        let Some(index) = audio.next() else {
            break;
        };
        if download_audio(app, task, account, index, &artwork, true).await {
            deferred.push(index);
        }
    }
    let pinned = app
        .state()
        .queue
        .jobs
        .get(&task.id)
        .is_some_and(|job| job.pinned.contains_key(album));
    if pinned {
        settle_release_cover(app, task, account, album, &artwork).await;
    }
    {
        let artwork = &artwork;
        let pending = stream::iter(audio.map(|index| async move {
            download_audio(app, task, account, index, artwork, true)
                .await
                .then_some(index)
        }))
        .buffer_unordered(limit);
        settle_audio_pass(pending, deferred, &task.cancel, |index| async move {
            let budget = crate::recovery::RetryBudget::default();
            tokio::select! {
                biased;
                _ = task.cancel.cancelled() => return,
                _ = budget.wait("") => {}
            }
            download_audio(app, task, account, index, artwork, false).await;
        })
        .await;
    }
    if !pinned {
        settle_release_cover(app, task, account, album, &artwork).await;
    }
    app.media.discard_artwork(artwork).await;
    for kind in [FileKind::Lyrics, FileKind::Booklet] {
        stream::iter(release_extras(app, task, album, kind))
            .for_each_concurrent(limit, |(id, extra)| {
                download_extra(app, task, account, id, extra, None)
            })
            .await;
    }
}

async fn settle_audio_pass<F: Future<Output = ()>>(
    pending: impl Stream<Item = Option<usize>>,
    mut deferred: Vec<usize>,
    cancel: &CancellationToken,
    mut replay: impl FnMut(usize) -> F,
) {
    let mut pending = std::pin::pin!(pending);
    while let Some(index) = pending.next().await {
        deferred.extend(index);
    }
    deferred.sort_unstable();
    for index in deferred {
        if cancel.is_cancelled() {
            break;
        }
        replay(index).await;
    }
}

async fn settle_release_cover(
    app: &Arc<App>,
    task: &RunTask,
    account: &Credentials,
    album: &str,
    artwork: &ArtworkCache,
) {
    if task.cancel.is_cancelled() {
        return;
    }
    update_job(app, task, |job| {
        if task.only_file.is_none() || !task.audio.is_empty() {
            plan_assets_for(job, task, Some(album));
        }
    });
    for (id, extra) in release_extras(app, task, album, FileKind::Artwork) {
        if matches!(
            extra,
            Extra::Artwork {
                final_target: false,
                ..
            }
        ) {
            update_job(app, task, |job| {
                update_file(job, id, |file| {
                    file.status = Status::Unavailable;
                    file.phase = FilePhase::Unavailable;
                    file.reason = Some("The cover has no verified output folder.".into());
                    file.independent_retry = false;
                })
            });
        } else {
            download_extra(app, task, account, id, extra, Some(artwork)).await;
        }
    }
}

fn plan_assets(job: &mut Job, task: &RunTask) {
    plan_assets_for(job, task, None);
}

fn plan_assets_for(job: &mut Job, task: &RunTask, album: Option<&str>) {
    let mut cover_albums = BTreeSet::new();
    let mut booklet_dirs = BTreeSet::new();
    let mut proposed = Vec::new();
    let requested = DeliveredQuality::requested(task.settings.output.quality);
    for (index, track) in task.plan.tracks.iter().enumerate() {
        if track.unavailable.is_some() || album.is_some_and(|album| track.album_id != album) {
            continue;
        }
        let pinned = job.pinned.get(&track.album_id);
        let Ok((_, layout)) = media::plan_output(
            &task.root,
            &task.settings.output,
            track,
            &requested,
            pinned,
            fallback_sequence(&task.plan, index),
        ) else {
            continue;
        };
        let directory = layout.cover_dir.clone().unwrap_or_default();
        if job.dto.mode == JobMode::Audio
            && task.settings.output.download_cover_art
            && cover_albums.insert(track.album_id.as_str())
        {
            proposed.push((
                FileKind::Artwork,
                "Cover artwork".to_owned(),
                None,
                index,
                Extra::Artwork {
                    track: index,
                    directory,
                    basename: task.settings.output.cover_filename.clone(),
                    final_target: pinned.is_some(),
                },
                layout.album_root.clone(),
            ));
        }
        if booklet_dirs.insert((track.album_id.as_str(), layout.album_root.clone())) {
            for source in media::resolve_booklets(&track.album, task.settings.output.safe_mode) {
                proposed.push((
                    FileKind::Booklet,
                    source.name.clone(),
                    None,
                    index,
                    Extra::Booklet {
                        track: index,
                        directory: layout.album_root.clone(),
                        source,
                    },
                    layout.album_root.clone(),
                ));
            }
        }
    }
    let mut relevant = BTreeSet::new();
    for (kind, title, disc, index, mut extra, asset_root) in proposed {
        let existing = job.extras.iter().find_map(|(id, old)| {
            let same = match (old, &extra) {
                (Extra::Artwork { track: a, .. }, Extra::Artwork { track: b, .. }) => {
                    task.plan.tracks[*a].album_id == task.plan.tracks[*b].album_id
                }
                (
                    Extra::Booklet {
                        track: a,
                        source: x,
                        ..
                    },
                    Extra::Booklet {
                        track: b,
                        source: y,
                        ..
                    },
                ) => task.plan.tracks[*a].album_id == task.plan.tracks[*b].album_id && x.id == y.id,
                _ => false,
            };
            same.then_some(*id)
        });
        let id = existing.unwrap_or_else(|| {
            let id = next_file(job);
            job.dto
                .files
                .push(item(id, kind, title, &task.plan.tracks[index], disc));
            id
        });
        if let Some(Extra::Artwork {
            basename: previous,
            final_target: true,
            ..
        }) = job.extras.get(&id)
            && let Extra::Artwork { basename, .. } = &mut extra
        {
            basename.clone_from(previous);
        }
        if matches!(
            job.extras.get(&id),
            Some(Extra::Artwork {
                final_target: false,
                ..
            })
        ) && matches!(
            &extra,
            Extra::Artwork {
                final_target: true,
                ..
            }
        ) && let Some(row) = job
            .dto
            .files
            .iter_mut()
            .find(|row| row.id == id && row.status == Status::Unavailable)
        {
            *row = item(
                id,
                FileKind::Artwork,
                "Cover artwork".into(),
                &task.plan.tracks[index],
                None,
            );
            row.generation = job.generation;
        }
        if let Extra::Artwork {
            track, directory, ..
        } = &extra
            && let Some(error) = job
                .artwork_failures
                .get(&(task.plan.tracks[*track].album_id.clone(), directory.clone()))
                .cloned()
        {
            update_file(job, id, |file| {
                if file.status == Status::Queued {
                    file_error(file, error);
                }
            });
        }
        if job
            .dto
            .files
            .iter()
            .any(|file| file.id == id && file.status != Status::Completed)
        {
            job.asset_roots.insert(id, asset_root);
            job.extras.insert(id, extra);
        }
        relevant.insert(id);
    }
    if let Some(album) = album {
        for (id, extra) in &job.extras {
            if !relevant.contains(id)
                && matches!(extra, Extra::Artwork { track, .. } | Extra::Booklet { track, .. }
                if task.plan.tracks[*track].album_id == album)
                && let Some(file) = job.dto.files.iter_mut().find(|file| file.id == *id)
                && file.status == Status::Queued
            {
                file.status = Status::Unavailable;
                file.phase = FilePhase::Unavailable;
                file.independent_retry = false;
                file.reason = Some("This file has no verified output folder.".into());
            }
        }
        return;
    }
    job.dto.files.retain(|file| {
        matches!(
            file.kind,
            FileKind::Audio | FileKind::Lyrics | FileKind::Playlist
        ) || relevant.contains(&file.id)
            || file.status == Status::Completed
    });
    let ids: BTreeSet<_> = job.dto.files.iter().map(|file| file.id).collect();
    job.extras.retain(|id, _| ids.contains(id));
}

async fn download_extra(
    app: &Arc<App>,
    task: &RunTask,
    account: &Credentials,
    id: u64,
    extra: Extra,
    artwork_cache: Option<&ArtworkCache>,
) {
    if task.cancel.is_cancelled() {
        return;
    }
    let transfer = transfer_context(app, task, id);
    if !transfer.wait_for_media(&task.cancel).await {
        return;
    }
    let artwork = match &extra {
        Extra::Artwork {
            track,
            final_target: true,
            ..
        } => Some(*track),
        Extra::Artwork {
            final_target: false,
            ..
        } => return,
        _ => None,
    };
    update_job(app, task, |job| {
        update_file(job, id, |file| {
            file.status = Status::Downloading;
            file.phase = FilePhase::Connecting;
            file.reason = None;
        })
    });
    let extra = &extra;
    let transfer = &transfer;
    let result = run_file_stage(app, task, id, transfer, || async move {
        match extra {
            Extra::Artwork {
                track,
                directory,
                basename,
                ..
            } => app
                .media
                .download_artwork(
                    &task.plan.tracks[*track],
                    directory,
                    task.settings.output.artwork,
                    basename,
                    transfer,
                    &task.cancel,
                    artwork_cache,
                    progress(app, task, id),
                )
                .await
                .map(|output| {
                    let size = output
                        .width
                        .zip(output.height)
                        .map(|(width, height)| format!("{width} × {height} px"))
                        .unwrap_or_else(|| output.resolution.label().to_owned());
                    (output.path, Some(size), output.reused)
                }),
            Extra::Booklet { track, directory, source } => app
                .media
                .download_booklet(
                    source,
                    &task.plan.tracks[*track].album_id,
                    directory,
                    transfer,
                    &task.cancel,
                    progress(app, task, id),
                )
                .await
                .map(|path| (path, None, false)),
            Extra::Lyrics { track, audio } => {
                let artwork_cache = ArtworkCache::default();
                let request = AudioRequest {
                    #[cfg(target_os = "macos")]
                    require_full_duration: false,
                    replacement: None,
                    artwork_cache: &artwork_cache,
                    cover_basename: &task.settings.output.cover_filename,
                    client: &app.qobuz,
                    credentials: account,
                    track: &task.plan.tracks[*track],
                    options: &task.settings.output,
                    root: &task.root,
                    pinned: None,
                    transfer,
                    sequence: fallback_sequence(&task.plan, *track),
                };
                app.media
                    .download_external_lyrics(request, audio, &task.cancel)
                    .await
                    .map(|output| (output.path, Some(lyrics_label(output.source, output.timed)), false))
            }
            Extra::Playlist => unreachable!("Playlist export uses its own stage"),
        }
    })
    .await;
    let size = match &result {
        Ok((path, _, _)) => final_size(path).await,
        Err(_) => None,
    };
    update_job(app, task, |job| {
        update_file(job, id, |file| match result {
            Ok((path, quality, reused)) => {
                complete(file, path, quality, size);
                file.reused = reused;
            }
            Err(error) if !task.cancel.is_cancelled() && file.kind == FileKind::Lyrics => {
                lyric_failure(file, Some(error))
            }
            Err(error) if !task.cancel.is_cancelled() => file_error(file, error),
            Err(_) => (),
        });
        if let Some(index) = artwork {
            refresh_cover_warnings(job, &task.plan, &task.plan.tracks[index].album_id);
        }
    });
}

fn refresh_cover_warnings(job: &mut Job, plan: &ResolvedResource, album: &str) {
    let Some(directory) = job
        .pinned
        .get(album)
        .and_then(|layout| layout.cover_dir.as_ref())
    else {
        return;
    };
    let count = job
        .missing_embedded
        .iter()
        .filter(|index| plan.tracks[**index].album_id == album)
        .count();
    let reason = (count > 0).then(|| {
        if count == 1 {
            "1 finalized track still lacks embedded artwork.".into()
        } else {
            format!("{count} finalized tracks still lack embedded artwork.")
        }
    });
    for file in &mut job.dto.files {
        if file.status == Status::Completed
            && matches!(job.extras.get(&file.id), Some(Extra::Artwork { track, directory: path, .. })
                if plan.tracks[*track].album_id == album && path == directory)
        {
            file.reason.clone_from(&reason);
        }
    }
}

async fn write_playlist(app: &Arc<App>, task: &RunTask) {
    if task.cancel.is_cancelled() || task.settings.output.booklet_only {
        return;
    }
    let prepare = |job: &Job| {
        let selected_playlist = task
            .only_file
            .filter(|id| matches!(job.extras.get(id), Some(Extra::Playlist)));
        job.link.key.kind == ResourceKind::Playlist
            && !job.paths.is_empty()
            && (selected_playlist.is_some()
                || task.settings.output.create_playlist
                    && task.audio.iter().any(|index| job.paths.contains_key(index)))
    };
    let needed = app.state().queue.jobs.get(&task.id).is_some_and(prepare);
    if !needed {
        update_job(app, task, |job| job.playlist_reserved = false);
        return;
    }
    let gate = transfer_context(app, task, 0);
    if !gate.wait_for_media(&task.cancel).await {
        return;
    }
    let request = update_job(app, task, |job| {
        let existing = job
            .extras
            .iter()
            .find_map(|(id, extra)| matches!(extra, Extra::Playlist).then_some(*id));
        let file_id = existing.unwrap_or_else(|| {
            let id = next_file(job);
            job.dto.files.push(item(
                id,
                FileKind::Playlist,
                "Playlist export".into(),
                &task.plan.tracks[0],
                None,
            ));
            job.extras.insert(id, Extra::Playlist);
            id
        });
        job.playlist_reserved = false;
        update_file(job, file_id, |file| {
            file.status = Status::Downloading;
            file.phase = FilePhase::Processing;
            file.reason = None;
        });
        Some((
            file_id,
            job.link.key.id.clone(),
            job.playlist_path.clone(),
            job.paths.values().cloned().collect::<Vec<_>>(),
        ))
    })
    .flatten();
    let Some((file_id, playlist_id, recorded, paths)) = request else {
        return;
    };
    let transfer = transfer_context(app, task, file_id);
    let destination = recorded
        .as_deref()
        .and_then(Path::parent)
        .unwrap_or(&task.root);
    let result = run_file_stage(app, task, file_id, &transfer, || async {
        app.media
            .wait_for_drive(destination, &transfer, &task.cancel)
            .await?;
        app.media
            .write_playlist(
                &task.plan.title,
                &playlist_id,
                &task.root,
                recorded.as_deref(),
                &paths,
                task.settings.output.safe_mode,
                &task.cancel,
            )
            .await
    })
    .await;
    let size = match &result {
        Ok(path) => final_size(path).await,
        Err(_) => None,
    };
    update_job(app, task, |job| match result {
        Ok(path) => {
            job.playlist_path = Some(path.clone());
            update_file(job, file_id, |file| complete(file, path, None, size));
        }
        Err(error) if !task.cancel.is_cancelled() => {
            update_file(job, file_id, |file| file_error(file, error));
        }
        Err(_) => (),
    });
}

async fn final_size(path: &Path) -> Option<u64> {
    tokio::fs::metadata(path)
        .await
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
}

fn complete(file: &mut FileDto, path: PathBuf, quality: Option<String>, size: Option<u64>) {
    file.status = Status::Completed;
    file.progress = Some(100);
    file.phase = FilePhase::Completed;
    file.final_size_bytes = size;
    file.path = Some(path);
    file.conflict_path = None;
    file.quality = quality;
    file.reason = None;
}

fn fail(file: &mut FileDto, reason: String) {
    file.status = Status::Failed;
    file.phase = FilePhase::Failed;
    file.reason = Some(reason);
}

fn file_error(file: &mut FileDto, error: media::MediaError) {
    if error.code == "path_conflict"
        || error.code == "file_conflict"
        || error.code == "output_conflict"
        || error.code == "artwork_conflict"
    {
        file.conflict_path = error.path;
    }
    fail(file, error.message);
}

fn lyric_failure(file: &mut FileDto, error: Option<media::MediaError>) {
    if error
        .as_ref()
        .is_none_or(|error| error.code == "lyrics_unavailable")
    {
        file.status = Status::Unavailable;
        file.phase = FilePhase::Unavailable;
        file.reason = Some("Lyrics unavailable".into());
        file.independent_retry = false;
    } else if let Some(error) = error {
        file.independent_retry = error.retryable;
        file_error(file, error);
    }
}

fn quality_label(quality: &DeliveredQuality) -> String {
    if quality.tier == 1 {
        return "MP3 · 320 kbps".into();
    }
    match (quality.bit_depth, quality.sample_rate) {
        (Some(bits), Some(rate)) => format!("FLAC · {bits}-bit / {rate} kHz"),
        _ => format!("FLAC · tier {}", quality.tier),
    }
}

fn lyrics_label(source: LyricsSource, timed: bool) -> String {
    let provider = match source {
        LyricsSource::Qobuz => "Qobuz",
        LyricsSource::Lrclib => "LRCLIB",
    };
    format!(
        "{provider} · {}",
        if timed { "Timed" } else { "Plain text" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qobuz::normalize_link;

    fn track(id: &str, album_id: &str, disc: u32, number: u32) -> ResolvedTrack {
        ResolvedTrack {
            id: id.into(),
            title: "Track".into(),
            album_id: album_id.into(),
            disc,
            number,
            track: serde_json::json!({}),
            album: Arc::new(
                serde_json::json!({"id": album_id, "title": "Album", "goodies": [
                    {"id": "book", "file_format_id": 21, "name": "Booklet", "url": "https://example.test/book.pdf"}
                ]}),
            ),
            unavailable: None,
        }
    }

    fn active_queue() -> Queue {
        let mut queue = Queue::default();
        queue.enqueue(
            vec![normalize_link("https://qobuz.com/album/1").unwrap()],
            &Settings::default(),
            "Waiting",
        );
        let job = queue.jobs.get_mut(&1).unwrap();
        job.phase = Phase::Active;
        job.running = true;
        job.dto.status = Status::Downloading;
        let mut file = item(
            1,
            FileKind::Audio,
            "Track".into(),
            &track("1", "album", 1, 1),
            Some(1),
        );
        file.status = Status::Downloading;
        file.phase = FilePhase::Connecting;
        job.dto.files.push(file);
        queue
    }

    #[test]
    fn artwork_rows_follow_the_delivered_release_without_changing_audio_or_booklet_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let mut settings = Settings::default();
        settings.output.folder_template =
            "%album artist%/[%bit depth% Bit]/Disc %disc number%/%album name%".into();
        let mut tracks = Vec::new();
        for (id, album, disc) in [("1", "001", 1), ("2", "001", 2), ("3", "002", 2)] {
            let mut entry = track(id, album, disc, 1);
            let metadata = Arc::make_mut(&mut entry.album);
            metadata["artist"] = serde_json::json!({"name":"Artist"});
            metadata["media_count"] = serde_json::json!(2);
            tracks.push(entry);
        }
        let plan = Arc::new(ResolvedResource {
            title: "Releases".into(),
            tracks,
            thumbnail: None,
            thumbnail_url: None,
        });
        let mut queue = Queue::default();
        queue.enqueue(
            vec![normalize_link("https://play.qobuz.com/artist/123").unwrap()],
            &settings,
            "Waiting",
        );
        let task = RunTask {
            id: 1,
            generation: 0,
            settings: settings.clone(),
            root: root.clone(),
            plan: plan.clone(),
            cancel: CancellationToken::new(),
            audio: vec![0, 1, 2],
            only_file: None,
            account_epoch: 0,
            search_origin: false,
            kind: ResourceKind::Artist,
            replacement: None,
            mode: DownloadMode::Sequential,
        };
        let job = queue.jobs.get_mut(&1).unwrap();
        job.plan = Some(plan.clone());
        plan_assets(job, &task);
        let cover_rows = |job: &Job| {
            job.extras
                .iter()
                .filter_map(|(id, extra)| match extra {
                    Extra::Artwork {
                        track,
                        directory,
                        final_target,
                        ..
                    } => Some((*id, *track, directory.clone(), *final_target)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let initial = cover_rows(job);
        assert_eq!(initial.len(), 2);
        assert!(initial.iter().all(|(_, _, _, final_target)| !final_target));
        let old_row = initial.iter().find(|(_, index, _, _)| *index == 0).unwrap();
        let (audio, layout) = media::plan_output(
            &root,
            &settings.output,
            &plan.tracks[0],
            &DeliveredQuality::requested(2),
            None,
            1,
        )
        .unwrap();
        let cover_dir = layout.cover_dir.clone().unwrap();
        assert_ne!(cover_dir, old_row.2);
        assert!(audio.parent().unwrap().ends_with("16 Bit/Disc 1/Album"));
        assert!(cover_dir.ends_with("16 Bit/Album [001]"));
        let release_root = layout.album_root.clone();
        job.pinned.insert("001".into(), layout);
        plan_assets(job, &task);
        let final_rows = cover_rows(job);
        assert!(final_rows.contains(&(old_row.0, 0, cover_dir.clone(), true)));
        assert_eq!(final_rows.len(), 2);
        assert!(job.extras.values().any(|extra| matches!(extra,
            Extra::Booklet { track:0, directory, .. } if directory == &release_root)));
        let mut retry = RunTask { settings, ..task };
        retry.settings.output.cover_filename = "Different".into();
        plan_assets(job, &retry);
        assert!(
            matches!(job.extras.get(&old_row.0), Some(Extra::Artwork { basename, directory, .. })
            if basename == "cover" && directory == &cover_dir)
        );
        let cover_path = cover_dir.join("cover.jpg");
        update_file(job, old_row.0, |file| {
            complete(file, cover_path.clone(), None, Some(1))
        });
        let audio_id = next_file(job);
        let mut row = item(
            audio_id,
            FileKind::Audio,
            "Song".into(),
            &plan.tracks[0],
            Some(1),
        );
        complete(&mut row, audio.clone(), None, Some(1));
        job.dto.files.push(row);
        job.missing_embedded.extend([0, 1]);
        refresh_cover_warnings(job, &plan, "001");
        assert_eq!(
            job.dto
                .files
                .iter()
                .find(|file| file.id == old_row.0)
                .unwrap()
                .reason
                .as_deref(),
            Some("2 finalized tracks still lack embedded artwork.")
        );
        plan_assets(job, &retry);
        assert!(job.dto.files.iter().any(|file| file.id == old_row.0
            && file.status == Status::Completed
            && file.path.as_ref() == Some(&cover_path)));
        job.phase = Phase::Finished;
        job.dto.status = Status::Incomplete;
        assert_eq!(
            queue.release_folder_evidence(1).unwrap(),
            [(audio, release_root)]
        );
    }

    #[test]
    fn search_parent_only_removes_covered_pending_children_after_admission() {
        let settings = Settings::default();
        let mut queue = Queue::default();
        let tracks = (1..=4).map(|id| normalize_link(&format!("https://play.qobuz.com/track/{id}")).unwrap()).collect();
        queue.enqueue(tracks, &settings, "Waiting");
        queue.jobs.get_mut(&2).unwrap().phase = Phase::Active;
        queue.jobs.get_mut(&3).unwrap().phase = Phase::Finished;
        queue.enqueue_search(
            vec![normalize_link("https://play.qobuz.com/album/album").unwrap()],
            &settings, "Waiting", HashSet::from(["album".into()]), HashMap::new(),
        );
        let parent = queue.jobs.get_mut(&5).unwrap();
        parent.plan = Some(Arc::new(ResolvedResource {
            title: "Album".into(), thumbnail: None, thumbnail_url: None,
            tracks: (1..=3).map(|id| track(&id.to_string(), "album", 1, id)).collect(),
        }));
        parent.phase = Phase::Held;
        assert!(!queue.apply_search_coverage());
        assert_eq!(queue.jobs.len(), 5);
        queue.jobs.get_mut(&5).unwrap().phase = Phase::Eligible;
        assert!(queue.apply_search_coverage());
        assert_eq!(queue.jobs.keys().copied().collect::<Vec<_>>(), [2, 3, 4, 5]);
    }

    #[test]
    fn release_folder_uses_saved_layouts_and_rejects_stale_or_unknown_jobs() {
        let mut queue = active_queue();
        assert!(queue.release_folder_evidence(1).is_err());
        assert!(queue.release_folder_evidence(999).is_err());
        let job = queue.jobs.get_mut(&1).unwrap();
        job.phase = Phase::Finished;
        job.running = false;
        job.dto.status = Status::Completed;
        job.dto.files[0].status = Status::Completed;
        job.dto.files[0].path = Some(PathBuf::from("C:/saved/album/Disc 1/track.flac"));
        assert!(queue.release_folder_evidence(1).is_err());
        let job = queue.jobs.get_mut(&1).unwrap();
        job.pinned.insert(
            "album".into(),
            ReleaseLayout {
                album_root: PathBuf::from("C:/saved/album"),
                cover_dir: None,
                disc_dirs: BTreeMap::new(),
            },
        );
        job.settings.output_location = None;
        assert_eq!(
            queue.release_folder_evidence(1).unwrap()[0].1,
            PathBuf::from("C:/saved/album")
        );
        queue.jobs.get_mut(&1).unwrap().dto.status = Status::Incomplete;
        assert!(queue.release_folder_evidence(1).is_ok());
        queue.jobs.get_mut(&1).unwrap().dto.status = Status::Failed;
        assert!(queue.release_folder_evidence(1).is_err());
    }

    #[test]
    fn mixed_windows_paths_reach_the_real_filesystem_resolver() {
        let temp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap().join("Album");
        std::fs::create_dir_all(root.join("Disc 1")).unwrap();
        let mut q = active_queue();
        let job = q.jobs.get_mut(&1).unwrap();
        job.phase = Phase::Finished;
        job.running = false;
        job.dto.status = Status::Completed;
        job.dto.files.clear();
        for (i, name) in [
            "Disc 1/track.flac",
            "Disc 1/track.lrc",
            "cover.jpg",
            "booklet.pdf",
        ]
        .iter()
        .enumerate()
        {
            let path = root.join(name);
            std::fs::write(&path, b"saved").unwrap();
            let mut file = item(
                i as u64 + 1,
                FileKind::Audio,
                name.to_string(),
                &track("1", "album", 1, 1),
                Some(1),
            );
            file.kind = [
                FileKind::Audio,
                FileKind::Lyrics,
                FileKind::Artwork,
                FileKind::Booklet,
            ][i];
            file.status = Status::Completed;
            file.path = Some(std::fs::canonicalize(path).unwrap());
            job.dto.files.push(file);
        }
        job.pinned.insert(
            "album".into(),
            ReleaseLayout {
                album_root: root.clone(),
                cover_dir: None,
                disc_dirs: BTreeMap::new(),
            },
        );
        let evidence = q
            .release_folder_evidence(1)
            .expect("canonical saved output must match planned ordinary root");
        assert_eq!(
            crate::release_folder::resolve(evidence).unwrap(),
            dunce::canonicalize(&root).unwrap()
        );
        q.jobs.get_mut(&1).unwrap().dto.status = Status::Incomplete;
        assert!(q.release_folder_evidence(1).is_ok());
        std::fs::remove_file(root.join("cover.jpg")).unwrap();
        assert!(crate::release_folder::resolve(q.release_folder_evidence(1).unwrap()).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_prefix_equivalence_retains_component_boundaries() {
        let mut q = active_queue();
        let job = q.jobs.get_mut(&1).unwrap();
        job.phase = Phase::Finished;
        job.running = false;
        job.dto.status = Status::Completed;
        job.dto.files[0].status = Status::Completed;
        for (file, root, ok) in [
            (
                r"\\?\E:\Music\Album\Disc 1\track.flac",
                r"E:\Music\Album",
                true,
            ),
            (r"E:\Music\Album\track.flac", r"\\?\E:\Music\Album", true),
            (
                r"\\?\UNC\server\share\Album\track.flac",
                r"\\server\share\Album",
                true,
            ),
            (
                r"\\server\share\Album\track.flac",
                r"\\?\UNC\server\share\Album",
                true,
            ),
            (r"E:\Music\Album2\track.flac", r"E:\Music\Album", false),
            (r"F:\Music\Album\track.flac", r"E:\Music\Album", false),
            (
                r"\\?\UNC\server\share2\Album\track.flac",
                r"\\server\share\Album",
                false,
            ),
        ] {
            let job = q.jobs.get_mut(&1).unwrap();
            job.dto.files[0].path = Some(file.into());
            job.pinned.clear();
            job.pinned.insert(
                "album".into(),
                ReleaseLayout {
                    album_root: root.into(),
                    cover_dir: None,
                    disc_dirs: BTreeMap::new(),
                },
            );
            assert_eq!(q.release_folder_evidence(1).is_ok(), ok, "{file} / {root}");
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "opens Explorer for an isolated completed-release fixture"]
    fn native_completed_release_folder_action() {
        let root =
            PathBuf::from(std::env::var_os("SUONELO_FOLDER_SMOKE").expect("isolated test folder"));
        std::fs::create_dir_all(root.join("Disc 1")).unwrap();
        let saved = root.join("Disc 1/round7-track.flac");
        std::fs::write(&saved, b"fixture").unwrap();
        let mut q = active_queue();
        let job = q.jobs.get_mut(&1).unwrap();
        job.phase = Phase::Finished;
        job.running = false;
        job.dto.status = Status::Completed;
        job.dto.files[0].status = Status::Completed;
        job.dto.files[0].path = Some(std::fs::canonicalize(saved).unwrap());
        job.pinned.insert(
            "album".into(),
            ReleaseLayout {
                album_root: root.clone(),
                cover_dir: None,
                disc_dirs: BTreeMap::new(),
            },
        );
        let folder = crate::release_folder::resolve(q.release_folder_evidence(1).unwrap()).unwrap();
        assert_eq!(folder, dunce::canonicalize(root).unwrap());
        crate::release_folder::open(&folder).unwrap();
    }

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../tests/fixtures/implementation-fixtures.json"
        ))
        .unwrap()
    }

    #[test]
    fn auxiliary_files_do_not_determine_completion() {
        assert!(aggregate(0, 0) == Status::Failed);
        assert!(aggregate(0, 2) == Status::Failed);
        assert!(aggregate(1, 2) == Status::Incomplete);
        assert!(aggregate(2, 2) == Status::Completed);
    }

    #[test]
    fn progress_deltas_use_the_queue_ipc_fields() {
        let event = QueueEvent::Progress {
            revision: 5,
            job_id: 2,
            file_id: 3,
            generation: 1,
            attempt: 3,
            progress: Some(28),
            phase: FilePhase::Transferring,
            received_bytes: 28,
            total_bytes: Some(100),
            final_size_bytes: None,
            job_progress: Some(28),
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"revision": 5, "jobId": 2, "fileId": 3, "generation": 1,
                "attempt":3, "progress": 28, "phase":"transferring", "receivedBytes":28,
                "totalBytes":100, "finalSizeBytes":null, "jobProgress":28})
        );
    }

    #[test]
    fn large_submission_confirmation_covers_all_jobs_and_rejects_an_old_count() {
        let links = [
            "https://open.qobuz.com/playlist/100",
            "https://open.qobuz.com/playlist/101",
        ]
        .into_iter()
        .map(|url| normalize_link(url).unwrap())
        .collect();
        let mut queue = Queue::default();
        queue.enqueue(links, &Settings::default(), "Waiting");
        for (job, count) in queue.jobs.values_mut().zip([110, 20]) {
            job.phase = Phase::Held;
            let source = track("1", "album", 1, 1);
            job.dto.files.extend(
                (1..=count).map(|id| item(id, FileKind::Audio, "Track".into(), &source, None)),
            );
        }
        queue.reconsider(1);
        queue.changed();
        let old = queue.snapshot().confirmations.pop().unwrap();
        assert_eq!(old.track_count, 130);
        queue.cancel(2, true).unwrap();
        let next = queue.snapshot().confirmations.pop().unwrap();
        assert_eq!(next.track_count, 110);
        assert!(queue.decide(old.id, old.plan_revision, true).is_err());
        queue.decide(next.id, next.plan_revision, false).unwrap();
        assert!(
            queue
                .jobs
                .values()
                .all(|job| job.dto.status == Status::Cancelled)
        );
        assert!(queue.snapshot().confirmations.is_empty());
    }

    #[test]
    fn retry_preserves_job_mode_and_resolved_selection_while_capturing_compatible_settings() {
        let mut original = Settings::default();
        original.output.booklet_only = true;
        original.output.limited_to_album_releases = true;
        let mut queue = Queue::default();
        queue.enqueue(
            vec![normalize_link("https://qobuz.com/album/album").unwrap()],
            &original,
            "Waiting",
        );
        let plan = Arc::new(ResolvedResource {
            title: "Release".into(),
            tracks: vec![track("1", "album", 1, 1)],
            thumbnail_url: None,
            thumbnail: None,
        });
        let job = queue.jobs.get_mut(&1).unwrap();
        job.phase = Phase::Finished;
        job.dto.status = Status::Failed;
        plan_booklet_rows(job, &plan);
        job.plan = Some(plan.clone());
        let mut current = Settings::default();
        current.output.quality = 2;
        current.output.smart_discography = false;
        current.max_connections = crate::storage::MaxConnections::Count(2);
        queue.retry(1, &current, "Waiting", None).unwrap();
        let job = &queue.jobs[&1];
        assert!(job.settings.output.booklet_only);
        assert!(job.settings.output.limited_to_album_releases);
        assert!(job.settings.output.smart_discography);
        assert_eq!(job.settings.output.quality, 2);
        assert_eq!(job.settings.max_connections.limit(), 2);
        assert!(Arc::ptr_eq(job.plan.as_ref().unwrap(), &plan));
    }

    #[test]
    fn pdf_confirmation_deduplicates_repeated_releases_across_a_submission() {
        let mut settings = Settings::default();
        settings.output.booklet_only = true;
        let mut queue = Queue::default();
        queue.enqueue(
            ["100", "101"]
                .map(|id| normalize_link(&format!("https://qobuz.com/playlist/{id}")).unwrap())
                .into(),
            &settings,
            "Waiting",
        );
        let tracks = (0..100)
            .map(|index| track(&index.to_string(), &format!("album{index}"), 1, 1))
            .collect();
        let plan = Arc::new(ResolvedResource {
            title: "Set".into(),
            tracks,
            thumbnail_url: None,
            thumbnail: None,
        });
        for job in queue.jobs.values_mut() {
            plan_booklet_rows(job, &plan);
            job.plan = Some(plan.clone());
            job.phase = Phase::Held;
        }
        queue.reconsider(1);
        let confirmation = queue.snapshot().confirmations.pop().unwrap();
        assert_eq!(confirmation.unit, "pdfs");
        assert_eq!(confirmation.track_count, 100);
    }

    #[test]
    fn missing_names_keep_the_original_release_sequence_and_repeated_identity() {
        let plan = ResolvedResource {
            title: "Set".into(),
            thumbnail_url: None,
            thumbnail: None,
            tracks: vec![
                track("1", "a", 1, 0),
                track("2", "a", 1, 0),
                track("3", "a", 1, 0),
                track("4", "b", 1, 0),
                track("2", "a", 1, 0),
                track("5", "b", 2, 0),
            ],
        };
        assert_eq!(
            (0..plan.tracks.len())
                .map(|index| fallback_sequence(&plan, index))
                .collect::<Vec<_>>(),
            [1, 2, 3, 1, 2, 1]
        );
    }

    #[test]
    fn applying_a_mode_change_invalidates_pending_confirmation_and_keeps_active_settings() {
        let mut queue = Queue::default();
        queue.enqueue(
            vec![normalize_link("https://qobuz.com/album/1").unwrap()],
            &Settings::default(),
            "Waiting",
        );
        queue.enqueue(
            vec![normalize_link("https://qobuz.com/album/2").unwrap()],
            &Settings::default(),
            "Waiting",
        );
        queue.jobs.get_mut(&1).unwrap().phase = Phase::Held;
        queue.jobs.get_mut(&2).unwrap().phase = Phase::Active;
        let mut settings = Settings::default();
        settings.output.booklet_only = true;
        settings.max_connections = crate::storage::MaxConnections::Count(8);
        queue.apply(None, &settings);
        assert!(queue.jobs[&1].dto.mode == JobMode::Booklet);
        assert!(queue.jobs[&1].phase == Phase::Waiting);
        assert!(queue.jobs[&2].dto.mode == JobMode::Audio);
        assert_eq!(queue.jobs[&2].settings.max_connections.limit(), 1);
    }

    #[test]
    fn sequential_head_uses_visible_order_across_pending_and_removed_jobs() {
        let mut queue = Queue::default();
        let mut settings = Settings::default();
        let recovery = crate::recovery::Recovery::default();
        queue.enqueue(
            (1..=3)
                .map(|id| normalize_link(&format!("https://qobuz.com/album/{id}")).unwrap())
                .collect(),
            &settings,
            "Waiting",
        );
        queue.jobs.get_mut(&2).unwrap().phase = Phase::Eligible;
        assert_eq!(queue.admission_head(), Some(1));
        queue.jobs.get_mut(&1).unwrap().phase = Phase::Held;
        assert_eq!(queue.admission_head(), Some(1));
        queue.cancel(1, false).unwrap();
        assert_eq!(queue.admission_head(), Some(2));
        queue.jobs.get_mut(&2).unwrap().phase = Phase::Active;
        queue.admitted.insert(2);
        queue.set_user_pause(true, &recovery);
        settings.download_mode = DownloadMode::Concurrent;
        queue.settings_saved(&settings, false, &recovery);
        assert_eq!(queue.snapshot().effective_mode, DownloadMode::Sequential);
        assert_eq!(queue.snapshot().requested_mode, DownloadMode::Concurrent);
        assert!(queue.snapshot().paused);
        settings.download_mode = DownloadMode::Sequential;
        queue.settings_saved(&settings, false, &recovery);
        assert_eq!(
            queue.snapshot().requested_mode,
            queue.snapshot().effective_mode
        );
        settings.download_mode = DownloadMode::Concurrent;
        queue.settings_saved(&settings, false, &recovery);
        queue.cancel(2, true).unwrap();
        assert!(queue.active());
        assert_eq!(queue.admission_head(), Some(3));
        queue.finish_admission(2, &recovery);
        assert_eq!(queue.snapshot().effective_mode, DownloadMode::Concurrent);
        queue.cancel(3, true).unwrap();
        queue.reset_idle_pause(&recovery);
        assert!(!queue.snapshot().paused);
    }

    #[test]
    fn saved_limit_is_distinct_from_the_mixed_live_queue_until_explicit_save() {
        let mut settings = Settings {
            download_mode: DownloadMode::Concurrent,
            ..Settings::default()
        };
        let recovery = crate::recovery::Recovery::default();
        let mut queue = Queue::new(&settings);
        queue.enqueue(
            vec![normalize_link("https://qobuz.com/album/a").unwrap()],
            &settings,
            "Waiting",
        );
        queue.jobs.get_mut(&1).unwrap().phase = Phase::Active;
        queue.admitted.insert(1);
        settings.max_connections = MaxConnections::Count(4);
        queue.settings_saved(&settings, false, &recovery);
        assert_eq!(queue.snapshot().live_limit, Some(MaxConnections::Count(1)));
        queue.enqueue(
            vec![normalize_link("https://qobuz.com/album/b").unwrap()],
            &settings,
            "Waiting",
        );
        assert_eq!(queue.snapshot().live_limit, None);
        queue.apply(None, &settings);
        assert_eq!(queue.snapshot().live_limit, None);
        queue.settings_saved(&settings, true, &recovery);
        assert_eq!(queue.snapshot().live_limit, Some(MaxConnections::Count(4)));
        assert_eq!(
            queue.jobs[&1].settings.max_connections,
            MaxConnections::Count(4)
        );
    }

    #[test]
    fn groups_follow_first_occurrence_and_keep_playlist_indices() {
        let plan = ResolvedResource {
            title: "Playlist".into(),
            tracks: vec![
                track("1", "z", 1, 1),
                track("2", "a", 1, 1),
                track("3", "z", 2, 1),
            ],
            thumbnail_url: None,
            thumbnail: None,
        };
        assert_eq!(release_groups(&plan), [("z", vec![0, 2]), ("a", vec![1])]);
    }

    #[tokio::test]
    async fn deferred_audio_waits_for_the_first_pass_then_replays_once_in_plan_order() {
        use futures_util::poll;
        use std::cell::RefCell;
        let started = RefCell::new(Vec::new());
        let replayed = RefCell::new(Vec::new());
        let (first, first_done) = tokio::sync::oneshot::channel();
        let (second, second_done) = tokio::sync::oneshot::channel();
        let (third, third_done) = tokio::sync::oneshot::channel();
        let pending = stream::iter([first_done, second_done, third_done].into_iter().enumerate())
            .map(|(index, settled)| {
                let started = &started;
                async move {
                    started.borrow_mut().push(index + 1);
                    settled.await.unwrap()
                }
            })
            .buffer_unordered(3);
        let cancel = CancellationToken::new();
        let mut stage = std::pin::pin!(settle_audio_pass(pending, vec![0], &cancel, |index| {
            replayed.borrow_mut().push(index);
            std::future::ready(())
        }));
        assert!(poll!(&mut stage).is_pending());
        assert_eq!(*started.borrow(), [1, 2, 3]);
        third.send(Some(3)).unwrap();
        first.send(Some(1)).unwrap();
        assert!(poll!(&mut stage).is_pending());
        assert!(replayed.borrow().is_empty());
        second.send(None).unwrap();
        stage.await;
        assert_eq!(*replayed.borrow(), [0, 1, 3]);
    }

    #[tokio::test]
    async fn active_output_stage_waits_for_drive_retry_without_releasing_queue_ownership() {
        use futures_util::poll;
        use std::pin::pin;
        let directory = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(directory.path()).unwrap();
        let app = App::initialize_root(root.clone()).unwrap();
        let settings = Settings::default();
        let mut queue = active_queue();
        queue.admitted.insert(1);
        let job = queue.jobs.get_mut(&1).unwrap();
        let plan = Arc::new(ResolvedResource {
            title: "Release".into(),
            tracks: vec![track("1", "album", 1, 1)],
            thumbnail_url: None,
            thumbnail: None,
        });
        job.plan = Some(plan.clone());
        let task = RunTask {
            id: 1,
            generation: 0,
            settings: settings.clone(),
            root: root.clone(),
            plan,
            cancel: job.cancel.clone(),
            audio: vec![0],
            only_file: None,
            account_epoch: 0,
            search_origin: false,
            kind: ResourceKind::Album,
            replacement: None,
            mode: DownloadMode::Sequential,
        };
        app.state().queue = queue;
        let transfer = transfer_context(&app, &task, 1);
        let mut attempts = 0;
        let mut run = pin!(run_file_stage(&app, &task, 1, &transfer, || {
            attempts += 1;
            std::future::ready(if attempts == 1 {
                Err(media::MediaError {
                    code: "disk_full",
                    message: "Disk full".into(),
                    retryable: true,
                    path: Some(root.clone()),
                })
            } else {
                Ok(())
            })
        }));
        assert!(poll!(&mut run).is_pending());
        {
            let mut state = app.state();
            assert!(state.queue.snapshot().jobs[0].drive_blocked);
            assert!(state.queue.active());
            state.queue.retry(1, &settings, "Retry", None).unwrap();
        }
        schedule(&app);
        assert!(run.await.is_ok());
        assert!(!app.state().queue.snapshot().jobs[0].drive_blocked);
    }

    #[test]
    fn a_pinned_album_uses_its_established_drive_when_current_destination_differs() {
        let mut queue = Queue::default();
        queue.enqueue(
            vec![normalize_link("https://qobuz.com/album/a").unwrap()],
            &Settings::default(),
            "Waiting",
        );
        let job = queue.jobs.get_mut(&1).unwrap();
        job.plan = Some(Arc::new(ResolvedResource {
            title: "Release".into(),
            tracks: vec![track("1", "a", 1, 1)],
            thumbnail_url: None,
            thumbnail: None,
        }));
        let pinned = PathBuf::from("C:\\Music\\Album");
        job.pinned.insert(
            "a".into(),
            ReleaseLayout {
                album_root: pinned.clone(),
                cover_dir: None,
                disc_dirs: BTreeMap::new(),
            },
        );
        assert_eq!(
            write_destinations(job, Path::new("D:\\New Destination"), &[0]),
            [pinned]
        );
    }

    #[test]
    fn dialog_quality_follows_the_accepted_retry_apply_and_again_precedence() {
        let fixture = fixture();
        let lifecycle = &fixture["qualityLifecycle"];
        let mut settings = Settings::default();
        settings.output.quality = lifecycle["initialGlobalTier"].as_u64().unwrap() as u8;
        let mut queue = Queue::default();
        queue.enqueue_with_quality(
            vec![normalize_link("https://qobuz.com/album/1").unwrap()],
            &settings,
            "Waiting",
            Some(lifecycle["dialogTier"].as_u64().unwrap() as u8),
        );
        assert_eq!(settings.output.quality, 4);
        settings.output.quality = lifecycle["globalTierAfterSubmission"].as_u64().unwrap() as u8;
        {
            let job = queue.jobs.get_mut(&1).unwrap();
            job.phase = Phase::Finished;
            job.dto.status = Status::Failed;
        }
        queue.retry(1, &settings, "Waiting", None).unwrap();
        assert_eq!(
            u64::from(queue.jobs[&1].settings.output.quality),
            lifecycle["expectedOriginalJobAfterRetry"].as_u64().unwrap()
        );
        queue.apply(Some(vec![1]), &settings);
        assert_eq!(
            u64::from(queue.jobs[&1].settings.output.quality),
            lifecycle["expectedQueuedJobAfterExplicitApply"]
                .as_u64()
                .unwrap()
        );
        {
            let job = queue.jobs.get_mut(&1).unwrap();
            job.phase = Phase::Finished;
            job.dto.status = Status::Failed;
        }
        queue.retry(1, &settings, "Waiting", None).unwrap();
        assert_eq!(
            u64::from(queue.jobs[&1].settings.output.quality),
            lifecycle["expectedRetryAfterApply"].as_u64().unwrap()
        );
        {
            let job = queue.jobs.get_mut(&1).unwrap();
            job.phase = Phase::Finished;
            job.dto.status = Status::Failed;
        }
        queue.download_again(1, &settings, "Waiting").unwrap();
        assert_eq!(
            u64::from(queue.jobs[&2].settings.output.quality),
            lifecycle["expectedDownloadAgain"].as_u64().unwrap()
        );
        assert_eq!(queue.jobs[&2].quality_override, None);
    }

    #[test]
    fn same_percentage_bytes_unknown_totals_and_stale_attempts_match_reference_telemetry() {
        let fixture = fixture();
        let mut queue = active_queue();
        let zero = &fixture["telemetry"][0];
        queue.progress(
            1,
            0,
            1,
            TransferProgress {
                attempt: 1,
                phase: TransferPhase::Transferring,
                received: zero["receivedBytes"].as_u64().unwrap(),
                total: zero["totalBytes"].as_u64(),
            },
        );
        assert_eq!(queue.snapshot().jobs[0].files[0].progress, Some(0));
        let same_percent = &fixture["telemetry"][2];
        for received in same_percent["receivedSequenceBytes"].as_array().unwrap() {
            let previous_revision = queue.revision;
            queue.progress(
                1,
                0,
                1,
                TransferProgress {
                    attempt: 1,
                    phase: TransferPhase::Transferring,
                    received: received.as_u64().unwrap(),
                    total: same_percent["totalBytes"].as_u64(),
                },
            );
            let snapshot = queue.snapshot();
            assert_eq!(snapshot.jobs[0].files[0].progress, Some(42));
            assert_eq!(
                snapshot.jobs[0].files[0].received_bytes,
                received.as_u64().unwrap()
            );
            assert!(snapshot.revision > previous_revision);
        }
        queue.progress(
            1,
            0,
            1,
            TransferProgress {
                attempt: 2,
                phase: TransferPhase::Connecting,
                received: 0,
                total: None,
            },
        );
        let unknown = &fixture["telemetry"][1];
        queue.progress(
            1,
            0,
            1,
            TransferProgress {
                attempt: 2,
                phase: TransferPhase::Transferring,
                received: unknown["receivedBytes"].as_u64().unwrap(),
                total: None,
            },
        );
        let before = queue.revision;
        queue.progress(
            1,
            0,
            1,
            TransferProgress {
                attempt: 1,
                phase: TransferPhase::Processing,
                received: 100_000_000,
                total: Some(100_000_000),
            },
        );
        queue.progress(
            1,
            8,
            1,
            TransferProgress {
                attempt: 3,
                phase: TransferPhase::Processing,
                received: 100_000_000,
                total: Some(100_000_000),
            },
        );
        assert_eq!(queue.revision, before);
        let file = &queue.snapshot().jobs[0].files[0];
        assert_eq!(file.attempt, 2);
        assert_eq!(file.progress, None);
        assert_eq!(file.received_bytes, 11_400_000);
    }

    #[test]
    fn release_completion_waits_for_processing_all_requested_files_and_terminal_job_state() {
        let mut queue = active_queue();
        queue.progress(
            1,
            0,
            1,
            TransferProgress {
                attempt: 1,
                phase: TransferPhase::Processing,
                received: 100,
                total: Some(100),
            },
        );
        assert_eq!(queue.snapshot().jobs[0].progress, Some(99));
        let job = queue.jobs.get_mut(&1).unwrap();
        complete(
            &mut job.dto.files[0],
            PathBuf::from("track.flac"),
            None,
            Some(100),
        );
        let source = track("1", "album", 1, 1);
        let mut cover = item(2, FileKind::Artwork, "Cover".into(), &source, None);
        complete(&mut cover, PathBuf::from("cover.jpg"), None, Some(20));
        job.dto.files.push(cover);
        job.dto
            .files
            .push(item(3, FileKind::Booklet, "Booklet".into(), &source, None));
        assert_eq!(release_progress(job), Some(66));
        complete(
            &mut job.dto.files[2],
            PathBuf::from("booklet.pdf"),
            None,
            Some(30),
        );
        assert_eq!(release_progress(job), Some(99));
        job.phase = Phase::Finished;
        job.dto.status = Status::Completed;
        assert_eq!(release_progress(job), Some(100));
        fail(&mut job.dto.files[2], "Booklet failed".into());
        assert_eq!(release_progress(job), Some(66));
    }

    #[test]
    fn release_progress_reserves_optional_slots_and_discards_partial_attempts() {
        let mut queue = active_queue();
        let source = track("1", "album", 1, 1);
        let job = queue.jobs.get_mut(&1).unwrap();
        complete(
            &mut job.dto.files[0],
            PathBuf::from("track.flac"),
            None,
            Some(100),
        );
        let mut second = item(2, FileKind::Audio, "Track 2".into(), &source, Some(1));
        second.status = Status::Downloading;
        job.dto.files.push(second);
        job.playlist_reserved = true;
        queue.progress(
            1,
            0,
            2,
            TransferProgress {
                attempt: 2,
                phase: TransferPhase::Transferring,
                received: 50,
                total: Some(100),
            },
        );
        assert_eq!(queue.snapshot().jobs[0].progress, Some(50));
        let job = queue.jobs.get_mut(&1).unwrap();
        job.playlist_reserved = false;
        job.dto.files.push(item(
            3,
            FileKind::Playlist,
            "Playlist".into(),
            &source,
            None,
        ));
        assert_eq!(release_progress(job), Some(50));
        job.dto.files.pop();
        queue.progress(
            1,
            0,
            2,
            TransferProgress {
                attempt: 2,
                phase: TransferPhase::Transferring,
                received: 80,
                total: Some(100),
            },
        );
        assert_eq!(queue.snapshot().jobs[0].progress, Some(90));
        queue.progress(
            1,
            0,
            2,
            TransferProgress {
                attempt: 3,
                phase: TransferPhase::Connecting,
                received: 0,
                total: None,
            },
        );
        assert_eq!(queue.snapshot().jobs[0].progress, Some(50));
        queue.progress(
            1,
            0,
            2,
            TransferProgress {
                attempt: 2,
                phase: TransferPhase::Transferring,
                received: 95,
                total: Some(100),
            },
        );
        assert_eq!(queue.snapshot().jobs[0].progress, Some(50));
        let job = queue.jobs.get_mut(&1).unwrap();
        for id in 2..=3 {
            if id == 3 {
                job.dto.files.push(item(
                    id,
                    FileKind::Audio,
                    "Track 3".into(),
                    &source,
                    Some(1),
                ));
            }
            update_file(job, id, |file| {
                complete(file, PathBuf::from("track.flac"), None, Some(100))
            });
        }
        let mut booklet = item(4, FileKind::Booklet, "Booklet".into(), &source, None);
        booklet.status = Status::Downloading;
        job.dto.files.push(booklet);
        assert_eq!(release_progress(job), Some(75));
        queue.progress(
            1,
            0,
            4,
            TransferProgress {
                attempt: 4,
                phase: TransferPhase::Transferring,
                received: 60,
                total: Some(100),
            },
        );
        assert_eq!(queue.snapshot().jobs[0].progress, Some(90));
    }
    #[test]
    fn rate_counts_concurrent_network_deltas_and_returns_to_zero_after_the_window() {
        let fixture = fixture();
        let telemetry = &fixture["telemetry"][3];
        let now = Instant::now();
        let mut window = RateWindow::default();
        for bytes in telemetry["activeTransferDeltasBytes"].as_array().unwrap() {
            window.record(now, bytes.as_u64().unwrap());
        }
        assert_eq!(
            window.bytes_per_second(now + Duration::from_millis(999)),
            telemetry["expectedBytesPerSecond"].as_u64().unwrap()
        );
        assert_eq!(
            window.bytes_per_second(now + Duration::from_millis(1000)),
            0
        );
        let mut queue = active_queue();
        complete(
            &mut queue.jobs.get_mut(&1).unwrap().dto.files[0],
            PathBuf::from("existing.flac"),
            None,
            Some(9_000_000),
        );
        queue.refresh_rate(now);
        assert_eq!(queue.snapshot().bytes_per_second, 0);
    }

    #[tokio::test]
    async fn final_sizes_come_from_published_files_instead_of_transfer_length() {
        let fixture = fixture();
        let telemetry = &fixture["telemetry"][4];
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("published.flac");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(telemetry["finalSizeBytes"].as_u64().unwrap())
            .unwrap();
        let mut queue = active_queue();
        queue.progress(
            1,
            0,
            1,
            TransferProgress {
                attempt: 1,
                phase: TransferPhase::Transferring,
                received: telemetry["receivedBytes"].as_u64().unwrap(),
                total: telemetry["totalBytes"].as_u64(),
            },
        );
        let size = final_size(&path).await;
        complete(
            &mut queue.jobs.get_mut(&1).unwrap().dto.files[0],
            path,
            None,
            size,
        );
        let file = &queue.snapshot().jobs[0].files[0];
        assert_eq!(file.final_size_bytes, Some(27_250_000));
        assert_eq!(file.received_bytes, 11_400_000);
        assert_eq!(file.total_bytes, Some(27_200_000));
    }

    #[test]
    fn clear_downloads_classifies_under_lock_preserves_active_jobs_and_rejects_late_events() {
        let fixture = fixture();
        let mut queue = Queue {
            next_id: 100,
            ..Queue::default()
        };
        let links = (1..=9)
            .map(|id| normalize_link(&format!("https://qobuz.com/album/{id}")).unwrap())
            .collect();
        queue.enqueue(links, &Settings::default(), "Waiting");
        let source = track("1", "album", 1, 1);
        let mut removed_tokens = Vec::new();
        for case in fixture["selectiveClear"]["jobs"].as_array().unwrap() {
            let id = case["id"].as_u64().unwrap();
            let job = queue.jobs.get_mut(&id).unwrap();
            job.phase = match case["internalPhase"].as_str().unwrap() {
                "Active" => Phase::Active,
                "Eligible" => Phase::Eligible,
                "Planning" => Phase::Planning,
                "Held" => Phase::Held,
                "Finished" => Phase::Finished,
                _ => unreachable!(),
            };
            job.dto
                .files
                .push(item(1, FileKind::Audio, "Track".into(), &source, None));
            if !case["protected"].as_bool().unwrap() {
                removed_tokens.push(job.cancel.clone());
            }
        }
        queue.batches.get_mut(&1).unwrap().confirmation = Some(Confirmation {
            id: 1,
            plan_revision: 1,
            track_count: 100,
            unit: "tracks",
        });
        let report = queue.clear_downloads();
        assert_eq!((report.removed, report.protected), (6, 3));
        assert_eq!(
            queue.jobs.keys().copied().collect::<Vec<_>>(),
            [101, 102, 103]
        );
        assert!(removed_tokens.iter().all(CancellationToken::is_cancelled));
        assert!(queue.jobs.values().all(|job| !job.cancel.is_cancelled()));
        assert!(!queue.snapshot().can_clear);
        assert_eq!(queue.snapshot().protected_count, 3);
        assert!(queue.snapshot().confirmations.is_empty());
        let revision = queue.revision;
        queue.progress(
            105,
            0,
            1,
            TransferProgress {
                attempt: 1,
                phase: TransferPhase::Transferring,
                received: 1,
                total: Some(10),
            },
        );
        assert_eq!(queue.revision, revision);
        assert_eq!(queue.jobs.len(), 3);
    }

    #[test]
    fn clear_keeps_a_queued_job_that_became_active_before_execution_and_preserves_files() {
        let directory = tempfile::tempdir().unwrap();
        let finalized = directory.path().join("saved.flac");
        std::fs::write(&finalized, b"finalized media").unwrap();
        let mut queue = active_queue();
        queue.jobs.get_mut(&1).unwrap().phase = Phase::Eligible;
        queue.enqueue(
            vec![normalize_link("https://qobuz.com/album/2").unwrap()],
            &Settings::default(),
            "Waiting",
        );
        assert!(queue.snapshot().can_clear);
        queue.jobs.get_mut(&1).unwrap().phase = Phase::Active;
        queue.jobs.get_mut(&2).unwrap().dto.files.push(item(
            1,
            FileKind::Audio,
            "Saved".into(),
            &track("2", "album", 1, 1),
            None,
        ));
        complete(
            &mut queue.jobs.get_mut(&2).unwrap().dto.files[0],
            finalized.clone(),
            None,
            Some(15),
        );
        let report = queue.clear_downloads();
        assert_eq!((report.removed, report.protected), (1, 1));
        assert!(queue.jobs.contains_key(&1));
        assert_eq!(std::fs::read(finalized).unwrap(), b"finalized media");
        queue.shutdown();
        assert!(!queue.snapshot().can_clear);
    }
}
