use super::{
    ApiError, JSON_LIMIT, PLAYER, QobuzClient, QobuzError, TokenTransport, bounded_body,
    stream::{MP3_FORMAT, read_mp3_preview, read_offer},
};
use crate::recovery::RetryBudget;
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use regex::Regex;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{future::Future, time::Duration};
use url::Url;
use zeroize::{Zeroize, Zeroizing};
#[cfg(target_os = "macos")]
use zeroize::ZeroizeOnDrop;

const PROBE_TRACK: &str = "5966783";
const SIGNING_PROBE_LIMIT: usize = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessMode {
    #[default]
    Unknown,
    Full,
    Preview,
}

impl AccessMode {
    pub fn is_preview(self) -> bool {
        self == Self::Preview
    }

    pub(crate) fn allows_preview_selection(self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self != Self::Full
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.is_preview()
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    pub token: String,
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub app_secret: String,
    #[serde(default, skip_serializing_if = "TokenTransport::is_query")]
    pub transport: TokenTransport,
    #[serde(skip)]
    pub access_mode: AccessMode,
    #[cfg(target_os = "macos")]
    #[serde(default)]
    pub app_pair_discovered: bool,
}

#[cfg(target_os = "macos")]
impl Credentials {
    pub(crate) fn has_token_app_pair(&self) -> bool {
        self.transport == TokenTransport::Query
            && valid_app_id(self.app_id.trim())
            && !self.app_secret.trim().is_empty()
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, ZeroizeOnDrop)]
pub(crate) struct AppPair {
    app_id: String,
    secret: String,
}

#[cfg(target_os = "macos")]
impl AppPair {
    pub(crate) fn from_token_credentials(credentials: &Credentials) -> Option<Self> {
        credentials.has_token_app_pair().then(|| Self {
            app_id: credentials.app_id.trim().to_owned(),
            secret: credentials.app_secret.trim().to_owned(),
        })
    }

    pub(crate) fn matches(&self, credentials: &Credentials) -> bool {
        self.app_id == credentials.app_id.trim() && self.secret == credentials.app_secret.trim()
    }

    pub(crate) fn is_complete(&self) -> bool {
        valid_app_id(&self.app_id) && !self.secret.trim().is_empty()
    }

    fn apply_to(&self, credentials: &mut Credentials) {
        credentials.app_id.zeroize();
        credentials.app_id.clone_from(&self.app_id);
        credentials.app_secret.zeroize();
        credentials.app_secret.clone_from(&self.secret);
        credentials.app_pair_discovered = false;
    }
}

impl Drop for Credentials {
    fn drop(&mut self) {
        self.token.zeroize();
        self.app_id.zeroize();
        self.app_secret.zeroize();
    }
}

pub(super) struct Candidates {
    pub(super) app_id: String,
    pub(super) secrets: Vec<String>,
}

impl Drop for Candidates {
    fn drop(&mut self) {
        self.secrets.zeroize();
    }
}

impl QobuzClient {
    #[cfg(not(target_os = "macos"))]
    pub async fn validate_account(
        &self,
        input: &Credentials,
    ) -> Result<(Credentials, String), QobuzError> {
        self.validate_account_with_reuse(input, false).await
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) async fn validate_account_with_reuse(
        &self,
        input: &Credentials,
        retry_reused_pair: bool,
    ) -> Result<(Credentials, String), QobuzError> {
        if input.transport == TokenTransport::HeaderOnly {
            return self.validate_with_candidates(input, None).await;
        }
        validation_deadline(validate_token(
            input.clone(),
            retry_reused_pair,
            || self.discover_app_credentials(),
            |credentials| async move { self.account_identity(&credentials).await },
        ))
        .await
    }
    #[cfg(target_os = "macos")]
    pub(crate) async fn validate_account(
        &self,
        input: &Credentials,
        retry_reused_pair: bool,
        remembered_pair: Option<&AppPair>,
    ) -> Result<(Credentials, String), QobuzError> {
        if input.transport == TokenTransport::HeaderOnly {
            return self.validate_with_candidates(input, None).await;
        }
        validation_deadline(validate_token(
            input.clone(),
            retry_reused_pair,
            remembered_pair,
            || self.discover_app_credentials(),
            |credentials| async move { self.account_identity(&credentials).await },
        ))
        .await
    }

    pub(super) async fn validate_with_candidates(
        &self,
        input: &Credentials,
        discovered: Option<Candidates>,
    ) -> Result<(Credentials, String), QobuzError> {
        validation_deadline(self.validate_hosted(input, discovered)).await
    }

    async fn account_identity(&self, credentials: &Credentials) -> Result<String, QobuzError> {
        let body = self
            .request_with_budget(
                "user/get",
                Vec::new,
                credentials,
                &RetryBudget::validation(),
            )
            .await
            .map_err(|failure| validation_error(failure.error))?;
        verified_account_id(&body)
    }

    async fn validate_hosted(
        &self,
        input: &Credentials,
        discovered: Option<Candidates>,
    ) -> Result<(Credentials, String), QobuzError> {
        let mut effective = normalized_credentials(input.clone())?;
        if effective.transport != TokenTransport::HeaderOnly {
            return Err(app_pair_rejected());
        }
        let candidates = if let Some(candidates) = discovered {
            candidates
        } else if valid_app_id(&effective.app_id) && !effective.app_secret.is_empty() {
            Candidates {
                app_id: effective.app_id.clone(),
                secrets: vec![std::mem::take(&mut effective.app_secret)],
            }
        } else {
            return Err(app_pair_rejected());
        };
        effective.app_id.zeroize();
        effective.app_id.clone_from(&candidates.app_id);
        let account_id = self.account_identity(&effective).await?;
        let budget = RetryBudget::validation();
        let mut probes = SigningProbes::new();
        let mut inconclusive = false;
        let mut preview_secret = None;
        for secret in &candidates.secrets {
            if probes.remaining == 0 {
                break;
            }
            effective.app_secret.zeroize();
            effective.app_secret.clone_from(secret);
            let credentials = &effective;
            let budget = &budget;
            let outcome = probe_candidate(
                &mut probes,
                |track_id: String, format_id| async move {
                    let offer = self
                        .signed_stream_request_with_budget(
                            &track_id,
                            format_id,
                            credentials,
                            budget,
                        )
                        .await;
                    verify_discovered_offer(offer, format_id)
                },
                || self.alternate_signing_tracks(credentials, budget),
            )
            .await?;
            match outcome {
                SigningProbe::Verified => {
                    effective.access_mode = AccessMode::Full;
                    return Ok((effective, account_id));
                }
                SigningProbe::Preview => {
                    preview_secret.get_or_insert(secret);
                }
                SigningProbe::Unavailable => inconclusive = true,
                SigningProbe::Rejected => (),
            }
        }
        if let Some(secret) = preview_secret {
            effective.app_secret.zeroize();
            effective.app_secret.clone_from(secret);
            effective.access_mode = AccessMode::Preview;
            return Ok((effective, account_id));
        }
        if inconclusive {
            Err(signing_inconclusive())
        } else {
            Err(app_pair_rejected())
        }
    }

