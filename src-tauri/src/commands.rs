use crate::{
    app::{App, AppState, BookmarksSnapshot, BootstrapDto, Destination, SubmissionReport},
    error::{AppError, Result},
    media::{self, MetadataField},
    qobuz::{Credentials, EmailSignInPhase},
    queue::{QueueEvent, QueueSnapshot},
    storage::{BookmarkDto, MaxConnections, SettingsDto, SettingsPatch},
    vault::{AccountDto, CredentialInput},
};
#[cfg(target_os = "macos")]
use crate::vault::PreparedToken;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use tauri::{AppHandle, State, ipc::Channel};
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

type Managed<'a> = State<'a, Arc<App>>;
#[cfg(not(target_os = "macos"))]
type PreparedToken = (Credentials, bool);

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(AppError::task)?
}

#[tauri::command]
pub async fn bootstrap(app: Managed<'_>) -> Result<BootstrapDto> {
    let state = app.state();
    Ok(BootstrapDto {
        settings: state.settings.dto(&app.root),
        queue: state.queue.snapshot(),
        bookmarks: state.bookmark_dtos(),
        account: state.account_status(),
    })
}

#[tauri::command]
pub fn queue_snapshot(app: Managed<'_>) -> Result<QueueSnapshot> {
    Ok(app.state().queue.snapshot())
}

#[tauri::command]
pub fn set_queue_paused(app: Managed<'_>, paused: bool) -> Result<QueueSnapshot> {
    let mut state = app.state();
    state.ensure_open()?;
    state.queue.set_user_pause(paused, app.qobuz.recovery());
    Ok(state.queue.snapshot())
}

#[tauri::command]
pub fn subscribe_queue(app: Managed<'_>, channel: Channel<QueueEvent>) -> Result<()> {
    app.state().queue.subscribe(channel);
    Ok(())
}

#[tauri::command]
pub async fn submit_links(
    app: Managed<'_>,
    text: String,
    destination: Destination,
    quality_override: Option<u8>,
) -> Result<SubmissionReport> {
    let app = app.inner().clone();
    blocking(move || match quality_override {
        Some(quality) => app.submit_with_options(text, destination, Some(quality), false),
        None => app.submit(text, destination),
    })
    .await
}

#[tauri::command]
pub async fn inspect_link_file(path: PathBuf) -> Result<crate::link_import::ImportReport> {
    blocking(move || Ok(crate::link_import::inspect_link_file(&path))).await
}

#[tauri::command]
pub async fn submit_imported_links(
    app: Managed<'_>,
    text: String,
    destination: Destination,
    quality_override: Option<u8>,
) -> Result<SubmissionReport> {
    let app = app.inner().clone();
    blocking(move || app.submit_with_options(text, destination, quality_override, true)).await
}

#[tauri::command]
pub async fn clear_downloads(app: Managed<'_>) -> Result<crate::queue::ClearReport> {
    let (report, removed) = {
        let mut state = app.state();
        state.ensure_open()?;
        let before = state
            .queue
            .snapshot()
            .jobs
            .into_iter()
            .map(|job| job.id)
            .collect::<Vec<_>>();
        let report = state.queue.clear_downloads();
        state.queue.reset_idle_pause(app.qobuz.recovery());
        let remaining = state
            .queue
            .snapshot()
            .jobs
            .into_iter()
            .map(|job| job.id)
            .collect::<std::collections::HashSet<_>>();
        (
            report,
            before
                .into_iter()
                .filter(|id| !remaining.contains(id))
                .collect::<Vec<_>>(),
        )
    };
    app.media.discard_retained_jobs(&removed).await;
    Ok(report)
}

#[tauri::command]
pub fn retry_job(app: Managed<'_>, job_id: u64) -> Result<()> {
    retry(app.inner(), job_id, None)
}

#[tauri::command]
pub fn retry_file(app: Managed<'_>, job_id: u64, file_id: u64) -> Result<()> {
    retry(app.inner(), job_id, Some(file_id))
}

#[tauri::command]
pub fn replace_search_audio(app: Managed<'_>, job_id: u64, file_id: u64, token: String) -> Result<()> {
    let mut state = app.state();
    state.ensure_open()?;
    if state.account.is_none() {
        return Err(AppError::invalid("account", "Validate the current account before replacing audio."));
    }
    let settings = state.settings.clone();
    let reason = state.waiting_reason();
    state.queue.approve_replacement(job_id, file_id, &token, &settings, reason)
}

fn retry(app: &App, id: u64, extra: Option<u64>) -> Result<()> {
    let mut state = app.state();
    state.ensure_open()?;
    let settings = state.settings.clone();
    let reason = state.waiting_reason();
    state.queue.retry(id, &settings, reason, extra)
}

#[tauri::command]
pub fn download_again(app: Managed<'_>, job_id: u64) -> Result<()> {
    let mut state = app.state();
    state.ensure_open()?;
    let settings = state.settings.clone();
    let reason = state.waiting_reason();
    state.queue.download_again(job_id, &settings, reason)
}

