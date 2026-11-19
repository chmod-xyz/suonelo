use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    path::{Component, Path},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::{
    sync::Notify,
    time::{Instant, sleep, sleep_until},
};
use tokio_util::sync::CancellationToken;

pub(crate) const CONCURRENT_MEDIA_LIMIT: usize = 8;
pub(crate) const SEQUENTIAL_MEDIA_LIMIT: usize = 32;
const API_LIMIT: usize = 8;
const API_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug)]
pub enum RecoveryNotice {
    Transferring,
    Retrying { reason: String },
    RateLimited { seconds: u64 },
    NetworkBytes { bytes: u64 },
}

type Observer = Arc<dyn Fn(RecoveryNotice) + Send + Sync>;

#[derive(Clone, Default)]
pub struct RetryBudget {
    used: Arc<Mutex<usize>>,
    observer: Option<Observer>,
    pub(crate) defer_no_response: bool,
    pub(crate) single_attempt: bool,
}

impl RetryBudget {
    pub(crate) fn validation() -> Self {
        Self { single_attempt: true, ..Self::default() }
    }

    pub async fn wait(&self, reason: &str) -> bool {
        let delay = {
            let mut used = self.used.lock().unwrap();
            let Some(delay) = [2, 5].get(*used) else {
                return false;
            };
            *used += 1;
            Duration::from_secs(*delay) + Duration::from_millis(rand::random::<u64>() % 501)
        };
        if let Some(observer) = &self.observer {
            observer(RecoveryNotice::Retrying {
                reason: reason.to_owned(),
            });
        }
        sleep(delay).await;
        true
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryDto {
    pub cooldown_seconds: Option<u64>,
    pub retry_connection: bool,
    pub worker_cap: Option<usize>,
}

#[derive(Default)]
pub struct Recovery {
    state: Mutex<State>,
    changed: Notify,
    next_id: AtomicU64,
}

#[derive(Default)]
struct State {
    next_api_start: Option<Instant>,
    api_count: usize,
    file_count: usize,
    sequential_media: bool,
    media_limits: HashMap<u64, usize>,
    media_pause: CancellationToken,
    accounts: HashMap<u64, ApiGate>,
    hosts: HashMap<String, Cooldown>,
    drives: HashMap<String, String>,
    files_by_job: HashMap<u64, usize>,
    file_waiters: VecDeque<FileWaiter>,
    last_grant: HashMap<u64, u64>,
    grant_number: u64,
    step: WorkerStep,
}

#[derive(Default)]
struct WorkerStep {
    cap: Option<usize>,
    epoch: u64,
    successes: u8,
}

#[derive(Default)]
struct ApiGate {
    phase: ApiPhase,
    fallback: u64,
}

#[derive(Default)]
enum ApiPhase {
    #[default]
    Open,
    Cooldown(Instant),
    Probe(Option<u64>),
    Blocked,
}

#[derive(Default)]
struct Cooldown {
    until: Option<Instant>,
    fallback: u64,
}

struct FileWaiter {
    id: u64,
    job: u64,
    limit: usize,
    host: String,
}

impl Recovery {
    pub(crate) fn set_media_paused(&self, paused: bool) {
        let mut state = self.state.lock().unwrap();
        if paused {
            state.media_pause.cancel();
        } else if state.media_pause.is_cancelled() {
            state.media_pause = CancellationToken::new();
        }
        self.changed.notify_waiters();
    }

    pub(crate) fn admit_media(&self, job: u64, sequential: bool, limit: usize) {
        let mut state = self.state.lock().unwrap();
        debug_assert!(state.media_limits.is_empty() || state.sequential_media == sequential);
        state.sequential_media = sequential;
        state.media_limits.insert(job, limit);
    }

    pub(crate) fn finish_media(&self, job: u64) {
        let mut state = self.state.lock().unwrap();
        state.media_limits.remove(&job);
        state.last_grant.remove(&job);
        self.changed.notify_waiters();
    }

    pub(crate) fn update_media_limits(&self, limit: usize) {
        let mut state = self.state.lock().unwrap();
        state
            .media_limits
            .values_mut()
            .for_each(|current| *current = limit);
        self.changed.notify_waiters();
    }

    pub fn status(&self, account: Option<u64>) -> RecoveryDto {
        let state = self.state.lock().unwrap();
        let api = state
            .accounts
            .iter()
            .filter(|(key, _)| account.is_none_or(|account| **key == account))
            .map(|(_, gate)| gate);
        let api_until = api.clone().filter_map(|gate| match gate.phase {
            ApiPhase::Cooldown(until) => Some(until),
            _ => None,
        });
        let cooldown_seconds = state
            .hosts
            .values()
            .filter_map(|host| host.until)
            .chain(api_until)
            .filter_map(seconds_left)
            .max();
        RecoveryDto {
            cooldown_seconds,
            retry_connection: api
                .into_iter()
                .any(|gate| matches!(gate.phase, ApiPhase::Blocked)),
            worker_cap: state.step.cap,
        }
    }

    pub fn retry_connection(&self) {
        let mut state = self.state.lock().unwrap();
        for gate in state.accounts.values_mut() {
            if matches!(gate.phase, ApiPhase::Blocked) {
                gate.phase = ApiPhase::Probe(None);
            }
        }
        self.changed.notify_waiters();
    }

    pub(crate) fn retry_account_connection(&self, account: u64) {
        let mut state = self.state.lock().unwrap();
        if let Some(gate) = state.accounts.get_mut(&account)
            && matches!(gate.phase, ApiPhase::Blocked)
        {
            gate.phase = ApiPhase::Probe(None);
            self.changed.notify_waiters();
        }
    }

    pub(crate) fn api_waiting_reason(&self, account: u64) -> Option<String> {
        let state = self.state.lock().unwrap();
        match state.accounts.get(&account)?.phase {
            ApiPhase::Cooldown(until) => seconds_left(until)
                .map(|seconds| format!("Qobuz is limiting API requests. Retrying in {seconds}s.")),
            ApiPhase::Probe(_) => Some("Checking the Qobuz connection…".into()),
            ApiPhase::Blocked => {
                Some("The Qobuz connection needs attention. Choose Retry connection.".into())
            }
            ApiPhase::Open => None,
        }
    }

    pub(crate) fn rate_limited_host(&self, host: &str, retry_after: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        let gate = state.hosts.entry(host.to_ascii_lowercase()).or_default();
        let until = cooldown_deadline(retry_after, &mut gate.fallback);
        gate.until = Some(gate.until.map_or(until, |current| current.max(until)));
        self.changed.notify_waiters();
    }

    pub(crate) fn host_is_throttled(&self, host: &str) -> bool {
        self.state.lock().unwrap().hosts.get(host)
            .and_then(|gate| gate.until)
            .is_some_and(|until| until > Instant::now())
    }

    pub fn drive_blocked(&self, path: &Path) -> Option<String> {
        let key = volume(path)?;
        self.state.lock().unwrap().drives.get(&key).cloned()
    }

    pub fn release_drive(&self, path: &Path) {
        if let Some(key) = volume(path) {
            self.state.lock().unwrap().drives.remove(&key);
            self.changed.notify_waiters();
        }
    }

    pub(crate) fn request(self: &Arc<Self>, account: u64) -> ApiRequest {
        ApiRequest {
            recovery: self.clone(),
            account,
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub(crate) fn transfer(
        self: &Arc<Self>,
        job: u64,
        generation: u64,
        limit: usize,
        observer: impl Fn(RecoveryNotice) + Send + Sync + 'static,
    ) -> TransferContext {
        let observer: Observer = Arc::new(observer);
        TransferContext {
            recovery: self.clone(),
            job,
            generation,
            limit,
            observer,
        }
    }
}

impl State {
    fn capacity(&self) -> bool {
        self.step
            .cap
            .is_none_or(|cap| self.api_count + self.file_count < cap)
    }

    fn next_file(&self) -> Option<u64> {
        let ceiling = if self.sequential_media {
            SEQUENTIAL_MEDIA_LIMIT
        } else {
            CONCURRENT_MEDIA_LIMIT
        };
        if self.media_pause.is_cancelled() || self.file_count >= ceiling || !self.capacity() {
            return None;
        }
        let now = Instant::now();
        self.file_waiters
            .iter()
            .filter(|waiter| {
                let limit = self
                    .media_limits
                    .get(&waiter.job)
                    .copied()
                    .unwrap_or(waiter.limit);
                self.files_by_job.get(&waiter.job).copied().unwrap_or(0) < limit
                    && self
                        .hosts
                        .get(&waiter.host)
                        .and_then(|host| host.until)
                        .is_none_or(|until| until <= now)
            })
            .min_by_key(|waiter| self.last_grant.get(&waiter.job).copied().unwrap_or(0))
            .map(|waiter| waiter.id)
    }
}

impl WorkerStep {
    fn finish(&mut self, epoch: u64, outcome: ApiOutcome<'_>) {
        if epoch != self.epoch {
            return;
        }
        match outcome {
            ApiOutcome::TimedOut => {
                self.successes = 0;
                let reduced = Some(if matches!(self.cap, Some(1 | 2)) {
                    1
                } else {
                    2
                });
                if reduced != self.cap {
                    self.cap = reduced;
                    self.epoch += 1;
                }
            }
            ApiOutcome::Success if self.cap.is_some() => {
                self.successes += 1;
                if self.successes == 3 {
                    self.cap = match self.cap {
                        Some(1) => Some(2),
                        Some(2) => Some(4),
                        _ => None,
                    };
                    self.successes = 0;
                    self.epoch += 1;
                }
            }
            ApiOutcome::Success => (),
            _ => self.successes = 0,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ApiOutcome<'a> {
    Success,
    TimedOut,
    Transient,
    Rejected,
    RateLimited(Option<&'a str>),
}

pub(crate) struct ApiRequest {
    recovery: Arc<Recovery>,
    account: u64,
    id: u64,
}

impl ApiRequest {
    pub async fn acquire(&self) -> ApiPermit<'_> {
        loop {
            let notice = self.recovery.changed.notified();
            tokio::pin!(notice);
            notice.as_mut().enable();
            let deadline = {
                let mut state = self.recovery.state.lock().unwrap();
                let now = Instant::now();
                let gate = state.accounts.entry(self.account).or_default();
                if matches!(gate.phase, ApiPhase::Cooldown(until) if until <= now) {
                    gate.phase = ApiPhase::Probe(None);
                }
                let (eligible, probe, cooldown) = match gate.phase {
                    ApiPhase::Open => (true, false, None),
                    ApiPhase::Probe(owner) if owner.is_none_or(|id| id == self.id) => {
                        (true, true, None)
                    }
                    ApiPhase::Cooldown(until) => (false, false, Some(until)),
                    _ => (false, false, None),
                };
                let next_start = state.next_api_start.filter(|next| *next > now);
                if eligible
                    && next_start.is_none()
                    && state.api_count < API_LIMIT
                    && state.capacity()
                {
                    if probe {
                        state.accounts.get_mut(&self.account).unwrap().phase =
                            ApiPhase::Probe(Some(self.id));
                    }
                    state.api_count += 1;
                    state.next_api_start = Some(now + API_INTERVAL);
                    return ApiPermit {
                        request: self,
                        probe,
                        epoch: state.step.epoch,
                    };
                }
                cooldown.or(next_start.filter(|_| eligible))
            };
            match deadline {
                Some(deadline) => {
                    tokio::select! { _ = notice => (), _ = sleep_until(deadline) => () }
                }
                None => notice.await,
            }
        }
    }

    pub fn block_probe(&self) {
        let mut state = self.recovery.state.lock().unwrap();
        let gate = state.accounts.entry(self.account).or_default();
        if matches!(gate.phase, ApiPhase::Probe(Some(id)) if id == self.id) {
            gate.phase = ApiPhase::Blocked;
            self.recovery.changed.notify_waiters();
        }
    }
}

impl Drop for ApiRequest {
    fn drop(&mut self) {
        let mut state = self.recovery.state.lock().unwrap();
        let gate = state.accounts.entry(self.account).or_default();
        if matches!(gate.phase, ApiPhase::Probe(Some(id)) if id == self.id) {
            gate.phase = ApiPhase::Probe(None);
            self.recovery.changed.notify_waiters();
        }
    }
}

pub(crate) struct ApiPermit<'a> {
    request: &'a ApiRequest,
    probe: bool,
    epoch: u64,
}

impl ApiPermit<'_> {
    pub fn probing(&self) -> bool {
        self.probe
    }

    pub fn finish(self, outcome: ApiOutcome<'_>) {
        let mut state = self.request.recovery.state.lock().unwrap();
        state.step.finish(self.epoch, outcome);
        let gate = state.accounts.entry(self.request.account).or_default();
        let owns_probe = matches!(gate.phase, ApiPhase::Probe(Some(id)) if id == self.request.id);
        match outcome {
            ApiOutcome::Success => {
                gate.fallback = 0;
                if owns_probe {
                    gate.phase = ApiPhase::Open;
                }
            }
            ApiOutcome::RateLimited(header) => {
                let until = cooldown_deadline(header, &mut gate.fallback);
                let until = match gate.phase {
                    ApiPhase::Cooldown(current) => current.max(until),
                    _ => until,
                };
                gate.phase = ApiPhase::Cooldown(until);
            }
            ApiOutcome::Rejected if owns_probe => gate.phase = ApiPhase::Probe(None),
            _ => (),
        }
        // The permit releases shared capacity after this lock guard is dropped.
    }
}

impl Drop for ApiPermit<'_> {
    fn drop(&mut self) {
        let mut state = self.request.recovery.state.lock().unwrap();
        state.api_count -= 1;
        self.request.recovery.changed.notify_waiters();
    }
}

#[derive(Clone)]
pub struct TransferContext {
    recovery: Arc<Recovery>,
    job: u64,
    generation: u64,
    limit: usize,
    observer: Observer,
}

impl TransferContext {
    pub(crate) async fn wait_for_media(&self, cancel: &CancellationToken) -> bool {
        loop {
            let notice = self.recovery.changed.notified();
            tokio::pin!(notice);
            notice.as_mut().enable();
            if cancel.is_cancelled() {
                return false;
            }
            if !self
                .recovery
                .state
                .lock()
                .unwrap()
                .media_pause
                .is_cancelled()
            {
                return true;
            }
            tokio::select! {
                _ = cancel.cancelled() => return false,
                _ = notice => (),
            }
        }
    }

    pub(crate) fn is_sequential(&self) -> bool {
        let state = self.recovery.state.lock().unwrap();
        state.sequential_media && state.media_limits.contains_key(&self.job)
    }

    pub fn received(&self, bytes: u64) {
        (self.observer)(RecoveryNotice::NetworkBytes { bytes });
    }

    pub fn attempt_key(&self) -> (u64, u64) {
        (self.job, self.generation)
    }

    pub fn budget(&self) -> RetryBudget {
        RetryBudget {
            observer: Some(self.observer.clone()),
            ..RetryBudget::default()
        }
    }

    pub async fn acquire(&self, host: &str, cancel: &CancellationToken) -> Option<TransferPermit> {
        let host = host.to_ascii_lowercase();
        let id = self.recovery.next_id.fetch_add(1, Ordering::Relaxed);
        let waiter = TransferWaiter {
            recovery: &self.recovery,
            id,
        };
        self.recovery
            .state
            .lock()
            .unwrap()
            .file_waiters
            .push_back(FileWaiter {
                id,
                job: self.job,
                limit: self.limit,
                host: host.clone(),
            });
        self.recovery.changed.notify_waiters();
        let mut last_report = None;
        loop {
            let notice = self.recovery.changed.notified();
            tokio::pin!(notice);
            notice.as_mut().enable();
            if cancel.is_cancelled() {
                return None;
            }
            let (permit, cooldown) = {
                let mut state = self.recovery.state.lock().unwrap();
                let cooldown = state
                    .hosts
                    .get(&host)
                    .and_then(|gate| gate.until)
                    .filter(|until| *until > Instant::now());
                if state.next_file() == Some(id) {
                    state.file_waiters.retain(|waiter| waiter.id != id);
                    state.file_count += 1;
                    *state.files_by_job.entry(self.job).or_default() += 1;
                    state.grant_number += 1;
                    let grant = state.grant_number;
                    state.last_grant.insert(self.job, grant);
                    (
                        Some(TransferPermit {
                            recovery: self.recovery.clone(),
                            job: self.job,
                            pause: state.media_pause.clone(),
                        }),
                        None,
                    )
                } else {
                    (None, cooldown)
                }
            };
            if let Some(permit) = permit {
                drop(waiter);
                (self.observer)(RecoveryNotice::Transferring);
                return Some(permit);
            }
            if let Some(until) = cooldown {
                if let Some(seconds) =
                    seconds_left(until).filter(|seconds| Some(*seconds) != last_report)
                {
                    last_report = Some(seconds);
                    (self.observer)(RecoveryNotice::RateLimited { seconds });
                }
                let next = until.min(Instant::now() + Duration::from_secs(1));
                tokio::select! { _ = cancel.cancelled() => return None, _ = notice => (), _ = sleep_until(next) => () }
            } else {
                tokio::select! { _ = cancel.cancelled() => return None, _ = notice => () }
            }
        }
    }

    pub fn disk_full(&self, path: &Path) {
        if let Some(key) = volume(path) {
            self.recovery
                .state
                .lock()
                .unwrap()
                .drives
                .insert(key.clone(), key);
        }
    }

    pub async fn wait_for_drive(&self, path: &Path, cancel: &CancellationToken) -> bool {
        !cancel.is_cancelled() && self.recovery.drive_blocked(path).is_none()
    }

    pub(crate) async fn wait_for_drive_release(
        &self,
        path: &Path,
        cancel: &CancellationToken,
    ) -> bool {
        loop {
            let notice = self.recovery.changed.notified();
            tokio::pin!(notice);
            notice.as_mut().enable();
            if cancel.is_cancelled() {
                return false;
            }
            if self.recovery.drive_blocked(path).is_none() {
                return true;
            }
            tokio::select! {
                _ = cancel.cancelled() => return false,
                _ = notice => (),
            }
        }
    }
}

struct TransferWaiter<'a> {
    recovery: &'a Recovery,
    id: u64,
}

impl Drop for TransferWaiter<'_> {
    fn drop(&mut self) {
        self.recovery
            .state
            .lock()
            .unwrap()
            .file_waiters
            .retain(|waiter| waiter.id != self.id);
        self.recovery.changed.notify_waiters();
    }
}

pub struct TransferPermit {
    recovery: Arc<Recovery>,
    job: u64,
    pub(crate) pause: CancellationToken,
}

impl TransferPermit {
    pub fn rate_limited_host(&self, host: &str, retry_after: Option<&str>) {
        self.recovery.rate_limited_host(host, retry_after);
    }