    async fn alternate_signing_tracks(
        &self,
        credentials: &Credentials,
        budget: &RetryBudget,
    ) -> Result<Vec<String>, QobuzError> {
        match self
            .request_with_budget(
                "track/search",
                || vec![("query", "love".into()), ("limit", "20".into())],
                credentials,
                budget,
            )
            .await
        {
            Ok(body) => Ok(alternate_track_ids(&body)),
            Err(failure) if validation_interrupted(failure.error.code) => {
                Err(validation_error(failure.error))
            }
            Err(_) => Ok(Vec::new()),
        }
    }

    pub(super) async fn discover_app_credentials(&self) -> Result<Candidates, QobuzError> {
        let bundle = self.player_bundle().await?;
        parse_player_bundle(&bundle).ok_or_else(discovery_unavailable)
    }

    pub(super) async fn player_bundle(&self) -> Result<Zeroizing<String>, QobuzError> {
        let page = self.player_text("/login", 2 * 1024 * 1024).await?;
        let path = player_bundle_path(&page).ok_or_else(discovery_unavailable)?;
        self.player_text(path, JSON_LIMIT).await
    }

    async fn player_text(&self, path: &str, limit: usize) -> Result<Zeroizing<String>, QobuzError> {
        #[cfg(all(target_os = "macos", test))]
        if let Some(fixture) = &self.fixture {
            return (fixture.player_text)(path);
        }
        const PLAYER_HOST: &str = "play.qobuz.com";
        if self.recovery.host_is_throttled(PLAYER_HOST) {
            return Err(validation_error(QobuzError::new("rate_limit", "", true)));
        }
        // The API client uses HTTPS and refuses redirects. No auth headers or query are added here.
        let response = self
            .api
            .get(format!("{PLAYER}{path}"))
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await
            .map_err(|_| discovery_unavailable())?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|header| header.to_str().ok());
            self.recovery.rate_limited_host(PLAYER_HOST, retry_after);
            return Err(validation_error(QobuzError::new("rate_limit", "", true)));
        }
        if !response.status().is_success() {
            return Err(discovery_unavailable());
        }
        let bytes = Zeroizing::new(
            bounded_body(response, limit)
                .await
                .map_err(|_| discovery_unavailable())?,
        );
        let body = std::str::from_utf8(&bytes).map_err(|_| discovery_unavailable())?;
        Ok(Zeroizing::new(body.to_owned()))
    }
}

async fn validation_deadline<T>(
    validate: impl Future<Output = Result<T, QobuzError>>,
) -> Result<T, QobuzError> {
    tokio::time::timeout(Duration::from_secs(45), validate)
        .await
        .map_err(|_| {
            QobuzError::new(
                "validation_timeout",
                "Qobuz validation timed out. Try again shortly.",
                true,
            )
        })?
}

fn normalized_credentials(mut credentials: Credentials) -> Result<Credentials, QobuzError> {
    credentials.access_mode = AccessMode::Unknown;
    for value in [
        &mut credentials.token,
        &mut credentials.app_id,
        &mut credentials.app_secret,
    ] {
        let trimmed = value.trim();
        if trimmed.len() != value.len() {
            let trimmed = trimmed.to_owned();
            value.zeroize();
            *value = trimmed;
        }
    }
    if !valid_token(&credentials.token) {
        return Err(QobuzError::new(
            "invalid_token",
            "Enter a valid Qobuz account token.",
            false,
        ));
    }
    Ok(credentials)
}

async fn validate_token<Discover, Verify>(
    input: Credentials,
    retry_reused_pair: bool,
    #[cfg(target_os = "macos")]
    remembered_pair: Option<&AppPair>,
    discover: impl FnOnce() -> Discover,
    verify: impl Fn(Credentials) -> Verify,
) -> Result<(Credentials, String), QobuzError>
where
    Discover: Future<Output = Result<Candidates, QobuzError>>,
    Verify: Future<Output = Result<String, QobuzError>>,
{
    let mut effective = normalized_credentials(input)?;
    // Repeating an App ID with another secret cannot change identity verification.
    #[cfg(target_os = "macos")]
    let remembered_pair = remembered_pair.filter(|pair| pair.app_id != effective.app_id);
    match (effective.app_id.is_empty(), effective.app_secret.is_empty()) {
        (true, true) => (),
        (false, false) if valid_app_id(&effective.app_id) => {
            match verify(effective.clone()).await {
                Ok(id) => return Ok((effective, id)),
                Err(error) if retry_reused_pair && app_id_may_be_rejected(&error) => (),
                Err(error) => return Err(error),
            }
        }
        (false, false) => {
            return Err(QobuzError::new(
                "invalid_app_id",
                "Enter a nine-digit Qobuz App ID.",
                false,
            ));
        }
        _ => {
            return Err(QobuzError::new(
                "incomplete_app_pair",
                "Enter both app fields, or clear both for automatic setup.",
                false,
            ));
        }
    }
    let mut candidates = discover().await?;
    #[cfg(target_os = "macos")]
    {
        effective.app_pair_discovered = true;
    }
    effective.app_id.zeroize();
    effective.app_id.clone_from(&candidates.app_id);
    effective.app_secret.zeroize();
    // The first provider-bundle candidate is provisional signing material.
    // Identity validation has no signed operations and does not prove this value.
    effective.app_secret = std::mem::take(
        candidates
            .secrets
            .first_mut()
            .ok_or_else(discovery_unavailable)?,
    );
    #[cfg(target_os = "macos")]
    let id = match verify(effective.clone()).await {
        Err(error) if app_id_may_be_rejected(&error) => {
            let Some(pair) = remembered_pair.filter(|pair| pair.app_id != effective.app_id) else {
                return Err(error);
            };
            pair.apply_to(&mut effective);
            verify(effective.clone()).await?
        }
        result => result?,
    };
    #[cfg(not(target_os = "macos"))]
    let id = verify(effective.clone()).await?;
    Ok((effective, id))
}

