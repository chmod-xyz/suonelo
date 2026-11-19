#[cfg(target_os = "macos")]
use crate::locations::profile_error;
use crate::{
    error::{AppError, Result},
    media::Media,
    qobuz::{CanonicalResource, Credentials, QobuzClient, ResourceKey, normalize_link},
    queue::{self, Queue, QueueSnapshot},
    storage::{Bookmark, BookmarkDto, Settings, SettingsDto, Storage},
    vault::{AccountDto, Vault},
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration,
};
use tauri::{AppHandle, Emitter, ipc::Channel};
use tokio::sync::Semaphore;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub struct App {
    pub root: PathBuf,
    pub storage: Storage,
    pub media: Media,
    pub qobuz: QobuzClient,
    pub tasks: TaskTracker,
    pub stop: CancellationToken,
    bookmark_workers: Semaphore,
    pub handle: OnceLock<AppHandle>,
    state: Mutex<AppState>,
    _session_lock: File,
}

pub struct AppState {
    pub settings: Settings,
    pub queue: Queue,
    pub bookmarks: BTreeMap<u64, Bookmark>,
    pub(crate) next_bookmark: u64,
    bookmarks_revision: u64,
    bookmark_channels: Vec<Channel<BookmarksSnapshot>>,
    bookmark_requests: BTreeSet<(u64, u64)>,
    bookmark_activity: BTreeMap<u64, i64>,
    pub bookmark_refresh: BTreeSet<u64>,
    pub vault: Vault,
    pub account: Option<Credentials>,
    pub search: crate::search::Session,
    pub fingerprint: Option<String>,
    pub(crate) account_attempt: Option<crate::commands::AccountAttempt>,
    pub validating: bool,
    pub account_error: Option<AppError>,
    pub account_epoch: u64,
    pub account_cancel: CancellationToken,
    pub closing: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookmarksSnapshot {
    revision: u64,
    bookmarks: Vec<BookmarkDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapDto {
    pub settings: SettingsDto,
    pub queue: QueueSnapshot,
    pub bookmarks: Vec<BookmarkDto>,
    pub account: AccountDto,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Destination {
    Downloads,
    Bookmarks,
}

#[derive(Serialize)]
pub struct RejectedLink {
    url: String,
    reason: String,
}

#[derive(Default, Serialize)]
pub struct SubmissionReport {
    accepted: Vec<String>,
    duplicates: Vec<String>,
    invalid: Vec<RejectedLink>,
}

impl App {
    #[cfg(not(target_os = "macos"))]
    pub fn initialize() -> Result<Arc<Self>> {
        let exe = std::env::current_exe().map_err(|_| profile_error())?;
        let root = dunce::canonicalize(exe.parent().ok_or_else(profile_error)?)
            .map_err(|_| profile_error())?;
        Self::initialize_root(root)
    }

    #[cfg(any(not(target_os = "macos"), test))]
    pub(crate) fn initialize_root(root: PathBuf) -> Result<Arc<Self>> {
        Self::initialize_at(root, |data| Vault::new(data.join("credentials.vault")))
    }

    #[cfg(target_os = "macos")]
    pub fn initialize_macos(root: PathBuf, qa_name: Option<&str>) -> Result<Arc<Self>> {
        Self::initialize_at(root, |_| Vault::keychain(qa_name))
    }

    fn initialize_at(root: PathBuf, vault: impl FnOnce(&Path) -> Vault) -> Result<Arc<Self>> {
        let data = root.join("data");
        fs::create_dir_all(&data).map_err(|_| profile_error())?;
        #[cfg(target_os = "macos")]
        let root = dunce::canonicalize(&root).map_err(|_| profile_error())?;
        #[cfg(target_os = "macos")]
        let data = root.join("data");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(data.join("session.lock"))
            .map_err(|_| profile_error())?;
        FileExt::try_lock_exclusive(&lock).map_err(|error| {
            if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                AppError::new(
                    "session_open",
                    "storage",
                    "Another instance of suonelo is already using this profile.",
                    true,
                )
            } else {
                profile_error()
            }
        })?;
        #[cfg(not(target_os = "macos"))]
        let database = crate::profile_migration::prepare(&data)?;
        #[cfg(target_os = "macos")]
        let database = data.join("suonelo.sqlite3");
        let (storage, settings, bookmarks, next_bookmark) = Storage::initialize(database)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("suonelo/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .https_only(true)
            .build()
            .map_err(|_| {
                AppError::new(
                    "http_client",
                    "network",
                    "Could not initialize media networking.",
                    true,
                )
            })?;
        let media = Media::new(client, storage.path.clone(), root.clone());
        let qobuz = QobuzClient::new().map_err(|error| AppError::qobuz(error, "network"))?;
        Ok(Arc::new(Self {
            root,
            storage,
            media,
            qobuz,
            tasks: TaskTracker::new(),
            stop: CancellationToken::new(),
            bookmark_workers: Semaphore::new(4),
            handle: OnceLock::new(),
            _session_lock: lock,
            state: Mutex::new(AppState {
                queue: Queue::new(&settings),
                settings,
                bookmarks,
                next_bookmark,
                bookmarks_revision: 0,
                bookmark_channels: Vec::new(),
                bookmark_requests: BTreeSet::new(),
                bookmark_activity: BTreeMap::new(),
                bookmark_refresh: BTreeSet::new(),
                vault: vault(&data),
                account: None,
                search: crate::search::Session::default(),
                fingerprint: None,
                account_attempt: None,
                validating: false,
                account_error: None,
                account_epoch: 0,
                account_cancel: CancellationToken::new(),
                closing: false,
            }),
        }))
    }

    pub fn state(&self) -> MutexGuard<'_, AppState> {
        self.state.lock().expect("application state mutex poisoned")
    }

    pub async fn start(self: Arc<Self>) {
        #[cfg(target_os = "macos")]
        if !self.restore_keychain(Vault::read_keychain).await {
            return;
        }
        let app = self.clone();
        {
            let state = self.state();
            if state.vault.credentials().is_some() && !state.closing {
                self.tasks.spawn(async move {
                    let _ = crate::commands::validate_saved_account(app).await;
                });
            }
        }
        let _ = self.media.cleanup_stages().await;
        queue::scheduler(self).await;
    }

    #[cfg(target_os = "macos")]
    async fn restore_keychain(&self, read: impl FnOnce(String) -> Vault + Send + 'static) -> bool {
        let account = self.state().vault.pending_keychain_account();
        let Some(account) = account else {
            return true;
        };
        // A rebuilt app may wait for a SecurityAgent prompt. Keep the main
        // thread and state mutex free and let quit discard a pending read.
        let loaded = tokio::select! {
            biased;
            _ = self.stop.cancelled() => return false,
            loaded = async { tokio::task::spawn_blocking(move || read(account)).await } => loaded,
        };
        let mut state = self.state();
        if state.closing {
            return false;
        }
        state.vault.finish_keychain_load(loaded);
        self.emit_account(state.account_status());
        true
    }

    pub fn emit_account(&self, status: AccountDto) {
        if let Some(handle) = self.handle.get() {
            let _ = handle.emit("account-changed", status);
        }
    }

    pub fn reject_runtime_account(&self, epoch: u64) {
        let mut state = self.state();
        if state.account_epoch != epoch || state.closing || state.account.is_none() {
            return;
        }
        state.clear_account();
        state.account_error = Some(AppError::new(
            "invalid_token",
            "account",
            "Qobuz rejected the saved token. Enter a replacement token.",
            false,
        ));
        let reason = state.waiting_reason();
        state.queue.account_changed(reason, true);
        self.emit_account(state.account_status());
    }

    pub fn submit(
        self: &Arc<Self>,
        text: String,
        destination: Destination,
    ) -> Result<SubmissionReport> {
        self.submit_with_options(text, destination, None, false)
    }

    pub fn submit_with_options(
        self: &Arc<Self>,
        text: String,
        destination: Destination,
        quality_override: Option<u8>,
        strict_file: bool,
    ) -> Result<SubmissionReport> {
        if quality_override.is_some_and(|tier| !(1..=4).contains(&tier)) {
            return Err(AppError::invalid(
                "links",
                "Choose a quality tier from 1 through 4.",
            ));
        }
        if text.len() > 2_000_000 {
            return Err(AppError::invalid(
                "links",
                "The pasted link list is too large.",
            ));
        }
        if strict_file {
            crate::link_import::validate_import_snapshot(&text)?;
        }
        let mut report = SubmissionReport::default();
        let mut state = self.state();
        state.ensure_open()?;
        let mut known: HashSet<ResourceKey> = match destination {
            Destination::Downloads => state
                .queue
                .jobs
                .values()
                .map(|job| job.link.key.clone())
                .collect(),
            Destination::Bookmarks => state
                .bookmarks
                .values()
                .map(|bookmark| bookmark.key.clone())
                .collect(),
        };
        let links = text
            .split_whitespace()
            .filter_map(|raw| match normalize_link(raw) {
                Ok(link) if known.insert(link.key.clone()) => {
                    report.accepted.push(raw.to_owned());
                    Some(link)
                }
                Ok(_) => {
                    report.duplicates.push(raw.to_owned());
                    None
                }
                Err(error) => {
                    report.invalid.push(RejectedLink {
                        url: raw.to_owned(),
                        reason: error.message,
                    });
                    None
                }
            })
            .collect::<Vec<_>>();
        let reason = state.waiting_reason();
        match destination {
            Destination::Downloads => {
                let settings = state.settings.clone();
                state
                    .queue
                    .enqueue_with_quality(links, &settings, reason, quality_override);
            }
            Destination::Bookmarks if !links.is_empty() => {
                let bookmarks = links
                    .into_iter()
                    .map(|link| {
                        let id = state.next_bookmark;
                        state.next_bookmark += 1;
                        Bookmark::new(id, link, reason.to_owned())
                    })
                    .collect::<Vec<_>>();
                self.storage.save_bookmarks(&bookmarks)?;
                for bookmark in bookmarks {
                    let id = bookmark.dto.id;
                    state.bookmark_activity.insert(id, crate::search::activity::event_time());
                    state.bookmarks.insert(id, bookmark);
                    self.resolve_bookmark(&mut state, id);
                }
                state.bookmarks_changed();
            }
            _ => (),
        }
        Ok(report)
    }

    pub fn download_bookmarks(self: &Arc<Self>, ids: Option<Vec<u64>>) -> Result<SubmissionReport> {
        let mut state = self.state();
        state.ensure_open()?;
        let mut known: HashSet<ResourceKey> = state
            .queue
            .jobs
            .values()
            .map(|job| job.link.key.clone())
            .collect();
        let mut report = SubmissionReport::default();
        let mut refresh = Vec::new();
        let links = state
            .bookmarks
            .values()
            .filter(|bookmark| {
                ids.as_ref()
                    .is_none_or(|ids| ids.contains(&bookmark.dto.id))
            })
            .filter_map(|bookmark| {
                if !known.insert(bookmark.key.clone()) {
                    report.duplicates.push(bookmark.dto.url.clone());
                    return None;
                }
                if bookmark.fingerprint != state.fingerprint || bookmark.dto.reason.is_some() {
                    refresh.push(bookmark.dto.id);
                }
                report.accepted.push(bookmark.dto.url.clone());
                Some(CanonicalResource {
                    key: bookmark.key.clone(),
                    original_url: bookmark.dto.url.clone(),
                })
            })
            .collect();
        let settings = state.settings.clone();
        let reason = state.waiting_reason();
        state.queue.enqueue(links, &settings, reason);
        for id in refresh {
            self.resolve_bookmark(&mut state, id);
        }
        Ok(report)
    }

    pub fn resolve_bookmark(self: &Arc<Self>, state: &mut AppState, id: u64) {
        let Some(bookmark) = state.bookmarks.get(&id) else {
            return;
        };
        let Some(account) = state
            .account
            .clone()
            .filter(|_| !state.closing)
        else {
            state.bookmark_refresh.insert(id);
            return;
        };
        let Some(fingerprint) = state.fingerprint.clone() else {
            return;
        };
        state.bookmark_refresh.remove(&id);
        let epoch = state.account_epoch;
        if !state.bookmark_requests.insert((id, epoch)) {
            return;
        }
        let key = bookmark.key.clone();
        let activity_at = state.bookmark_activity.get(&id).copied();
        let expand_activity = activity_at.is_some()
            && !matches!(key.kind, crate::qobuz::ResourceKind::Album | crate::qobuz::ResourceKind::Track);
        let app = self.clone();
        let cancel = state.account_cancel.clone();
        self.tasks.spawn(async move {
            let result = tokio::select! {
                _ = cancel.cancelled() => None,
                result = async {
                    let _worker = app.bookmark_workers.acquire().await.ok();
                    let mut summary = app.qobuz.resolve_bookmark_summary(&key, &account).await?;
                    if let Some(url) = &summary.thumbnail_url {
                        summary.thumbnail = app.media.thumbnail(url).await;
                    }
                    let credits = if expand_activity {
                        let resolved = tokio::time::timeout(Duration::from_secs(8), app.qobuz.resolve_resource_for_job(&key, &account, false, false)).await;
                        resolved.ok().and_then(std::result::Result::ok)
                            .map(|plan| plan.tracks.iter().filter_map(|track| {
                                crate::search::activity::Credit::from_track(track, key.kind)
                            }).collect::<HashSet<_>>().into_iter().collect::<Vec<_>>())
                            .unwrap_or_default()
                    } else {
                        summary.activity.iter().cloned().collect::<Vec<_>>()
                    };
                    Ok::<_, crate::qobuz::QobuzError>((summary, credits))
                } => Some(result),
            };
            if result.as_ref().is_some_and(|result| {
                result
                    .as_ref()
                    .is_err_and(|error| error.code == "invalid_token")
            }) {
                app.reject_runtime_account(epoch);
            }
            let writer = app.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let mut state = writer.state();
                state.bookmark_requests.remove(&(id, epoch));
                if state.account_epoch != epoch || state.closing {
                    return;
                }
                let Some(mut next) = state.bookmarks.get(&id).cloned() else {
                    return;
                };
                let Some(result) = result else {
                    return;
                };
                let credits = match result {
                    Ok((summary, credits)) => {
                        next.update(summary, fingerprint);
                        credits
                    }
                    Err(error) => {
                        next.dto.reason = Some(error.message);
                        Vec::new()
                    }
                };
                let saved = if let Some(at) = activity_at {
                    writer.storage.save_bookmarks_with_activity(std::slice::from_ref(&next), &[], &credits, at)
                } else {
                    writer.storage.save_bookmarks([&next])
                };
                if let Err(error) = saved {
                    next.dto.reason = Some(error.message);
                } else if !credits.is_empty() {
                    state.bookmark_activity.remove(&id);
                }
                state.bookmarks.insert(id, next);
                state.bookmarks_changed();
            })
            .await;
        });
    }