#[tauri::command]
pub fn cancel_job(app: Managed<'_>, job_id: u64) -> Result<()> {
    let mut state = app.state();
    state.queue.cancel(job_id, false)?;
    state.queue.reset_idle_pause(app.qobuz.recovery());
    Ok(())
}

#[tauri::command]
pub async fn remove_job(app: Managed<'_>, job_id: u64) -> Result<()> {
    {
        let mut state = app.state();
        state.queue.cancel(job_id, true)?;
        state.queue.reset_idle_pause(app.qobuz.recovery());
    }
    app.media.discard_retained_jobs(&[job_id]).await;
    Ok(())
}

#[tauri::command]
pub fn apply_settings_to_pending(app: Managed<'_>, ids: Option<Vec<u64>>) -> Result<()> {
    let mut state = app.state();
    state.ensure_open()?;
    let settings = state.settings.clone();
    state.queue.apply(ids, &settings);
    Ok(())
}

#[tauri::command]
pub fn decide_large_submission(
    app: Managed<'_>,
    id: u64,
    plan_revision: u64,
    decision: bool,
) -> Result<()> {
    app.state().queue.decide(id, plan_revision, decision)
}

#[tauri::command]
pub fn list_bookmarks(app: Managed<'_>) -> Result<Vec<BookmarkDto>> {
    Ok(app.state().bookmark_dtos())
}