    pub fn success_host(&self, host: &str) {
        if let Some(gate) = self
            .recovery
            .state
            .lock()
            .unwrap()
            .hosts
            .get_mut(&host.to_ascii_lowercase())
        {
            gate.fallback = 0;
        }
    }
}

impl Drop for TransferPermit {
    fn drop(&mut self) {
        let mut state = self.recovery.state.lock().unwrap();
        state.file_count -= 1;
        if let Some(count) = state.files_by_job.get_mut(&self.job) {
            *count -= 1;
            if *count == 0 {
                state.files_by_job.remove(&self.job);
            }
        }
        self.recovery.changed.notify_waiters();
    }
}

fn seconds_left(until: Instant) -> Option<u64> {
    let left = until.checked_duration_since(Instant::now())?;
    Some(left.as_secs() + u64::from(left.subsec_nanos() != 0))
}

fn cooldown_deadline(header: Option<&str>, fallback: &mut u64) -> Instant {
    let now = Instant::now();
    if let Some(delay) = header
        .map(str::trim)
        .and_then(|header| {
            header
                .parse::<u64>()
                .ok()
                .map(Duration::from_secs)
                .or_else(|| {
                    let date = httpdate::parse_http_date(header).ok()?;
                    Some(date.duration_since(SystemTime::now()).unwrap_or_default())
                })
        })
        .and_then(|delay| now.checked_add(delay))
    {
        return delay;
    }
    *fallback = if *fallback == 0 {
        30
    } else {
        (*fallback * 2).min(300)
    };
    now + Duration::from_secs(*fallback)
}

fn volume(path: &Path) -> Option<String> {
    match path.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            std::path::Prefix::Disk(letter) | std::path::Prefix::VerbatimDisk(letter) => {
                Some(format!("{}:", char::from(letter).to_ascii_uppercase()))
            }
            std::path::Prefix::UNC(server, share)
            | std::path::Prefix::VerbatimUNC(server, share) => Some(format!(
                "\\\\{}\\{}",
                server.to_string_lossy().to_lowercase(),
                share.to_string_lossy().to_lowercase()
            )),
            _ => Some(prefix.as_os_str().to_string_lossy().to_lowercase()),
        },
        Component::RootDir => Some("/".to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_uses_only_requests_from_the_current_step() {
        let mut step = WorkerStep::default();
        step.finish(0, ApiOutcome::TimedOut);
        assert_eq!(step.cap, Some(2));
        step.finish(0, ApiOutcome::TimedOut);
        assert_eq!(step.cap, Some(2));
        step.finish(1, ApiOutcome::TimedOut);
        assert_eq!(step.cap, Some(1));
        for expected in [Some(2), Some(4), None] {
            let epoch = step.epoch;
            for _ in 0..3 {
                step.finish(epoch, ApiOutcome::Success);
            }
            assert_eq!(step.cap, expected);
        }
    }

    #[test]
    fn a_valid_retry_after_is_not_capped_at_five_minutes() {
        let mut fallback = 0;
        assert!(seconds_left(cooldown_deadline(Some("1800"), &mut fallback)).unwrap() >= 1799);
        assert!(seconds_left(cooldown_deadline(Some("garbage"), &mut fallback)).unwrap() >= 29);
        assert!(seconds_left(cooldown_deadline(None, &mut fallback)).unwrap() >= 59);
    }

    #[test]
    fn round_robin_skips_jobs_at_their_captured_cap() {
        let mut state = State::default();
        for (id, job) in [(1, 10), (2, 10), (3, 20), (4, 30)] {
            state.file_waiters.push_back(FileWaiter {
                id,
                job,
                limit: 1,
                host: "example.test".into(),
            });
        }
        assert_eq!(state.next_file(), Some(1));
        state.files_by_job.insert(10, 1);
        state.file_count = 1;
        assert_eq!(state.next_file(), Some(3));
        state.last_grant.insert(20, 2);
        assert_eq!(state.next_file(), Some(4));
        state.api_count = 1;
        state.step.cap = Some(2);
        assert_eq!(state.next_file(), None);
    }

    #[tokio::test]
    async fn cooldown_admits_one_api_check_and_manual_retry_reopens_a_blocked_connection() {
        let recovery = Arc::new(Recovery::default());
        let first = recovery.request(10);
        first
            .acquire()
            .await
            .finish(ApiOutcome::RateLimited(Some("0")));
        let check = first.acquire().await;
        assert!(check.probing());
        let waiting = recovery.request(10);
        assert!(
            tokio::time::timeout(Duration::from_millis(15), waiting.acquire())
                .await
                .is_err()
        );
        check.finish(ApiOutcome::TimedOut);
        first.block_probe();
        assert!(recovery.status(Some(10)).retry_connection);
        assert!(
            tokio::time::timeout(Duration::from_millis(15), waiting.acquire())
                .await
                .is_err()
        );
        recovery.retry_connection();
        let check = waiting.acquire().await;
        assert!(check.probing());
        check.finish(ApiOutcome::Success);
        assert!(!recovery.status(Some(10)).retry_connection);
        assert!(!first.acquire().await.probing());
    }

    #[tokio::test]
    async fn cancelled_transfer_waiters_release_their_place_without_using_a_slot() {
        let recovery = Arc::new(Recovery::default());
        let context = recovery.transfer(1, 0, 1, |_| {});
        let cancel = CancellationToken::new();
        let active = context.acquire("example.test", &cancel).await.unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(context.acquire("example.test", &cancelled).await.is_none());
        assert!(recovery.state.lock().unwrap().file_waiters.is_empty());
        drop(active);
        assert_eq!(recovery.state.lock().unwrap().file_count, 0);
        assert!(context.acquire("example.test", &cancel).await.is_some());
    }

    #[tokio::test]
    async fn eight_concurrent_jobs_can_fill_the_global_transfer_limit() {
        let recovery = Arc::new(Recovery::default());
        let cancel = CancellationToken::new();
        let mut permits = Vec::new();
        for job in 1..=8 {
            let context = recovery.transfer(job, 0, 1, |_| {});
            permits.push(context.acquire("example.test", &cancel).await.unwrap());
        }
        let ninth = recovery.transfer(9, 0, 1, |_| {});
        assert!(
            tokio::time::timeout(
                Duration::from_millis(15),
                ninth.acquire("example.test", &cancel)
            )
            .await
            .is_err()
        );
        permits.pop();
        assert!(ninth.acquire("example.test", &cancel).await.is_some());
    }

    #[tokio::test]
    async fn existing_waiters_obey_live_raises_and_reductions_and_the_sequential_ceiling() {
        use futures_util::poll;
        use std::pin::pin;
        let recovery = Arc::new(Recovery::default());
        recovery.admit_media(1, true, 1);
        let transfer = recovery.transfer(1, 0, 1, |_| {});
        let cancel = CancellationToken::new();
        let first = transfer.acquire("example.test", &cancel).await.unwrap();
        let mut second = pin!(transfer.acquire("example.test", &cancel));
        assert!(poll!(&mut second).is_pending());
        recovery.update_media_limits(2);
        let second = second.await.unwrap();
        let mut third = pin!(transfer.acquire("example.test", &cancel));
        assert!(poll!(&mut third).is_pending());
        recovery.update_media_limits(1);
        drop(second);
        assert!(poll!(&mut third).is_pending());
        drop(first);
        let third = third.await.unwrap();
        recovery.update_media_limits(usize::MAX);
        let mut held = vec![third];
        while held.len() < SEQUENTIAL_MEDIA_LIMIT {
            held.push(transfer.acquire("example.test", &cancel).await.unwrap());
        }
        let mut excess = pin!(transfer.acquire("example.test", &cancel));
        assert!(poll!(&mut excess).is_pending());
        drop(held.pop());
        assert!(excess.await.is_some());
        drop(held);
        assert_eq!(recovery.state.lock().unwrap().file_count, 0);
    }

    #[tokio::test]
    async fn pause_invalidates_grants_and_resume_releases_existing_waiters() {
        use futures_util::poll;
        use std::pin::pin;
        let recovery = Arc::new(Recovery::default());
        recovery.admit_media(1, true, 1);
        let context = recovery.transfer(1, 0, 1, |_| {});
        let cancel = CancellationToken::new();
        let active = context.acquire("example.test", &cancel).await.unwrap();
        let signal = active.pause.clone();
        recovery.set_media_paused(true);
        assert!(signal.is_cancelled());
        drop(active);
        assert_eq!(recovery.state.lock().unwrap().file_count, 0);
        let mut waiting = pin!(context.acquire("example.test", &cancel));
        assert!(poll!(&mut waiting).is_pending());
        assert!(!recovery.request(2).acquire().await.probing());
        recovery.set_media_paused(false);
        let resumed = waiting.await.unwrap();
        assert!(!resumed.pause.is_cancelled());
        assert!(signal.is_cancelled());
    }
    #[tokio::test]
    async fn file_host_cooldowns_leave_other_hosts_and_accounts_eligible() {
        let recovery = Arc::new(Recovery::default());
        let context = recovery.transfer(1, 0, 8, |_| {});
        let cancel = CancellationToken::new();
        let file = context.acquire("limited.test", &cancel).await.unwrap();
        file.rate_limited_host("limited.test", Some("60"));
        drop(file);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(15),
                context.acquire("limited.test", &cancel)
            )
            .await
            .is_err()
        );
        assert!(context.acquire("other.test", &cancel).await.is_some());
        let account = recovery.request(1);
        account
            .acquire()
            .await
            .finish(ApiOutcome::RateLimited(Some("60")));
        let other = recovery.request(2);
        assert!(!other.acquire().await.probing());
        assert!(
            !recovery
                .state
                .lock()
                .unwrap()
                .file_waiters
                .iter()
                .any(|waiter| waiter.host == "limited.test")
        );
    }

    #[test]
    fn http_date_waits_and_windows_drive_holds_keep_their_scope() {
        let date = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(1200));
        let mut fallback = 0;
        assert!(seconds_left(cooldown_deadline(Some(&date), &mut fallback)).unwrap() >= 1198);
        assert_eq!(fallback, 0);
        #[cfg(windows)]
        {
            let recovery = Arc::new(Recovery::default());
            let context = recovery.transfer(1, 0, 4, |_| {});
            context.disk_full(Path::new("C:\\Music\\file.flac"));
            assert_eq!(
                recovery.drive_blocked(Path::new("C:\\Other")),
                Some("C:".into())
            );
            assert!(recovery.drive_blocked(Path::new("D:\\Music")).is_none());
            recovery.release_drive(Path::new("C:\\Music"));
            assert!(recovery.drive_blocked(Path::new("C:\\Other")).is_none());
        }
    }
}