    pub async fn shutdown(&self) {
        {
            let mut state = self.state();
            state.closing = true;
            state.queue.shutdown();
            state.clear_account();
            state.vault.clear_memory();
            self.stop.cancel();
            self.tasks.close();
        }
        self.tasks.wait().await;
        let _ = self.media.cleanup_stages().await;
    }
}

#[cfg(test)]
mod submission_tests {
    use super::*;

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn lock_path_io_error_is_storage_failure_before_database_open() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        std::fs::create_dir_all(data.join("session.lock")).unwrap();
        let error = App::initialize_root(directory.path().to_owned()).err().unwrap();
        assert_eq!(error.code, "portable_storage");
        assert!(!data.join("suonelo.sqlite3").exists());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn pending_keychain_read_leaves_state_and_shutdown_responsive() {
        let directory = tempfile::tempdir().unwrap();
        let app = App::initialize_macos(directory.path().to_owned(), Some("keychain-startup-test"))
            .unwrap();
        let fixture_path = directory.path().join("credentials.vault");
        let (started, worker_started) = tokio::sync::oneshot::channel();
        let (unblock, blocked) = std::sync::mpsc::channel();
        let startup_app = app.clone();
        let startup = tokio::spawn(async move {
            startup_app
                .restore_keychain(move |account| {
                    assert_eq!(account, "qa:keychain-startup-test");
                    started.send(()).unwrap();
                    let _ = blocked.recv_timeout(Duration::from_secs(5));
                    Vault::new(fixture_path)
                })
                .await
        });
        worker_started.await.unwrap();
        {
            let mut state = app.state();
            assert_eq!(state.account_status().state, "validating");
            assert_eq!(state.vault.reset().unwrap_err().code, "vault_loading");
            let save = state.vault.save(Credentials::default(), "123".into(), false);
            assert_eq!(save.unwrap_err().code, "vault_loading");
        }
        tokio::time::timeout(Duration::from_secs(1), app.shutdown())
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), startup).await;
        let _ = unblock.send(());
        assert!(!result.unwrap().unwrap());
        assert!(app.state().vault.is_loading());
    }

    #[test]
    fn shared_session_lock_blocks_migration_before_any_database_change() {
        let directory = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(directory.path()).unwrap();
        let data = root.join("data");
        fs::create_dir_all(&data).unwrap();
        let old = data.join("qbdlx.sqlite3");
        fs::write(&old, b"legacy profile must remain untouched").unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(data.join("session.lock"))
            .unwrap();
        FileExt::try_lock_exclusive(&lock).unwrap();
        let error = App::initialize_root(root)
            .err()
            .expect("locked profile must be rejected");
        assert_eq!(error.code, "session_open");
        assert_eq!(
            fs::read(old).unwrap(),
            b"legacy profile must remain untouched"
        );
        assert!(!data.join("suonelo.sqlite3").exists());
        assert!(!data.join("suonelo-migration.json").exists());
    }

    #[tokio::test]
    async fn import_preflight_and_invalid_quality_do_not_mutate_either_destination() {
        let directory = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(directory.path()).unwrap();
        let app = App::initialize_root(root.clone()).unwrap();
        let good = "https://open.qobuz.com/album/import-one";
        let mixed = format!("{good}\nnot-a-url");
        assert!(
            app.submit_with_options(mixed.clone(), Destination::Downloads, Some(2), true)
                .is_err()
        );
        assert!(
            app.submit_with_options(mixed, Destination::Bookmarks, None, true)
                .is_err()
        );
        assert!(app.state().queue.snapshot().jobs.is_empty());
        assert!(app.state().bookmarks.is_empty());
        for tier in [0, 5, 255] {
            assert!(
                app.submit_with_options(good.into(), Destination::Downloads, Some(tier), false)
                    .is_err()
            );
        }
        assert!(app.state().queue.snapshot().jobs.is_empty());

        let path = root.join("import.txt");
        fs::write(&path, good).unwrap();
        let inspected = crate::link_import::inspect_link_file(&path);
        assert!(inspected.valid);
        fs::write(&path, "https://open.qobuz.com/album/external-change").unwrap();
        let captured = inspected.content.unwrap();
        let report = app
            .submit_with_options(captured.clone(), Destination::Downloads, Some(2), true)
            .unwrap();
        assert_eq!(report.accepted, [good]);
        assert_eq!(app.state().settings.output.quality, 4);
        let bookmark_report = app
            .submit_with_options(captured, Destination::Bookmarks, Some(1), true)
            .unwrap();
        assert_eq!(bookmark_report.accepted, [good]);
        let (_, settings, bookmarks, _) = Storage::initialize(app.storage.path.clone()).unwrap();
        assert_eq!(settings.output.quality, 4);
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks.values().next().unwrap().dto.url, good);
        let duplicates = app
            .submit_with_options(good.into(), Destination::Downloads, None, true)
            .unwrap();
        assert!(duplicates.accepted.is_empty());
        assert_eq!(duplicates.duplicates, [good]);
        app.shutdown().await;
    }
}