fn app_id_may_be_rejected(error: &QobuzError) -> bool {
    matches!(error.code, "invalid_app_id" | "account_rejected")
}

fn discovery_unavailable() -> QobuzError {
    QobuzError::new(
        "credential_discovery",
        "Suonelo could not complete automatic Qobuz setup. Try signing in again shortly.",
        true,
    )
}

fn player_bundle_path(page: &str) -> Option<&str> {
    let scripts = Regex::new(r#"(?i)<script\b[^>]*?\ssrc\s*=\s*(?:"([^"]+)"|'([^']+)')"#).ok()?;
    let allowed =
        Regex::new(r"^/resources/[A-Za-z0-9._-]+(?:/[A-Za-z0-9._-]+)*/bundle\.js$").ok()?;
    let mut result = None;
    for script in scripts.captures_iter(page) {
        let source = script.get(1).or_else(|| script.get(2))?.as_str();
        if !source.split(['?', '#']).next()?.ends_with("/bundle.js") {
            continue;
        }
        if !allowed.is_match(source) || source.split('/').any(|part| matches!(part, "." | "..")) {
            return None;
        }
        let url = Url::parse(&format!("{PLAYER}{source}")).ok()?;
        if url.path() != source || result.is_some_and(|previous| previous != source) {
            return None;
        }
        result = Some(source);
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SigningProbe {
    Verified,
    Rejected,
    Unavailable,
    Preview,
}

struct SigningProbes {
    remaining: usize,
    alternatives: Option<Vec<String>>,
}

impl SigningProbes {
    fn new() -> Self {
        Self {
            remaining: SIGNING_PROBE_LIMIT,
            alternatives: None,
        }
    }
}

async fn probe_candidate<Offer, Tracks>(
    probes: &mut SigningProbes,
    probe: impl Fn(String, u32) -> Offer,
    load_tracks: impl FnOnce() -> Tracks,
) -> Result<SigningProbe, QobuzError>
where
    Offer: std::future::Future<Output = Result<SigningProbe, QobuzError>>,
    Tracks: std::future::Future<Output = Result<Vec<String>, QobuzError>>,
{
    let probe_formats = async |track: &str, remaining: &mut usize, saw_preview: &mut bool| {
        for format_id in [MP3_FORMAT, 6] {
            let Some(next) = remaining.checked_sub(1) else {
                break;
            };
            *remaining = next;
            match probe(track.to_owned(), format_id).await? {
                SigningProbe::Preview => *saw_preview = true,
                SigningProbe::Rejected if *saw_preview => return Ok(SigningProbe::Preview),
                SigningProbe::Unavailable => (),
                outcome => return Ok(outcome),
            }
        }
        Ok(SigningProbe::Unavailable)
    };
    let mut saw_preview = false;
    let initial = probe_formats(PROBE_TRACK, &mut probes.remaining, &mut saw_preview).await?;
    if initial != SigningProbe::Unavailable {
        return Ok(initial);
    }
    if probes.alternatives.is_none() && probes.remaining > 0 {
        probes.alternatives = Some(load_tracks().await?);
    }
    for track in probes.alternatives.as_deref().unwrap_or_default() {
        let outcome = probe_formats(track, &mut probes.remaining, &mut saw_preview).await?;
        if outcome != SigningProbe::Unavailable {
            return Ok(outcome);
        }
    }
    Ok(if saw_preview {
        SigningProbe::Preview
    } else {
        SigningProbe::Unavailable
    })
}

fn alternate_track_ids(body: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    let tracks = body["tracks"]["items"].as_array().into_iter().flatten();
    let preferred = |track: &&Value| track["streamable"].as_bool() == Some(true);
    for track in tracks
        .clone()
        .filter(preferred)
        .chain(tracks.filter(|track| !preferred(track)))
    {
        let id = &track["id"];
        if let Some(id) = id
            .as_u64()
            .or_else(|| id.as_str()?.parse().ok())
            .filter(|id| *id > 0)
        {
            let id = id.to_string();
            if id != PROBE_TRACK && !ids.contains(&id) {
                ids.push(id);
                if ids.len() == 2 {
                    break;
                }
            }
        }
    }
    ids
}

fn verify_discovered_offer(
    offer: Result<Value, ApiError>,
    format_id: u32,
) -> Result<SigningProbe, QobuzError> {
    match offer {
        Ok(body)
            if body["sample"].as_bool() == Some(false)
                && read_offer(&body, 1, 2).is_ok_and(|offer| offer.format_id == format_id) =>
        {
            Ok(SigningProbe::Verified)
        }
        Ok(body) if format_id == MP3_FORMAT && read_mp3_preview(&body).is_some() => {
            Ok(SigningProbe::Preview)
        }
        Ok(_) => Ok(SigningProbe::Unavailable),
        Err(failure) if failure.error.code == "invalid_app_pair" => Ok(SigningProbe::Rejected),
        Err(failure) if validation_interrupted(failure.error.code) => {
            Err(validation_error(failure.error))
        }
        Err(failure)
            if matches!(
                failure.error.code,
                "resource_unavailable" | "quality_unavailable" | "invalid_response"
            ) =>
        {
            Ok(SigningProbe::Unavailable)
        }
        Err(_) => Err(signing_inconclusive()),
    }
}

fn validation_interrupted(code: &str) -> bool {
    matches!(
        code,
        "rate_limit" | "api_timeout" | "network" | "invalid_token" | "missing_entitlement"
    )
}

fn signing_inconclusive() -> QobuzError {
    QobuzError::new(
        "signing_inconclusive",
        "Your account was recognized, but Suonelo could not verify streaming access. Try signing in again shortly.",
        true,
    )
}

fn app_pair_rejected() -> QobuzError {
    #[cfg(target_os = "macos")]
    let (message, retryable) = (
        "Qobuz did not accept the discovered signing material. Try signing in again.",
        false,
    );
    #[cfg(not(target_os = "macos"))]
    let (message, retryable) = (
        "Qobuz rejected automatic app verification. Try signing in again shortly.",
        true,
    );
    QobuzError::new("invalid_app_pair", message, retryable)
}

fn validation_error(error: QobuzError) -> QobuzError {
    let message = match error.code {
        "invalid_app_id" => {
            "Qobuz rejected this App ID. Retry automatic setup or enter app credentials manually."
        }
        "account_rejected" => {
            "Qobuz rejected the token or App ID without identifying which. Retry automatic setup or enter app credentials manually."
        }
        "provider_blocked" => {
            "Qobuz blocked account verification. Check your access on Qobuz and try again."
        }
        "invalid_token" => "Qobuz rejected the account token. Enter a replacement token.",
        "invalid_app_pair" => {
            "Qobuz rejected the app pair. Enter both app fields, or submit the token with both blank to discover a new pair."
        }
        "missing_entitlement" => "This account does not have Qobuz streaming access.",
        "rate_limit" => "Qobuz is limiting requests. Try again after the cooldown.",
        "api_timeout" => "The Qobuz validation request timed out.",
        "network" => "Could not complete the Qobuz validation request. Try again shortly.",
        _ => {
            "Qobuz could not verify these credentials. Check the token and app pair and try again."
        }
    };
    QobuzError::new(error.code, message, error.retryable)
}

pub(crate) fn valid_token(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_whitespace)
}

pub(crate) fn valid_app_id(value: &str) -> bool {
    value.len() == 9 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn verified_account_id(account: &Value) -> Result<String, QobuzError> {
    account
        .pointer("/user/id")
        .or_else(|| account.get("id"))
        .and_then(|id| id.as_u64().or_else(|| id.as_str()?.parse().ok()))
        .filter(|id| *id > 0)
        .map(|id| id.to_string())
        .ok_or_else(|| QobuzError::invalid_response("Qobuz did not return a verified account ID."))
}

pub(super) fn parse_player_bundle(bundle: &str) -> Option<Candidates> {
    let production = Regex::new(r#"(?:^|[,{;\s])(?:production|"production"|'production')\s*:\s*\{\s*(?:api|["']api["'])\s*:\s*\{\s*(?:appId|["']appId["'])\s*:\s*"#).ok()?;
    let quoted_id = Regex::new(r#"^(?:"([0-9]{9})"|'([0-9]{9})')\s*[,}]"#).ok()?;
    let mut app_id = None;
    for field in production.find_iter(bundle) {
        let capture = quoted_id.captures(bundle.get(field.end()..)?)?;
        let id = capture.get(1).or_else(|| capture.get(2))?.as_str();
        if app_id.is_some_and(|previous| previous != id) {
            return None;
        }
        app_id = Some(id);
    }
    let app_id = app_id?.to_owned();
    let seed = Regex::new(
        r#"\.initialSeed\(["']([a-zA-Z0-9+/=_-]+)["'],\s*window\.utimezone\.([a-zA-Z]+)\)"#,
    )
    .ok()?;
    let parts = Regex::new(r#"name\s*:\s*["'][a-zA-Z_]+/([a-zA-Z]+)["'],\s*info\s*:\s*["']([a-zA-Z0-9+/=_-]+)["'],\s*extras\s*:\s*["']([a-zA-Z0-9+/=_-]+)["']"#).ok()?;
    let mut seeds: Vec<_> = seed.captures_iter(bundle).collect();
    if seeds.len() > 1 {
        seeds.rotate_left(1);
    }
    let parts: Vec<_> = parts.captures_iter(bundle).collect();
    let decoded = seeds.iter().filter_map(|seed| {
        let parts = parts
            .iter()
            .find(|part| part[1].eq_ignore_ascii_case(&seed[2]))?;
        let joined = Zeroizing::new([&seed[1], &parts[2], &parts[3]].concat());
        let encoded = joined.get(..joined.len().checked_sub(44)?)?;
        let decoded = Zeroizing::new(
            STANDARD
                .decode(encoded)
                .or_else(|_| STANDARD_NO_PAD.decode(encoded))
                .ok()?,
        );
        let secret = std::str::from_utf8(&decoded).ok()?;
        (!secret.is_empty()).then(|| secret.to_owned())
    });
    let mut secrets = Vec::new();
    for mut secret in decoded {
        if secrets.contains(&secret) {
            secret.zeroize();
        } else {
            secrets.push(secret);
        }
    }
    (!secrets.is_empty()).then_some(Candidates { app_id, secrets })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_selection_preserves_platform_policy() {
        assert_eq!(
            AccessMode::Unknown.allows_preview_selection(),
            cfg!(target_os = "macos"),
        );
        assert!(AccessMode::Preview.allows_preview_selection());
        assert!(!AccessMode::Full.allows_preview_selection());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn rejected_discovered_app_id_can_use_the_remembered_pair() {
        use std::cell::RefCell;

        let mut input = Credentials::default();
        input.token = "fixture-subscription".into();
        let remembered = AppPair {
            app_id: "987654321".into(),
            secret: "manual-fixture-key".into(),
        };
        let calls = RefCell::new(Vec::new());
        let (credentials, id) = validate_token(
            input,
            false,
            Some(&remembered),
            || async {
                Ok(Candidates {
                    app_id: "123456789".into(),
                    secrets: vec!["discovered-fixture-key".into()],
                })
            },
            |candidate| {
                let app_id = candidate.app_id.clone();
                let accepted = app_id == "987654321";
                if accepted {
                    assert_eq!(candidate.app_secret, "manual-fixture-key");
                }
                calls.borrow_mut().push(app_id);
                std::future::ready(if accepted {
                    Ok("42".into())
                } else {
                    Err(QobuzError::new("account_rejected", "Qobuz rejected the token or App ID without identifying which.", false))
                })
            },
        )
        .await
        .unwrap();

        assert_eq!(id, "42");
        assert_eq!(credentials.access_mode, AccessMode::Unknown);
        assert_eq!(calls.into_inner(), ["123456789", "987654321"]);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn remembered_pair_is_not_tried_after_an_unrelated_error_or_repeated_app_id() {
        use std::cell::RefCell;

        for (first_id, remembered_id, error_code, expected_calls) in [
            ("", "987654321", "invalid_token", &["123456789"][..]),
            ("", "987654321", "provider_blocked", &["123456789"][..]),
            ("", "987654321", "rate_limit", &["123456789"][..]),
            ("", "987654321", "network", &["123456789"][..]),
            ("", "123456789", "invalid_app_id", &["123456789"][..]),
            ("987654321", "987654321", "account_rejected", &["987654321", "123456789"][..]),
        ] {
            let input = Credentials {
                token: "fixture-token".into(),
                app_id: first_id.into(),
                app_secret: if first_id.is_empty() { String::new() } else { "previous-fixture-key".into() },
                transport: TokenTransport::Query,
                access_mode: AccessMode::Unknown,
                app_pair_discovered: false,
            };
            let remembered = AppPair {
                app_id: remembered_id.into(),
                secret: "manual-fixture-key".into(),
            };
            let calls = RefCell::new(Vec::new());
            let result = validate_token(
                input,
                !first_id.is_empty(),
                Some(&remembered),
                || async { Ok(Candidates {
                    app_id: "123456789".into(),
                    secrets: vec!["discovered-fixture-key".into()],
                }) },
                |candidate| {
                    calls.borrow_mut().push(candidate.app_id.clone());
                    std::future::ready(Err(QobuzError::new(error_code, "fixture", false)))
                },
            ).await;
            assert_eq!(result.err().unwrap().code, error_code);
            assert_eq!(calls.into_inner(), expected_calls);
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn token_validation_requests_only_identity_for_fresh_manual_saved_and_reused_pairs() {
        use serde_json::json;
        use std::sync::{Arc, Mutex};

        let api_calls = Arc::new(Mutex::new(Vec::new()));
        let player_calls = Arc::new(Mutex::new(Vec::new()));
        let mut client = QobuzClient::new().unwrap();
        let salt = "z".repeat(44);
        client.fixture = Some(Arc::new(crate::qobuz::ProviderFixture {
            api: Box::new({
                let calls = api_calls.clone();
                move |endpoint, params, credentials| {
                    calls.lock().unwrap().push((endpoint, credentials.app_id.clone()));
                    assert_eq!(endpoint, "user/get");
                    assert!(params.is_empty(), "identity needs no legacy parameters");
                    assert_eq!(credentials.transport, TokenTransport::Query);
                    if credentials.app_id == "111111111" {
                        return Err(QobuzError::new("invalid_app_id", "fixture", false).into());
                    }
                    if credentials.token == "invalid-identity" {
                        return Ok(json!({"user":{"id":0}}));
                    }
                    Ok(json!({"user":{"id":"00042"}}))
                }
            }),
            player_text: Box::new({
                let calls = player_calls.clone();
                move |path| {
                    calls.lock().unwrap().push(path.to_owned());
                    match path {
                        "/login" => Ok(Zeroizing::new(
                            r#"<script src="/resources/1.2.3/bundle.js"></script>"#.into(),
                        )),
                        "/resources/1.2.3/bundle.js" => Ok(Zeroizing::new(format!(
                            r#"production:{{api:{{appId:"987654321"}}}},
                            a.initialSeed("dGVz",window.utimezone.paris),
                            {{name:"Europe/Paris",info:"dC1z",extras:"ZWNyZXQ={salt}"}}"#
                        ))),
                        _ => panic!("unexpected player path"),
                    }
                }
            }),
        }));
        for (app_id, retry, expected_api, discovered) in [
            ("", false, &["987654321"][..], true),
            ("123456789", false, &["123456789"][..], false),
            ("111111111", true, &["111111111", "987654321"][..], true),
        ] {
            let credentials = Credentials {
                token: "fixture-token".into(),
                app_id: app_id.into(),
                app_secret: if app_id.is_empty() { String::new() } else { "fixture-key".into() },
                transport: TokenTransport::Query,
                access_mode: AccessMode::Unknown,
                app_pair_discovered: false,
            };
            let (verified, id) = client.validate_account(&credentials, retry, None).await.unwrap();
            assert_eq!(id, "42");
            assert_eq!(verified.access_mode, AccessMode::Unknown);
            if discovered {
                assert_eq!(verified.app_secret, "test-secret");
            }
            let actual = std::mem::take(&mut *api_calls.lock().unwrap());
            assert_eq!(actual.iter().map(|(_, id)| id.as_str()).collect::<Vec<_>>(), expected_api);
            let actual = std::mem::take(&mut *player_calls.lock().unwrap());
            assert_eq!(actual, if discovered { vec!["/login", "/resources/1.2.3/bundle.js"] } else { vec![] });
        }
        let mut saved = Credentials {
            token: "fixture-token".into(),
            app_id: "123456789".into(),
            app_secret: "saved-key".into(),
            transport: TokenTransport::Query,
            access_mode: AccessMode::Unknown,
            app_pair_discovered: false,
        };
        let (_, id) = client.validate_account(&saved, false, None).await.unwrap();
        assert_eq!(id, "42");
        saved.token = "invalid-identity".into();
        assert_eq!(client.validate_account(&saved, true, None).await.err().unwrap().code, "invalid_response");
        assert_eq!(api_calls.lock().unwrap().len(), 2);
        assert!(player_calls.lock().unwrap().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn hosted_validation_prefers_a_signed_full_offer_and_revalidates_saved_preview() {
        use serde_json::json;
        use std::sync::{Arc, Mutex};

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut client = QobuzClient::new().unwrap();
        client.fixture = Some(Arc::new(crate::qobuz::ProviderFixture {
            api: Box::new({
                let calls = calls.clone();
                move |endpoint, params, credentials| {
                    calls.lock().unwrap().push(endpoint);
                    assert_eq!(credentials.transport, TokenTransport::HeaderOnly);
                    match endpoint {
                        "user/get" => {
                            assert!(params.is_empty());
                            Ok(json!({"id":42}))
                        }
                        "track/search" => Ok(json!({"tracks":{"items":[]}})),
                        "track/getFileUrl" => {
                            let format = params.iter().find(|(key, _)| *key == "format_id").unwrap().1.parse::<u32>().unwrap();
                            let preview = credentials.app_secret == "preview-key";
                            Ok(json!({
                                "sample":preview, "format_id":format, "url":"https://example.test/offer"
                            }))
                        }
                        _ => panic!("unexpected hosted endpoint"),
                    }
                }
            }),
            player_text: Box::new(|_| panic!("hosted verification uses an existing pair or fetched bundle")),
        }));
        let input = Credentials {
            token: "hosted-token".into(),
            app_id: "123456789".into(),
            app_secret: "preview-key".into(),
            transport: TokenTransport::HeaderOnly,
            access_mode: AccessMode::Unknown,
            app_pair_discovered: false,
        };
        let (verified, _) = client.validate_account(&input, false, None).await.unwrap();
        assert_eq!(verified.access_mode, AccessMode::Preview);
        assert_eq!(*calls.lock().unwrap(), ["user/get", "track/getFileUrl", "track/getFileUrl", "track/search"]);
        calls.lock().unwrap().clear();

        let (verified, _) = client.validate_with_candidates(&input, Some(Candidates {
            app_id: input.app_id.clone(),
            secrets: vec!["preview-key".into(), "full-key".into()],
        })).await.unwrap();
        assert_eq!(verified.access_mode, AccessMode::Full);
        assert_eq!(verified.app_secret, "full-key");
        assert_eq!(*calls.lock().unwrap(), ["user/get", "track/getFileUrl", "track/getFileUrl", "track/search", "track/getFileUrl"]);
    }

    #[tokio::test]
    async fn token_setup_only_verifies_identity_and_rediscovers_at_most_once() {
        use serde_json::json;
        use std::cell::{Cell, RefCell};
        for (saved, retry, rejection, expected, discoveries, success) in [
            (false, false, None, &["987654321"][..], 1, true),
            (true, false, None, &["123456789"][..], 0, true),
            (
                true,
                true,
                Some("invalid_app_id"),
                &["123456789", "987654321"][..],
                1,
                true,
            ),
            (
                true,
                true,
                Some("account_rejected"),
                &["123456789", "987654321"][..],
                1,
                false,
            ),
            (
                true,
                true,
                Some("invalid_token"),
                &["123456789"][..],
                0,
                false,
            ),
            (
                true,
                true,
                Some("provider_blocked"),
                &["123456789"][..],
                0,
                false,
            ),
            (true, true, Some("network"), &["123456789"][..], 0, false),
            (true, true, Some("rate_limit"), &["123456789"][..], 0, false),
            (
                true,
                false,
                Some("invalid_app_id"),
                &["123456789"][..],
                0,
                false,
            ),
        ] {
            let mut input = Credentials::default();
            input.token = "test-token".into();
            if saved {
                input.app_id = "123456789".into();
                input.app_secret = "saved-key".into();
            }
            let discovered = Cell::new(0);
            let observed = RefCell::new(Vec::new());
            let result = validate_token(
                input,
                retry,
                #[cfg(target_os = "macos")]
                None,
                || async {
                    discovered.set(discovered.get() + 1);
                    Ok(Candidates {
                        app_id: "987654321".into(),
                        secrets: vec!["new-key".into()],
                    })
                },
                |credential| {
                    observed.borrow_mut().push(credential.app_id.clone());
                    let result = match rejection {
                        Some("invalid_app_id") if retry && credential.app_id == "987654321" => {
                            verified_account_id(&json!({"user":{"id":"00042"}}))
                        }
                        Some(code) => Err(QobuzError::new(code, "fixture", false)),
                        None => verified_account_id(&json!({"id":42})),
                    };
                    std::future::ready(result)
                },
            )
            .await;
            assert_eq!(discovered.get(), discoveries);
            assert_eq!(observed.into_inner(), expected);
            assert_eq!(result.is_ok(), success);
            if let Ok((credentials, id)) = result {
                assert_eq!(id, "42");
                assert_eq!(credentials.access_mode, AccessMode::Unknown);
            }
        }
        let mut partial = Credentials::default();
        partial.token = "test-token".into();
        partial.app_id = "123456789".into();
        let result = validate_token(
            partial,
            false,
            #[cfg(target_os = "macos")]
            None,
            || async { panic!("partial pair cannot discover") },
            |_| async { panic!("partial pair cannot verify identity") },
        )
        .await;
        assert_eq!(result.err().unwrap().code, "incomplete_app_pair");
    }

    #[test]
    fn account_identity_does_not_depend_on_legacy_entitlement_metadata() {
        let account = serde_json::json!({ "user": { "id": 123 } });
        assert_eq!(verified_account_id(&account).unwrap(), "123");
        assert!(verified_account_id(&serde_json::json!({})).is_err());
    }

    #[test]
    fn finds_production_id_and_assembles_only_complete_candidates() {
        let salt = "z".repeat(44);
        let bundle = format!(
            r#"
            staging:{{api:{{appId:"111111111"}}}}, production:{{api:{{appId:"123456789"}}}},
            a.initialSeed("bad",window.utimezone.oslo),
            b.initialSeed("dGVz",window.utimezone.paris),
            {{name:"Europe/Paris",info:"dC1z",extras:"ZWNyZXQ={salt}"}}
        "#
        );
        let candidates = parse_player_bundle(&bundle).expect("discovery succeeds");
        assert_eq!(candidates.app_id, "123456789");
        assert_eq!(candidates.secrets, ["test-secret"]);
    }

    #[test]
    fn requires_one_distinct_well_formed_production_id_and_deduplicates_keys() {
        let salt = "z".repeat(44);
        let parts = format!(
            r#"
            a.initialSeed("dGVz",window.utimezone.PARIS);
            b.initialSeed("dGVz",window.utimezone.London);
            c.initialSeed("a",window.utimezone.oslo);
            d.initialSeed("bad-",window.utimezone.berlin);
            {{name:"Europe/Paris",info:"dC1z",extras:"ZWNyZXQ={salt}"}}
            {{name:"Europe/LONDON",info:"dC1z",extras:"ZWNyZXQ{salt}"}}
            {{name:"Europe/Oslo",info:"b",extras:"c"}}
            {{name:"Europe/Berlin",info:"YWJj",extras:"eHk{salt}"}}
        "#
        );
        let valid = r#"production:{api:{appId:"123456789"}}"#;
        let duplicate = r#"'production' : { "api" : { 'appId' : '123456789', other: 1 } }"#;
        let candidates = parse_player_bundle(&format!("{valid},{duplicate}{parts}")).unwrap();
        assert_eq!(candidates.secrets, ["test-secret"]);
        assert!(
            parse_player_bundle(&format!(r#"staging:{{api:{{appId:"123456789"}}}}{parts}"#))
                .is_none()
        );
        for extra in [
            r#"production:{api:{appId:"987654321"}}"#,
            r#"production:{api:{appId:"malformed"}}"#,
            r#"production:{api:{appId:123456789}}"#,
        ] {
            assert!(parse_player_bundle(&format!("{valid},{extra}{parts}")).is_none());
        }
        let only_unpadded = format!(
            r#"{valid}
            a.initialSeed("dGVz",window.utimezone.LONDON);
            {{name:"Europe/London",info:"dC1z",extras:"ZWNyZXQ{salt}"}}
        "#
        );
        assert_eq!(
            parse_player_bundle(&only_unpadded).unwrap().secrets,
            ["test-secret"]
        );
    }

    #[test]
    fn only_allows_normalized_player_bundle_paths() {
        let html = r#"<script async src='/resources/9.12-a/ab/bundle.js'></script>"#;
        assert_eq!(
            player_bundle_path(html),
            Some("/resources/9.12-a/ab/bundle.js")
        );
        for path in [
            "//other.test/resources/v1/bundle.js",
            "https://other.test/resources/v1/bundle.js",
            "https://user@play.qobuz.com/resources/v1/bundle.js",
            "/resources/../bundle.js",
            "/resources/%2e%2e/bundle.js",
            "/resources/v1/bundle.js?query=1",
            "/unknown/v1/bundle.js",
            "/resources/bundle.js",
        ] {
            assert!(player_bundle_path(&format!(r#"<script src="{path}"></script>"#)).is_none());
        }
        assert!(
            player_bundle_path(r#"<script data-src="/resources/v1/bundle.js"></script>"#).is_none()
        );
    }

    #[test]
    fn distinguishes_full_streams_and_explicit_mp3_previews() {
        use serde_json::json;
        let fail = |code| Err(QobuzError::new(code, "synthetic upstream text", true).into());
        assert_eq!(
            verify_discovered_offer(fail("invalid_app_pair"), 5).unwrap(),
            SigningProbe::Rejected
        );
        let valid =
            json!({"sample":false,"format_id":5,"url":"https://example.test/stream?sig=fixture"});
        assert_eq!(
            verify_discovered_offer(Ok(valid.clone()), 5).unwrap(),
            SigningProbe::Verified
        );
        let cd =
            json!({"sample":false,"format_id":6,"url":"https://example.test/stream?sig=fixture"});
        assert_eq!(
            verify_discovered_offer(Ok(cd), 6).unwrap(),
            SigningProbe::Verified
        );
        let mut preview = valid.clone();
        preview["sample"] = json!(true);
        assert_eq!(
            verify_discovered_offer(Ok(preview.clone()), 5).unwrap(),
            SigningProbe::Preview
        );
        assert_eq!(
            verify_discovered_offer(Ok(preview), 6).unwrap(),
            SigningProbe::Unavailable
        );
        for (field, value) in [
            ("sample", Value::Null),
            ("format_id", json!(6)),
            ("url", Value::Null),
            ("url", json!("http://example.test/stream")),
            ("url", json!("https://username@example.test/stream")),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert_eq!(
                verify_discovered_offer(Ok(invalid), 5).unwrap(),
                SigningProbe::Unavailable
            );
        }
        for code in [
            "resource_unavailable",
            "quality_unavailable",
            "invalid_response",
        ] {
            assert_eq!(
                verify_discovered_offer(fail(code), 5).unwrap(),
                SigningProbe::Unavailable
            );
        }
        assert_eq!(
            verify_discovered_offer(fail("qobuz_request"), 5)
                .unwrap_err()
                .code,
            "signing_inconclusive"
        );
        for code in ["rate_limit", "api_timeout"] {
            let failure = verify_discovered_offer(fail(code), 5).unwrap_err();
            assert_eq!(failure.code, code);
            assert!(!failure.message.contains("synthetic upstream text"));
        }
    }

    #[tokio::test]
    async fn unavailable_initial_track_retries_catalog_tracks_until_a_full_offer() {
        use serde_json::json;
        use std::cell::RefCell;

        let tracks = json!({"tracks":{"items":[
            {"id":5966783,"streamable":true},
            {"id":91,"streamable":false},
            {"id":0,"streamable":true},
            {"id":"x","streamable":true},
            {"id":42,"streamable":true},
            {"id":"42","streamable":true},
            {"id":43,"streamable":true},
            {"id":44,"streamable":true}
        ]}});
        let mut attempts = SigningProbes::new();
        let probes = RefCell::new(Vec::new());
        let outcome = probe_candidate(
            &mut attempts,
            |track: String, format_id| {
                probes.borrow_mut().push((track.clone(), format_id));
                async move {
                    let offer = json!({
                        "sample": !(track == "42" && format_id == 6),
                        "format_id":format_id,
                        "url":"https://example.test/stream?sig=fixture"
                    });
                    verify_discovered_offer(Ok(offer), format_id)
                }
            },
            || async { Ok(alternate_track_ids(&tracks)) },
        )
        .await
        .unwrap();
        assert_eq!(outcome, SigningProbe::Verified);
        assert_eq!(attempts.alternatives.unwrap(), ["42", "43"]);
        assert_eq!(
            probes.into_inner(),
            [
                (PROBE_TRACK.to_owned(), 5),
                (PROBE_TRACK.to_owned(), 6),
                ("42".to_owned(), 5),
                ("42".to_owned(), 6)
            ]
        );
        assert_eq!(attempts.remaining, SIGNING_PROBE_LIMIT - 4);
    }

    #[tokio::test]
    async fn uses_verified_mp3_preview_after_exhausting_full_offer_alternatives() {
        use serde_json::json;

        let mut probes = SigningProbes::new();
        let outcome = probe_candidate(
            &mut probes,
            |track, format_id| async move {
                let response = if track == PROBE_TRACK {
                    json!({})
                } else {
                    json!({"sample":true,"format_id":format_id,"url":"https://example.test/preview"})
                };
                verify_discovered_offer(Ok(response), format_id)
            },
            || async { Ok(vec!["42".to_owned(), "43".to_owned()]) },
        )
        .await
        .unwrap();
        assert_eq!(outcome, SigningProbe::Preview);
        assert_eq!(probes.remaining, SIGNING_PROBE_LIMIT - 6);
    }

    #[tokio::test]
    async fn hosted_candidates_share_sixteen_probes_and_two_alternate_tracks() {
        use std::cell::{Cell, RefCell};

        let tracks = serde_json::json!({"tracks":{"items":[
            {"id":42,"streamable":true}, {"id":43,"streamable":true},
            {"id":44,"streamable":true}
        ]}});
        let calls = RefCell::new(Vec::new());
        let searches = Cell::new(0);
        let mut probes = SigningProbes::new();
        for key in 0..4 {
            assert_eq!(
                probe_candidate(
                    &mut probes,
                    |track, format| {
                        calls.borrow_mut().push((key, track, format));
                        std::future::ready(Ok(SigningProbe::Unavailable))
                    },
                    || {
                        searches.set(searches.get() + 1);
                        std::future::ready(Ok(alternate_track_ids(&tracks)))
                    },
                )
                .await
                .unwrap(),
                SigningProbe::Unavailable
            );
        }

        assert_eq!(searches.get(), 1);
        assert_eq!(probes.remaining, 0);
        let calls = calls.into_inner();
        assert_eq!(calls.len(), 16);
        for (index, (key, track, format)) in calls.iter().enumerate() {
            assert_eq!(*key, index / 6);
            assert_eq!(track, [PROBE_TRACK, "42", "43"][index % 6 / 2]);
            assert_eq!(*format, [5, 6][index % 2]);
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    #[ignore = "exercises the real 45-second validation deadline"]
    async fn validation_times_out_at_forty_five_seconds() {
        let start = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(50),
            validation_deadline(std::future::pending::<Result<(), QobuzError>>()),
        )
        .await
        .expect("validation must end before the guard timeout");
        assert_eq!(result.unwrap_err().code, "validation_timeout");
        assert!(start.elapsed() >= Duration::from_secs(45));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test(start_paused = true)]
    async fn rediscovery_shares_the_original_forty_five_second_deadline() {
        use std::cell::Cell;

        let input = Credentials {
            token: "test-token".into(),
            app_id: "123456789".into(),
            app_secret: "fixture-key".into(),
            transport: TokenTransport::Query,
            access_mode: AccessMode::Unknown,
            app_pair_discovered: false,
        };
        let identity_calls = Cell::new(0);
        let discovery_calls = Cell::new(0);
        let start = tokio::time::Instant::now();
        let result = validation_deadline(validate_token(
            input,
            true,
            None,
            || async {
                discovery_calls.set(discovery_calls.get() + 1);
                tokio::time::sleep(Duration::from_secs(20)).await;
                Ok(Candidates { app_id: "987654321".into(), secrets: vec!["fresh-key".into()] })
            },
            |_| async {
                identity_calls.set(identity_calls.get() + 1);
                tokio::time::sleep(Duration::from_secs(30)).await;
                Err(QobuzError::new("invalid_app_id", "fixture", false))
            },
        )).await;
        assert_eq!(result.err().unwrap().code, "validation_timeout");
        assert_eq!(start.elapsed(), Duration::from_secs(45));
        assert_eq!((identity_calls.get(), discovery_calls.get()), (1, 1));
    }

    #[tokio::test]
    async fn a_definite_rejection_or_throttle_does_not_probe_other_tracks() {
        for failure_code in ["invalid_app_pair", "rate_limit"] {
            let mut probes = SigningProbes::new();
            let result = probe_candidate(
                &mut probes,
                |track: String, format_id| async move {
                    assert_eq!((track.as_str(), format_id), (PROBE_TRACK, 5));
                    verify_discovered_offer(
                        Err(QobuzError::new(failure_code, "fixture", false).into()),
                        format_id,
                    )
                },
                || async { Err(signing_inconclusive()) },
            )
            .await;
            if failure_code == "invalid_app_pair" {
                assert_eq!(result.unwrap(), SigningProbe::Rejected);
            } else {
                assert_eq!(result.unwrap_err().code, failure_code);
            }
            assert_eq!(probes.remaining, SIGNING_PROBE_LIMIT - 1);
        }
    }
}