#[tauri::command]
pub fn subscribe_bookmarks(app: Managed<'_>, channel: Channel<BookmarksSnapshot>) -> Result<()> {
    app.state().subscribe_bookmarks(channel);
    Ok(())
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum BookmarkScope {
    All,
    Selected { ids: Vec<u64> },
    Single { id: u64 },
}

#[tauri::command]
pub async fn download_bookmarks(
    app: Managed<'_>,
    scope: BookmarkScope,
) -> Result<SubmissionReport> {
    let ids = match scope {
        BookmarkScope::All => None,
        BookmarkScope::Selected { ids } => Some(ids),
        BookmarkScope::Single { id } => Some(vec![id]),
    };
    let app = app.inner().clone();
    blocking(move || app.download_bookmarks(ids)).await
}

#[tauri::command]
pub async fn retry_bookmark(app: Managed<'_>, id: u64) -> Result<()> {
    let mut state = app.state();
    state.ensure_open()?;
    if !state.bookmarks.contains_key(&id) {
        return Err(AppError::invalid(
            "bookmarks",
            "This bookmark is no longer saved.",
        ));
    }
    if state.account.is_none() {
        return Err(AppError::new(
            "account_required",
            "bookmarks",
            state.waiting_reason(),
            true,
        ));
    }
    app.inner().resolve_bookmark(&mut state, id);
    Ok(())
}

#[tauri::command]
pub async fn remove_bookmarks(app: Managed<'_>, ids: Vec<u64>) -> Result<()> {
    delete_bookmarks(app.inner().clone(), Some(ids)).await
}

#[tauri::command]
pub async fn clear_bookmarks(app: Managed<'_>) -> Result<()> {
    delete_bookmarks(app.inner().clone(), None).await
}

async fn delete_bookmarks(app: Arc<App>, ids: Option<Vec<u64>>) -> Result<()> {
    blocking(move || {
        let mut state = app.state();
        state.ensure_open()?;
        let ids = ids.unwrap_or_else(|| state.bookmarks.keys().copied().collect());
        app.storage.remove_bookmarks(&ids)?;
        for id in ids {
            state.bookmarks.remove(&id);
            state.bookmark_refresh.remove(&id);
        }
        state.bookmarks_changed();
        Ok(())
    })
    .await
}

#[tauri::command]
pub fn read_settings(app: Managed<'_>) -> Result<SettingsDto> {
    Ok(app.state().settings.dto(&app.root))
}

#[tauri::command]
pub async fn patch_settings(
    app: Managed<'_>,
    expected_revision: u64,
    patch: SettingsPatch,
) -> Result<SettingsDto> {
    let app = app.inner().clone();
    blocking(move || save_settings_patch(&app, &mut app.state(), expected_revision, patch, false))
        .await
}

#[derive(Serialize)]
pub struct SavedConnectionLimit {
    settings: SettingsDto,
    queue: QueueSnapshot,
}

#[tauri::command]
pub async fn save_connection_limit(
    app: Managed<'_>,
    expected_revision: u64,
    limit: MaxConnections,
) -> Result<SavedConnectionLimit> {
    let app = app.inner().clone();
    blocking(move || {
        let mut patch = SettingsPatch::default();
        patch.max_connections = Some(limit);
        let mut state = app.state();
        let settings = save_settings_patch(&app, &mut state, expected_revision, patch, true)?;
        Ok(SavedConnectionLimit {
            settings,
            queue: state.queue.snapshot(),
        })
    })
    .await
}

fn save_settings_patch(
    app: &App,
    state: &mut crate::app::AppState,
    expected_revision: u64,
    patch: SettingsPatch,
    live: bool,
) -> Result<SettingsDto> {
    state.ensure_open()?;
    if state.settings.revision != expected_revision {
        return Err(AppError::new(
            "settings_conflict",
            "settings",
            "Settings changed. Reload and apply the edit again.",
            true,
        ));
    }
    let next = state.settings.apply(&app.root, patch)?;
    app.storage.save_settings(&next)?;
    state.settings = next;
    state
        .queue
        .settings_saved(&state.settings, live, app.qobuz.recovery());
    Ok(state.settings.dto(&app.root))
}

#[tauri::command]
pub async fn migrate_theme(app: Managed<'_>, expected_revision: u64, legacy_dark: bool) -> Result<SettingsDto> {
    let app = app.inner().clone();
    blocking(move || {
        let mut state = app.state();
        state.ensure_open()?;
        if let Some(next) = state.settings.migrated_theme(expected_revision, legacy_dark)? {
            app.storage.save_settings(&next)?;
            state.settings = next;
        }
        Ok(state.settings.dto(&app.root))
    }).await
}

#[tauri::command]
pub async fn reset_settings(app: Managed<'_>, expected_revision: u64) -> Result<SettingsDto> {
    let app = app.inner().clone();
    blocking(move || {
        let mut state = app.state();
        state.ensure_open()?;
        if state.settings.revision != expected_revision {
            return Err(AppError::new(
                "settings_conflict",
                "settings",
                "Settings changed. Reload before resetting.",
                true,
            ));
        }
        let next = crate::storage::Settings {
            revision: expected_revision + 1,
            ..Default::default()
        };
        app.storage.save_settings(&next)?;
        state
            .queue
            .settings_saved(&next, false, app.qobuz.recovery());
        state.settings = next;
        Ok(state.settings.dto(&app.root))
    })
    .await
}

#[tauri::command]
pub async fn reset_vault(app: Managed<'_>, confirmation: bool) -> Result<AccountDto> {
    if !confirmation {
        return Err(AppError::invalid(
            "vault",
            "Confirm removing the saved Qobuz credentials.",
        ));
    }
    let app = app.inner().clone();
    blocking(move || {
        let mut state = app.state();
        state.ensure_open()?;
        state.vault.reset()?;
        state.clear_account();
        account_waiting(&mut state, true);
        for bookmark in state.bookmarks.values_mut() {
            bookmark.fingerprint = None;
        }
        let status = state.account_status();
        app.emit_account(status.clone());
        app.storage.save_bookmarks(state.bookmarks.values())?;
        Ok(status)
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmationRequired {
    requires_confirmation: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum AccountConfirmation {
    Switch,
    Active,
}

struct ValidatedAccount {
    credentials: Credentials,
    id: String,
    #[cfg(target_os = "macos")]
    remember_pair: bool,
}

pub(crate) struct AccountAttempt {
    id: String,
    pub(crate) cancel: CancellationToken,
    expires: Instant,
    pending: Option<(ValidatedAccount, AccountConfirmation)>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationResult {
    account: Option<AccountDto>,
    confirmation: Option<AccountConfirmation>,
}

fn confirmation(state: &AppState, candidate: &ValidatedAccount) -> Option<AccountConfirmation> {
    let previous = state.vault.account_id();
    if previous.is_some_and(|id| id != candidate.id)
        || (previous.is_none() && state.vault.has_saved_credentials() && !state.queue.jobs.is_empty())
    {
        Some(AccountConfirmation::Switch)
    } else if state.account.as_ref().is_some_and(|account| account != &candidate.credentials)
        && state.queue.active()
    {
        Some(AccountConfirmation::Active)
    } else {
        None
    }
}

#[tauri::command]
pub async fn validate_credentials(
    app: Managed<'_>,
    attempt: String,
    input: Option<CredentialInput>,
) -> Result<ValidationResult> {
    let app = app.inner().clone();
    let (input, cancel) = {
        let mut state = app.state();
        state.ensure_open()?;
        #[cfg(target_os = "macos")]
        state.vault.ensure_loaded()?;
        let input = if let Some(input) = input {
            state.vault.prepare_token(input)?
        } else {
            saved_token(&state)?
        };
        let cancel = begin_attempt(&mut state, &attempt, Duration::from_secs(60))?;
        (input, cancel)
    };
    let result = validate_candidate(&app, &input, &cancel).await;
    finish_attempt(app, attempt, result).await
}

#[tauri::command]
pub async fn start_email_sign_in(
    window: tauri::WebviewWindow,
    app: Managed<'_>,
    attempt: String,
    phase: Channel<EmailSignInPhase>,
) -> Result<ValidationResult> {
    if window.label() != "main" {
        return Err(AppError::invalid("account", "Start Qobuz sign-in from the main window."));
    }
    let app = app.inner().clone();
    let cancel = begin_attempt(&mut app.state(), &attempt, Duration::from_secs(420))?;
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(account_changed()),
        result = app.qobuz.sign_in_with_email(window, phase) => result
            .map(|(credentials, id)| ValidatedAccount {
                credentials,
                id,
                #[cfg(target_os = "macos")]
                remember_pair: false,
            })
            .map_err(|error| AppError::qobuz(error, "account")),
    };
    finish_attempt(app, attempt, result).await
}

fn begin_attempt(state: &mut AppState, attempt: &str, lifetime: Duration) -> Result<CancellationToken> {
    state.ensure_open()?;
    #[cfg(target_os = "macos")]
    state.vault.ensure_loaded()?;
    if state.validating || state.account_attempt.is_some() {
        return Err(AppError::invalid("account", "A Qobuz validation is already in progress."));
    }
    if attempt.is_empty() || attempt.len() > 64 {
        return Err(AppError::invalid("account", "The Qobuz sign-in attempt is invalid."));
    }
    let cancel = state.account_cancel.child_token();
    state.account_attempt = Some(AccountAttempt {
        id: attempt.to_owned(),
        cancel: cancel.clone(),
        expires: Instant::now() + lifetime,
        pending: None,
    });
    Ok(cancel)
}

async fn finish_attempt(
    app: Arc<App>, attempt: String, result: Result<ValidatedAccount>,
) -> Result<ValidationResult> {
    blocking(move || {
        let mut state = app.state();
        state.ensure_open()?;
        let mut current = take_attempt(&mut state, &attempt)?;
        let candidate = result?;
        if let Some(required) = confirmation(&state, &candidate) {
            let expires = Instant::now() + Duration::from_secs(120);
            current.expires = expires;
            current.pending = Some((candidate, required));
            let cancel = current.cancel.clone();
            state.account_attempt = Some(current);
            let cleanup = app.clone();
            app.tasks.spawn(async move {
                tokio::select! {
                    _ = cancel.cancelled() => (),
                    _ = tokio::time::sleep_until(expires) => {
                        let mut state = cleanup.state();
                        if state.account_attempt.as_ref().is_some_and(|current| current.id == attempt) {
                            state.account_attempt = None;
                        }
                    }
                }
            });
            return Ok(ValidationResult { account: None, confirmation: Some(required) });
        }
        let account = commit_candidate(&app, &mut state, candidate, None)?;
        Ok(ValidationResult { account: Some(account), confirmation: None })
    }).await
}

#[tauri::command]
pub async fn confirm_account_validation(app: Managed<'_>, attempt: String) -> Result<AccountDto> {
    let app = app.inner().clone();
    blocking(move || {
        let mut state = app.state();
        state.ensure_open()?;
        let current = take_attempt(&mut state, &attempt)?;
        let (candidate, approved) = current.pending.ok_or_else(account_changed)?;
        current.cancel.cancel();
        commit_candidate(&app, &mut state, candidate, Some(approved))
    }).await
}

#[tauri::command]
pub fn cancel_account_validation(app: Managed<'_>, attempt: String) {
    cancel_attempt(&mut app.state(), &attempt);
}

fn cancel_attempt(state: &mut AppState, id: &str) {
    if state.account_attempt.as_ref().is_some_and(|current| current.id == id)
        && let Some(current) = state.account_attempt.take()
    {
        current.cancel.cancel();
    }
}

fn take_attempt(state: &mut AppState, id: &str) -> Result<AccountAttempt> {
    if state.account_attempt.as_ref().is_none_or(|current| current.id != id) {
        return Err(account_changed());
    }
    let attempt = state.account_attempt.take().ok_or_else(account_changed)?;
    if attempt.cancel.is_cancelled() || Instant::now() >= attempt.expires {
        attempt.cancel.cancel();
        return Err(account_changed());
    }
    Ok(attempt)
}

fn saved_token(state: &AppState) -> Result<PreparedToken> {
    #[cfg(target_os = "macos")]
    state.vault.ensure_loaded()?;
    let credentials = state.vault.credentials().cloned().ok_or_else(|| {
        state.vault.load_error().cloned().unwrap_or_else(|| {
            AppError::invalid("account", "Enter your Qobuz token.")
        })
    })?;
    #[cfg(not(target_os = "macos"))]
    let input = (credentials, false);
    #[cfg(target_os = "macos")]
    let input = PreparedToken {
        credentials,
        ..PreparedToken::default()
    };
    Ok(input)
}

async fn validate_candidate(
    app: &App,
    input: &PreparedToken,
    cancel: &CancellationToken,
) -> Result<ValidatedAccount> {
    #[cfg(not(target_os = "macos"))]
    let (credentials, retry_reused_pair) = input;
    #[cfg(target_os = "macos")]
    let credentials = &input.credentials;
    app.qobuz.retry_account_connection(credentials);
    #[cfg(not(target_os = "macos"))]
    let validation = app.qobuz.validate_account_with_reuse(credentials, *retry_reused_pair);
    #[cfg(target_os = "macos")]
    let validation = app.qobuz.validate_account(
        credentials,
        input.retry_reused_pair,
        input.remembered_fallback.as_ref(),
    );
    tokio::pin!(validation);
    let mut recovery = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(account_changed()),
            result = &mut validation => return result
                .map(|(credentials, id)| ValidatedAccount {
                    credentials,
                    id,
                    #[cfg(target_os = "macos")]
                    remember_pair: input.remember_pair,
                })
                .map_err(|error| AppError::qobuz(error, "account")),
            _ = recovery.tick() => {
                if app.qobuz.recovery_status(Some(credentials)).retry_connection {
                    return Err(AppError::new(
                        "connection_retry_required", "account",
                        "Qobuz is still unavailable. Choose Retry validation to check the connection again.", true,
                    ));
                }
            }
        }
    }
}

pub async fn validate_saved_account(app: Arc<App>) -> Result<AccountDto> {
    let (input, epoch, cancel) = {
        let mut state = app.state();
        state.ensure_open()?;
        if state.validating || state.account_attempt.is_some() || state.account.is_some() {
            return Ok(state.account_status());
        }
        let input = saved_token(&state)?;
        state.validating = true;
        state.account_error = None;
        account_waiting(&mut state, false);
        app.emit_account(state.account_status());
        (input, state.account_epoch, state.account_cancel.clone())
    };
    let result = validate_candidate(&app, &input, &cancel).await;
    blocking(move || {
        let mut state = app.state();
        state.ensure_open()?;
        if state.account_epoch != epoch {
            return Err(account_changed());
        }
        state.validating = false;
        // The saved token itself establishes continuity for legacy vaults. A
        // protected prior ID that disagrees requires explicit manual review.
        let result = result.and_then(|candidate| {
            if state.vault.account_id().is_some_and(|id| id != candidate.id) {
                return Err(AppError::new(
                    "account_confirmation", "account",
                    "The saved credentials now identify a different Qobuz account. Retry validation to confirm the switch.", false,
                ));
            }
            commit_candidate(&app, &mut state, candidate, None)
        });
        if let Err(error) = &result {
            state.account_error = Some(error.clone());
            account_waiting(&mut state, false);
            app.emit_account(state.account_status());
        }
        result
    }).await
}

fn commit_candidate(
    app: &Arc<App>, state: &mut AppState, candidate: ValidatedAccount,
    confirmed: Option<AccountConfirmation>,
) -> Result<AccountDto> {
    let replaces_live = state.account.as_ref().is_some_and(|account| account != &candidate.credentials);
    let fingerprint = state.vault.save(
        candidate.credentials.clone(),
        candidate.id,
        #[cfg(target_os = "macos")]
        candidate.remember_pair,
    )?;
    if confirmed == Some(AccountConfirmation::Switch) {
        state.queue.shutdown();
        state.queue.reset_idle_pause(app.qobuz.recovery());
    }
    if replaces_live || confirmed == Some(AccountConfirmation::Switch) {
        state.clear_account();
    }
    state.account = Some(candidate.credentials);
    state.fingerprint = Some(fingerprint);
    state.account_error = None;
    if replaces_live || confirmed == Some(AccountConfirmation::Switch) || !state.queue.active() {
        account_waiting(state, replaces_live);
    }
    let mut pending = std::mem::take(&mut state.bookmark_refresh);
    pending.extend(
        state
            .bookmarks
            .values()
            .filter(|bookmark| {
                bookmark.fingerprint.is_none()
                    && bookmark.dto.album_name.is_none()
                    && bookmark.dto.reason.as_deref().is_some_and(|reason| {
                        [
                            "Set up a Qobuz account",
                            "Validate the Qobuz account",
                            "Unlock the vault",
                            "Validating saved Qobuz",
                            "Retry Qobuz account",
                            "Waiting to start",
                            "Waiting for the account change",
                        ]
                        .iter()
                        .any(|prefix| reason.starts_with(prefix))
                    })
            })
            .map(|bookmark| bookmark.dto.id),
    );
    for id in pending {
        app.resolve_bookmark(state, id);
    }
    let status = state.account_status();
    app.emit_account(status.clone());
    Ok(status)
}

fn account_waiting(state: &mut crate::app::AppState, cancel_active: bool) {
    let reason = state.waiting_reason();
    state.queue.account_changed(reason, cancel_active);
}

fn account_changed() -> AppError {
    AppError::new(
        "account_changed",
        "account",
        "The account changed during validation. Validate the current credentials.",
        true,
    )
}

#[tauri::command]
pub fn retry_connection(app: Managed<'_>) -> Result<()> {
    app.state().ensure_open()?;
    app.qobuz.retry_connection();
    Ok(())
}

#[tauri::command]
pub fn search_genres() -> &'static [crate::search::Genre] {
    crate::search::genres()
}

#[tauri::command]
pub async fn search_query(app: Managed<'_>, request: crate::search::Query) -> Result<crate::search::Snapshot> {
    crate::search::query(&app, request).await
}

#[tauri::command]
pub fn search_cancel(app: Managed<'_>, sequence: u64) {
    app.state().search.cancel(sequence);
}

#[tauri::command]
pub async fn search_detail(app: Managed<'_>, request: crate::search::DetailRequest) -> Result<crate::search::DetailResult> {
    crate::search::detail(&app, request).await
}

#[tauri::command]
pub async fn search_artwork(app: Managed<'_>, generation: u64, key: crate::qobuz::search::Id) -> Result<Option<String>> {
    crate::search::artwork(&app, generation, key).await
}

#[tauri::command]
pub async fn clear_search_history(app: Managed<'_>) -> Result<String> {
    let app = app.inner().clone();
    blocking(move || crate::search::clear_history(&app)).await
}

#[tauri::command]
pub fn prepare_search_submission(app: Managed<'_>, request: crate::search::Submission) -> Result<crate::search::Review> {
    crate::search::prepare(&app, request)
}

#[tauri::command]
pub async fn commit_search_submission(app: Managed<'_>, token: String) -> Result<crate::search::SubmissionReport> {
    let app = app.inner().clone();
    blocking(move || crate::search::commit(&app, token)).await
}

#[derive(Serialize)]
pub struct ReleaseFolderStatus {
    available: bool,
    reason: Option<String>,
}

#[tauri::command]
pub async fn release_folder_status(app: Managed<'_>, job_id: u64) -> Result<ReleaseFolderStatus> {
    let app = app.inner().clone();
    blocking(move || {
        let evidence = app.state().queue.release_folder_evidence(job_id);
        let result = evidence.and_then(crate::release_folder::resolve);
        Ok(ReleaseFolderStatus {
            available: result.is_ok(),
            reason: result.err().map(|error| error.message),
        })
    })
    .await
}

#[tauri::command]
pub async fn open_release_folder(app: Managed<'_>, job_id: u64) -> Result<()> {
    let app = app.inner().clone();
    let resolved = blocking(move || {
        let evidence = app.state().queue.release_folder_evidence(job_id)?;
        let folder = crate::release_folder::resolve(evidence.clone())?;
        // Recheck the logical job after potentially slow filesystem access.
        if app.state().queue.release_folder_evidence(job_id)? != evidence {
            return Err(AppError::invalid(
                "queue",
                "The release output changed. Check its folder again.",
            ));
        }
        #[cfg(target_os = "macos")]
        return Ok(folder);
        #[cfg(not(target_os = "macos"))]
        crate::release_folder::open(&folder)
    })
    .await;
    #[cfg(target_os = "macos")]
    {
        let folder = resolved?;
        open_with_macos(
            [
                std::ffi::OsStr::new("-a"),
                std::ffi::OsStr::new("Finder"),
                folder.as_os_str(),
            ],
            "open_folder",
            "queue",
            "Finder could not open the saved release folder.",
        )
        .await
    }
    #[cfg(not(target_os = "macos"))]
    resolved
}

#[tauri::command]
pub async fn open_containing_folder(app: Managed<'_>, job_id: u64, file_id: u64) -> Result<()> {
    let app = app.inner().clone();
    let resolved = blocking(move || {
        let path = app.state().queue.conflict_path(job_id, file_id)?;
        #[cfg(target_os = "macos")]
        {
            let unavailable = || {
                AppError::invalid(
                    "queue",
                    "The recorded conflicting file is unavailable.",
                )
            };
            if !std::fs::symlink_metadata(&path).is_ok_and(|info| info.is_file()) {
                return Err(unavailable());
            }
            let canonical = dunce::canonicalize(&path).map_err(|_| unavailable())?;
            if canonical.parent() != path.parent() {
                return Err(unavailable());
            }
            if app.state().queue.conflict_path(job_id, file_id)? != path {
                return Err(AppError::invalid(
                    "queue",
                    "The recorded conflicting file changed.",
                ));
            }
            Ok(canonical)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let folder = path.parent().filter(|folder| folder.is_dir()).ok_or_else(|| {
                AppError::invalid("queue", "The conflicting file's folder is unavailable.")
            })?;
            #[cfg(windows)]
            {
                use std::os::windows::ffi::OsStrExt;
                use windows::{
                    Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
                    core::{PCWSTR, w},
                };
                let folder: Vec<u16> = dunce::simplified(folder)
                    .as_os_str()
                    .encode_wide()
                    .chain([0])
                    .collect();
                let opened = unsafe {
                    ShellExecuteW(
                        None,
                        w!("open"),
                        PCWSTR(folder.as_ptr()),
                        PCWSTR::null(),
                        PCWSTR::null(),
                        SW_SHOWNORMAL,
                    )
                };
                if opened.0 as isize > 32 {
                    return Ok(());
                }
            }
            Err(AppError::new(
                "open_folder",
                "queue",
                "Windows could not open the conflicting file's folder.",
                true,
            ))
        }
    })
    .await;
    #[cfg(target_os = "macos")]
    {
        let path = resolved?;
        open_with_macos(
            [std::ffi::OsStr::new("-R"), path.as_os_str()],
            "open_folder",
            "queue",
            "Finder could not reveal the conflicting file.",
        )
        .await
    }
    #[cfg(not(target_os = "macos"))]
    resolved
}

#[tauri::command]
pub fn metadata_catalog(app: Managed<'_>) -> Result<Vec<MetadataField>> {
    Ok(app.media.metadata_catalog())
}

#[derive(Deserialize)]
pub struct TemplateInput {
    folder: String,
    filename: String,
}

#[derive(Serialize)]
pub struct Preview {
    path: PathBuf,
}

#[tauri::command]
pub fn preview_output(
    app: Managed<'_>,
    template: TemplateInput,
    quality: Option<u8>,
    safe_mode: Option<bool>,
) -> Result<Preview> {
    let quality = quality.unwrap_or_else(|| app.state().settings.output.quality);
    if !(1..=4).contains(&quality) {
        return Err(AppError::invalid(
            "settings",
            "Choose a quality tier from 1 through 4.",
        ));
    }
    let safe_mode = safe_mode.unwrap_or_else(|| app.state().settings.output.safe_mode);
    let path = media::preview_output(&template.folder, &template.filename, quality, safe_mode)
        .map_err(|error| AppError::media(error, "settings"))?;
    let path = app
        .state()
        .settings
        .destination(&app.root)
        .map_or(path.clone(), |root| root.join(path));
    Ok(Preview { path })
}

#[tauri::command]
pub fn request_close(app: Managed<'_>) -> Result<ConfirmationRequired> {
    Ok(ConfirmationRequired {
        requires_confirmation: app.state().queue.pending(),
    })
}

#[tauri::command]
pub async fn shutdown_session(app: Managed<'_>, handle: AppHandle) -> Result<()> {
    #[cfg(all(not(target_os = "macos"), feature = "preload-prototype"))]
    crate::preload::mark(
        "shutdown_started",
        serde_json::json!({"path": "normal-session-shutdown"}),
    );
    app.shutdown().await;
    handle.exit(0);
    Ok(())
}

#[tauri::command]
pub fn default_music_location() -> Result<Option<PathBuf>> {
    Ok(crate::locations::music_folder())
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AboutLink {
    Project,
    QobuzPrivacy,
}

#[tauri::command]
pub async fn open_credits() -> Result<()> {
    open_about_link(AboutLink::Project).await
}

#[tauri::command]
pub async fn open_about_link(link: AboutLink) -> Result<()> {
    let url = match link {
        AboutLink::Project => "https://github.com/HongYue1/qobuz-cli",
        AboutLink::QobuzPrivacy => "https://www.qobuz.com/us-en/discover/legals/privacy",
    };
    #[cfg(target_os = "macos")]
    return open_with_macos(
        [url],
        "open_link",
        "application",
        "Could not open the selected website.",
    )
    .await;
    #[cfg(windows)]
    {
        use windows::{
            Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
            core::{PCWSTR, w},
        };
        let url: Vec<u16> = url.encode_utf16().chain([0]).collect();
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(url.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize > 32 {
            return Ok(());
        }
    }
    #[cfg(not(target_os = "macos"))]
    Err(AppError::new(
        "open_link",
        "application",
        "Could not open the selected website.",
        true,
    ))
}

#[cfg(target_os = "macos")]
async fn open_with_macos(
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    code: &'static str,
    scope: &'static str,
    message: &'static str,
) -> Result<()> {
    use std::process::Stdio;
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("/usr/bin/open")
            .args(args)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await;
    if matches!(status, Ok(Ok(status)) if status.success()) {
        return Ok(());
    }
    Err(AppError::new(code, scope, message, true))
}

#[tauri::command]
pub fn preload_frontend_ready(handle: AppHandle) {
    #[cfg(all(not(target_os = "macos"), feature = "preload-prototype"))]
    crate::preload::frontend_ready(&handle);
    #[cfg(not(all(not(target_os = "macos"), feature = "preload-prototype")))]
    let _ = handle;
}

#[tauri::command]
pub fn preload_test_close(window: tauri::WebviewWindow) -> Result<()> {
    #[cfg(all(not(target_os = "macos"), feature = "preload-prototype"))]
    if crate::preload::enabled() {
        return window
            .close()
            .map_err(|_| AppError::invalid("startup", "Native close failed"));
    }
    #[cfg(not(all(not(target_os = "macos"), feature = "preload-prototype")))]
    let _ = window;
    Err(AppError::invalid("startup", "Diagnostic mode is inactive"))
}

#[cfg(test)]
mod connection_limit_tests {
    use super::*;

    #[test]
    fn failed_settings_write_preserves_the_saved_and_live_limit() {
        let root = tempfile::tempdir().unwrap();
        let app = App::initialize_root(root.path().to_owned()).unwrap();
        let mut state = app.state();
        app.storage.save_settings(&state.settings).unwrap();
        let db = app.storage.open().unwrap();
        db.execute_batch(
            "CREATE TRIGGER fail_settings BEFORE UPDATE ON settings
             BEGIN SELECT RAISE(ABORT, 'fixture write failure'); END;",
        )
        .unwrap();
        let before = state.queue.snapshot();
        let expected_revision = state.settings.revision;
        let mut patch = SettingsPatch::default();
        patch.max_connections = Some(MaxConnections::Count(8));
        assert!(save_settings_patch(&app, &mut state, expected_revision, patch, true).is_err());
        assert_eq!(state.settings.revision, expected_revision);
        assert_eq!(state.settings.max_connections, MaxConnections::Count(1));
        let after = state.queue.snapshot();
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.live_limit, before.live_limit);
        let saved: String = db
            .query_row("SELECT value FROM settings WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&saved).unwrap()["maxConnections"],
            1
        );
    }
}

#[cfg(all(test, windows))]
mod account_tests {
    use super::*;
    use crate::search::activity::{Credit, event_time};
    use std::{fs, os::windows::fs::OpenOptionsExt};

    fn candidate(id: &str, token: &str) -> ValidatedAccount {
        ValidatedAccount {
            credentials: Credentials {
                token: token.into(),
                app_id: "123456789".into(),
                app_secret: "synthetic-key".into(),
                transport: crate::qobuz::TokenTransport::Query,
                access_mode: crate::qobuz::AccessMode::Unknown,
            },
            id: id.into(),
        }
    }

    #[tokio::test]
    async fn account_commit_keeps_state_on_decline_or_vault_failure_and_clears_only_for_switch() {
        let directory = tempfile::tempdir().unwrap();
        let app = App::initialize_root(directory.path().to_owned()).unwrap();
        let vault = app.root.join("data/credentials.vault");
        {
            let mut state = app.state();
            state.vault.save(candidate("101", "original").credentials, "101".into()).unwrap();
            commit_candidate(&app, &mut state, candidate("101", "original"), None).unwrap();
        }
        app.submit("https://open.qobuz.com/album/fixture-release".into(), Destination::Downloads).unwrap();
        let credit = Credit { artist: "9".into(), release: "fixture-release".into() };
        app.storage.record_search_activity([&credit], 2, event_time()).unwrap();

        let mut state = app.state();
        let queue = state.queue.snapshot();
        assert_eq!(queue.jobs.len(), 1);
        assert!(confirmation(&state, &candidate("101", "new-token")).is_none());
        assert!(confirmation(&state, &candidate("202", "new-token")) == Some(AccountConfirmation::Switch));

        let original = fs::read(&vault).unwrap();
        let cancellation = state.account_cancel.child_token();
        state.account_attempt = Some(AccountAttempt {
            id: "first".into(),
            cancel: cancellation.clone(),
            expires: Instant::now() + Duration::from_secs(1),
            pending: Some((candidate("202", "new-token"), AccountConfirmation::Switch)),
        });
        assert!(take_attempt(&mut state, "wrong").is_err());
        cancel_attempt(&mut state, "first");
        assert!(cancellation.is_cancelled());
        assert!(take_attempt(&mut state, "first").is_err());
        assert_eq!(state.queue.snapshot().revision, queue.revision);
        assert_eq!(fs::read(&vault).unwrap(), original);

        let locked = fs::OpenOptions::new().read(true).share_mode(0).open(&vault).unwrap();
        let failure = commit_candidate(&app, &mut state, candidate("202", "new-token"), Some(AccountConfirmation::Switch))
            .err().expect("locked vault prevents replacement");
        assert_eq!(failure.code, "vault_storage");
        drop(locked);
        assert_eq!(fs::read(&vault).unwrap(), original);
        assert_eq!(state.account.as_ref().unwrap().token, "original");
        assert_eq!(state.queue.snapshot().revision, queue.revision);

        commit_candidate(&app, &mut state, candidate("101", "same-account"), None).unwrap();
        assert_eq!(state.queue.snapshot().jobs.len(), 1);
        let old_epoch = state.account_epoch;
        let old_cancel = state.account_cancel.clone();
        commit_candidate(&app, &mut state, candidate("202", "other-account"), Some(AccountConfirmation::Switch)).unwrap();
        assert!(old_cancel.is_cancelled());
        assert!(state.account_epoch > old_epoch);
        assert!(state.queue.snapshot().jobs.is_empty());
        assert!(state.queue.snapshot().confirmations.is_empty());
        assert_eq!(crate::vault::Vault::new(vault).account_id(), Some("202"));
        assert_eq!(app.storage.search_scores().unwrap().0["9"].0, 2);
    }

}