impl AppState {
    pub fn ensure_open(&self) -> Result<()> {
        if self.closing {
            Err(AppError::new(
                "closing",
                "application",
                "The application is closing.",
                false,
            ))
        } else {
            Ok(())
        }
    }

    pub fn account_status(&self) -> AccountDto {
        let credentials = self.vault.credentials();
        let validating = self.validating;
        #[cfg(target_os = "macos")]
        let validating = validating || self.vault.is_loading();
        let state = if validating {
            "validating"
        } else if self.account.is_some() {
            "ready"
        } else if self
            .account_error
            .as_ref()
            .is_some_and(|error| error.code == "invalid_token")
        {
            "rejected"
        } else if credentials.is_some() {
            "unvalidated"
        } else {
            "missing"
        };
        AccountDto {
            state,
            epoch: self.account_epoch,
            has_token: credentials.is_some(),
            has_saved_credentials: self.vault.has_saved_credentials(),
            has_app_id: credentials.is_some_and(|value| !value.app_id.is_empty()),
            has_secret: credentials.is_some_and(|value| !value.app_secret.is_empty()),
            #[cfg(target_os = "macos")]
            has_remembered_app_pair: self.vault.has_remembered_app_pair(),
            #[cfg(target_os = "macos")]
            can_remember_saved_app_pair: self.vault.can_remember_saved_app_pair(),
            access_mode: self.account.as_ref().map_or(crate::qobuz::AccessMode::Unknown, |value| value.access_mode),
            reason_code: self.account_error.as_ref().or_else(|| self.vault.load_error()).map(|error| error.code),
            reason: self
                .account_error
                .as_ref()
                .map(|error| error.message.clone())
                .or_else(|| self.vault.replacement_reason()),
        }
    }

    pub fn waiting_reason(&self) -> &'static str {
        match self.account_status().state {
            "ready" => "Waiting to start.",
            "validating" => "Validating saved Qobuz credentials.",
            "rejected" => "Qobuz rejected the token. Enter a replacement token.",
            "unvalidated" => "Retry Qobuz account validation to start downloads.",
            _ => "Set up a Qobuz account to start downloads.",
        }
    }

    pub fn clear_account(&mut self) {
        self.search.invalidate();
        self.bookmark_refresh.extend(self.bookmark_requests.iter()
            .filter(|(_, epoch)| *epoch == self.account_epoch).map(|(id, _)| *id));
        self.account_epoch += 1;
        self.account_cancel.cancel();
        self.account_cancel = CancellationToken::new();
        self.account = None;
        if let Some(attempt) = self.account_attempt.take() {
            attempt.cancel.cancel();
        }
        self.fingerprint = None;
        self.validating = false;
        self.account_error = None;
    }

    pub fn bookmark_dtos(&self) -> Vec<BookmarkDto> {
        self.bookmarks
            .values()
            .map(|bookmark| {
                let mut dto = bookmark.dto.clone();
                dto.resource_kind = Some(bookmark.key.kind);
                dto
            })
            .collect()
    }

    pub fn subscribe_bookmarks(&mut self, channel: Channel<BookmarksSnapshot>) {
        let initial = BookmarksSnapshot {
            revision: self.bookmarks_revision,
            bookmarks: self.bookmark_dtos(),
        };
        if channel.send(initial).is_ok() {
            self.bookmark_channels.push(channel);
        }
    }

    pub fn bookmarks_changed(&mut self) {
        self.bookmark_activity.retain(|id, _| self.bookmarks.contains_key(id));
        self.bookmarks_revision += 1;
        if !self.bookmark_channels.is_empty() {
            let snapshot = BookmarksSnapshot {
                revision: self.bookmarks_revision,
                bookmarks: self.bookmark_dtos(),
            };
            self.bookmark_channels
                .retain(|channel| channel.send(snapshot.clone()).is_ok());
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn profile_error() -> AppError {
    AppError::new(
        "portable_storage",
        "storage",
        "suonelo needs a writable data folder beside the executable.",
        true,
    )
}
