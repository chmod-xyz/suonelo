mod auth;
mod hosted;
#[cfg(target_os = "macos")]
mod hosted_macos;
mod lyrics;
mod resources;
pub mod search;
mod stream;

pub use auth::{AccessMode, Credentials};
#[cfg(target_os = "macos")]
pub(crate) use auth::{AppPair, valid_app_id, valid_token};
pub use hosted::EmailSignInPhase;
pub use lyrics::{Lyrics, LyricsSource};
pub use resources::{
    BookmarkSummary, CanonicalResource, ResolvedResource, ResolvedTrack, ResourceKey, ResourceKind,
    normalize_link,
};
pub use stream::StreamOffer;

use crate::recovery::{ApiOutcome, Recovery, RecoveryDto, RetryBudget};
use md5::{Digest, Md5};
use reqwest::{Client, RequestBuilder, Response, StatusCode, header::HeaderValue, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fmt,
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use unicode_normalization::UnicodeNormalization;
use url::Url;

const API: &str = "https://www.qobuz.com/api.json/0.2/";
const PLAYER: &str = "https://play.qobuz.com";
const PLAYER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
const JSON_LIMIT: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct QobuzError {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
}

impl QobuzError {
    fn new(code: &'static str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
        }
    }

    fn network() -> Self {
        Self::new(
            "network",
            "Could not connect to Qobuz. Check your connection and try again.",
            true,
        )
    }

    fn invalid_response(message: &str) -> Self {
        Self::new("invalid_response", message, true)
    }
}

impl fmt::Display for QobuzError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for QobuzError {}

#[derive(Clone)]
pub struct QobuzClient {
    api: Client,
    public: Client,
    recovery: Arc<Recovery>,
    #[cfg(target_os = "macos")]
    media_signer: Arc<tokio::sync::Mutex<Option<stream::MediaSigner>>>,
    #[cfg(all(target_os = "macos", test))]
    pub(crate) fixture: Option<Arc<ProviderFixture>>,
}

#[cfg(all(target_os = "macos", test))]
type FixtureApi = dyn Fn(&'static str, Params, &Credentials) -> Result<Value, ApiError> + Send + Sync;

#[cfg(all(target_os = "macos", test))]
pub(crate) struct ProviderFixture {
    pub(crate) api: Box<FixtureApi>,
    pub(crate) player_text: Box<dyn Fn(&str) -> Result<zeroize::Zeroizing<String>, QobuzError> + Send + Sync>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenTransport {
    #[default]
    Query,
    HeaderOnly,
}

impl TokenTransport {
    pub(super) fn is_query(&self) -> bool {
        *self == Self::Query
    }
}

type Params = Vec<(&'static str, String)>;

#[derive(Clone)]
pub(crate) struct ApiError {
    error: QobuzError,
    status: Option<StatusCode>,
}

impl From<QobuzError> for ApiError {
    fn from(error: QobuzError) -> Self {
        Self {
            error,
            status: None,
        }
    }
}

impl QobuzClient {
    pub fn new() -> Result<Self, QobuzError> {
        let api = Client::builder()
            .https_only(true)
            .redirect(Policy::none())
            .user_agent(PLAYER_UA)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| QobuzError::network())?;
        let public = Client::builder()
            .https_only(true)
            .connect_timeout(Duration::from_secs(12))
            .read_timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| QobuzError::network())?;
        Ok(Self {
            api,
            public,
            recovery: Arc::new(Recovery::default()),
            #[cfg(target_os = "macos")]
            media_signer: Arc::new(tokio::sync::Mutex::new(None)),
            #[cfg(all(target_os = "macos", test))]
            fixture: None,
        })
    }

    #[cfg(test)]
    pub fn http_client(&self) -> &Client {
        &self.public
    }

    pub fn recovery(&self) -> &Arc<Recovery> {
        &self.recovery
    }

    pub fn recovery_status(&self, account: Option<&Credentials>) -> RecoveryDto {
        self.recovery.status(account.map(account_key))
    }

    pub fn retry_connection(&self) {
        self.recovery.retry_connection();
    }

    pub fn retry_account_connection(&self, account: &Credentials) {
        self.recovery.retry_account_connection(account_key(account));
    }

    pub fn api_waiting_reason(&self, account: &Credentials) -> Option<String> {
        self.recovery.api_waiting_reason(account_key(account))
    }

    async fn request(
        &self,
        endpoint: &'static str,
        params: impl Fn() -> Params,
        credentials: &Credentials,
    ) -> Result<Value, ApiError> {
        self.request_with_budget(endpoint, params, credentials, &RetryBudget::default())
            .await
    }

    fn authenticated_request(
        &self, endpoint: &'static str, mut params: Params, credentials: &Credentials,
    ) -> Result<RequestBuilder, ApiError> {
        let request = self.api.get(format!("{API}{endpoint}"))
            .header("X-App-Id", &credentials.app_id);
        match credentials.transport {
            TokenTransport::Query => {
                params.push(("user_auth_token", credentials.token.clone()));
                Ok(request.query(&params))
            }
            TokenTransport::HeaderOnly => {
                let mut token = HeaderValue::from_str(&credentials.token).map_err(|_| {
                    QobuzError::new("invalid_token", "Qobuz returned an unusable session token.", false)
                })?;
                token.set_sensitive(true);
                Ok(request.query(&params).header("X-User-Auth-Token", token))
            }
        }
    }

    async fn request_with_budget(
        &self,
        endpoint: &'static str,
        params: impl Fn() -> Params,
        credentials: &Credentials,
        budget: &RetryBudget,
    ) -> Result<Value, ApiError> {
        #[cfg(all(target_os = "macos", test))]
        if let Some(fixture) = &self.fixture {
            return (fixture.api)(endpoint, params(), credentials);
        }
        let request = self.recovery.request(account_key(credentials));
        loop {
            let permit = request.acquire().await;
            let response = self
                .authenticated_request(endpoint, params(), credentials)?
                .header("Accept", "application/json, text/plain, */*")
                .header("Accept-Language", "en-US,en;q=0.9")
                .header("Origin", PLAYER)
                .header("Referer", format!("{PLAYER}/"))
                .header(
                    "Sec-Ch-Ua",
                    "\"Chromium\";v=\"124\", \"Google Chrome\";v=\"124\", \"Not-A.Brand\";v=\"99\"",
                )
                .header("Sec-Ch-Ua-Mobile", "?0")
                .header("Sec-Ch-Ua-Platform", "\"Windows\"")
                .header("Sec-Fetch-Dest", "empty")
                .header("Sec-Fetch-Mode", "cors")
                .header("Sec-Fetch-Site", "same-site")
                .send()
                .await;
            let result = match response {
                Ok(response) if response.status() == StatusCode::TOO_MANY_REQUESTS => {
                    let retry_after = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|header| header.to_str().ok());
                    permit.finish(ApiOutcome::RateLimited(retry_after));
                    if budget.single_attempt {
                        return Err(QobuzError::new("rate_limit", "Qobuz is limiting requests. Try again after the cooldown.", true).into());
                    }
                    continue;
                }
                Ok(response) => api_response(response, credentials, endpoint).await,
                Err(error) => {
                    let failure = network_failure(&error);
                    if budget.defer_no_response
                        && endpoint == "track/getFileUrl"
                        && no_response(&error)
                    {
                        let outcome = if error.is_timeout() {
                            ApiOutcome::TimedOut
                        } else {
                            ApiOutcome::Transient
                        };
                        permit.finish(outcome);
                        return Err(
                            QobuzError::new("stream_no_response", failure.message, true).into()
                        );
                    }
                    Err(failure.into())
                }
            };
            let outcome = match &result {
                Ok(_) => ApiOutcome::Success,
                Err(error) if error.error.code == "rate_limit" => ApiOutcome::RateLimited(None),
                Err(error) if error.error.code == "api_timeout" => ApiOutcome::TimedOut,
                Err(error) if error.error.code == "network" => ApiOutcome::Transient,
                Err(_) => ApiOutcome::Rejected,
            };
            let probing = permit.probing();
            permit.finish(outcome);
            if budget.single_attempt {
                return result;
            }
            match outcome {
                ApiOutcome::RateLimited(_) => continue,
                ApiOutcome::TimedOut | ApiOutcome::Transient => {
                    let reason = if matches!(outcome, ApiOutcome::TimedOut) {
                        "Qobuz API request timed out. Retrying…"
                    } else {
                        "Qobuz API connection was interrupted. Retrying…"
                    };
                    if budget.wait(reason).await {
                        continue;
                    }
                    if probing {
                        request.block_probe();
                        continue;
                    }
                }
                _ => (),
            }
            return result;
        }
    }

    async fn metadata(
        &self,
        endpoint: &'static str,
        params: Params,
        credentials: &Credentials,
    ) -> Result<Value, QobuzError> {
        let mut value = self
            .request(endpoint, || params.clone(), credentials)
            .await
            .map_err(|failure| failure.error)?;
        if !value.is_object() {
            return Err(QobuzError::invalid_response(
                "Qobuz did not return a catalog record.",
            ));
        }
        clean_metadata(&mut value);
        Ok(value)
    }
}

fn account_key(credentials: &Credentials) -> u64 {
    let mut hash = DefaultHasher::new();
    credentials.token.hash(&mut hash);
    hash.finish()
}

fn network_failure(error: &reqwest::Error) -> QobuzError {
    if error.is_timeout() {
        QobuzError::new("api_timeout", "The Qobuz API request timed out.", true)
    } else {
        QobuzError::network()
    }
}

fn no_response(error: &reqwest::Error) -> bool {
    use std::{error::Error, io::ErrorKind};
    if error.is_timeout() || error.is_connect() {
        return true;
    }
    let mut cause = error.source();
    while let Some(error) = cause {
        if error.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::UnexpectedEof
            )
        }) {
            return true;
        }
        cause = error.source();
    }
    false
}

async fn api_response(response: Response, credentials: &Credentials, endpoint: &'static str) -> Result<Value, ApiError> {
    let classify = |status, body: &Value| {
        if endpoint == "user/get"
            && let Some(error) = identity_rejection(status, body)
        {
            return ApiError { error, status: Some(status) };
        }
        api_failure(status, body, credentials)
    };
    let status = response.status();
    if status.is_server_error() {
        return Err(classify(status, &Value::Null));
    }
    let bytes = bounded_body(response, JSON_LIMIT).await?;
    let parsed = serde_json::from_slice::<Value>(&bytes);
    if !status.is_success() {
        return Err(classify(status, &parsed.unwrap_or_default()));
    }
    let value = parsed
        .map_err(|_| QobuzError::invalid_response("Qobuz returned an unreadable API response."))?;
    if value.get("status").and_then(Value::as_str) == Some("error") {
        let status = value
            .get("code")
            .and_then(u32_value)
            .and_then(|code| u16::try_from(code).ok())
            .and_then(|code| StatusCode::from_u16(code).ok())
            .unwrap_or(StatusCode::BAD_REQUEST);
        return Err(classify(status, &value));
    }
    Ok(value)
}

async fn bounded_body(mut response: Response, limit: usize) -> Result<Vec<u8>, QobuzError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| network_failure(&error))?
    {
        if chunk.len() > limit - body.len() {
            return Err(QobuzError::invalid_response(
                "The service response was too large.",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn identity_rejection(status: StatusCode, body: &Value) -> Option<QobuzError> {
    if !matches!(status, StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return None;
    }
    #[cfg(target_os = "macos")]
    let message = body.get("message").and_then(Value::as_str)
        .or_else(|| body.pointer("/error/message").and_then(Value::as_str));
    #[cfg(not(target_os = "macos"))]
    let message = body.get("message").or_else(|| body.pointer("/error/message"))
        .and_then(Value::as_str);
    let reason = message.unwrap_or_default().to_ascii_lowercase();
    let app_id = ["app id", "app_id", "application id"].iter().any(|part| reason.contains(part));
    let token = reason.contains("token") && ["invalid", "expired", "missing", "revoked", "rejected"]
        .iter().any(|part| reason.contains(part));
    let blocked = ["blocked", "suspended", "captcha", "access denied"]
        .iter().any(|part| reason.contains(part));
    let (code, message) = if blocked || (status == StatusCode::FORBIDDEN && !app_id && !token) {
        ("provider_blocked", "Qobuz blocked account verification.")
    } else if token && !app_id {
        ("invalid_token", "Qobuz rejected the account token.")
    } else if app_id && !reason.contains("token") {
        ("invalid_app_id", "Qobuz rejected the App ID.")
    } else {
        ("account_rejected", "Qobuz did not identify the rejected credential.")
    };
    Some(QobuzError::new(code, message, false))
}

fn api_failure(status: StatusCode, body: &Value, credentials: &Credentials) -> ApiError {
    let upstream = body
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| body.pointer("/error/message").and_then(Value::as_str));
    let message = upstream.and_then(|reason| safe_reason(reason, Some(credentials)));
    let reason = upstream.unwrap_or_default().to_ascii_lowercase();
    let (code, default_message, retryable) = if status == StatusCode::TOO_MANY_REQUESTS {
        (
            "rate_limit",
            "Qobuz is limiting requests. Try again shortly.",
            true,
        )
    } else if status.is_server_error() {
        (
            "network",
            "Qobuz could not complete the request. Try again shortly.",
            true,
        )
    } else if [
        "app id",
        "app_id",
        "app secret",
        "request signature",
        "invalid signature",
        "request_sig",
    ]
    .iter()
    .any(|part| reason.contains(part))
    {
        (
            "invalid_app_pair",
            "Qobuz rejected the application ID or secret.",
            false,
        )
    } else if status == StatusCode::UNAUTHORIZED
        || (reason.contains("token")
            && ["invalid", "expired", "missing"]
                .iter()
                .any(|detail| reason.contains(detail)))
    {
        ("invalid_token", "Qobuz rejected the account token.", false)
    } else if [
        "subscription",
        "entitlement",
        "eligible",
        "not authorized to stream",
    ]
    .iter()
    .any(|part| reason.contains(part))
    {
        (
            "missing_entitlement",
            "This account does not have Qobuz streaming access.",
            false,
        )
    } else if [
        "formatrestricted",
        "format availability",
        "format not available",
        "unavailable format",
        "not available in this format",
        "not available for this format",
        "invalid format",
        "quality not available",
    ]
    .iter()
    .any(|part| reason.contains(part))
    {
        (
            "quality_unavailable",
            "Qobuz does not offer the requested audio quality.",
            false,
        )
    } else if status == StatusCode::NOT_FOUND
        || [
            "not found",
            "not available in your",
            "territory",
            "not streamable",
            "unavailable",
            "not available",
            "invalid album_id",
            "invalid track_id",
            "invalid artist_id",
            "invalid label_id",
            "invalid playlist_id",
        ]
        .iter()
        .any(|part| reason.contains(part))
    {
        (
            "resource_unavailable",
            "Qobuz could not find or provide this resource for the current account.",
            false,
        )
    } else {
        ("qobuz_request", "Qobuz rejected this request.", false)
    };
    let detail = match (code, message) {
        ("qobuz_request" | "resource_unavailable" | "quality_unavailable", Some(message)) => {
            format!("{default_message} Qobuz reported: {message}")
        }
        ("qobuz_request", None) => format!("{default_message} HTTP {}.", status.as_u16()),
        _ => default_message.to_owned(),
    };
    ApiError {
        error: QobuzError::new(code, detail, retryable),
        status: Some(status),
    }
}

fn safe_reason(value: &str, credentials: Option<&Credentials>) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    if value.contains("://")
        || value.contains('@')
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("signature")
        || lower.contains("request_sig")
        || credentials.is_some_and(|credentials| {
            [
                &credentials.token,
                &credentials.app_id,
                &credentials.app_secret,
            ]
            .into_iter()
            .any(|secret| !secret.is_empty() && value.contains(secret))
        })
    {
        return None;
    }
    let reason: String = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|character| !character.is_control())
        .take(240)
        .collect();
    (!reason.is_empty()).then_some(reason)
}

fn clean_metadata(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|name, _| {
                !matches!(
                    name.to_ascii_lowercase().as_str(),
                    "user_auth_token"
                        | "auth_token"
                        | "access_token"
                        | "token"
                        | "app_secret"
                        | "app_id"
                        | "request_sig"
                        | "request_ts"
                        | "credential"
                        | "lyrics_url"
                        | "stream_url"
                )
            });
            for value in fields.values_mut() {
                clean_metadata(value);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(clean_metadata),
        Value::String(text) if !text.contains("://") => {
            *text = text.replace("...", "…").nfc().collect();
        }
        _ => {}
    }
}

fn u32_value(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .or_else(|| value.as_str()?.parse().ok())
}

fn f64_value(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
}

fn nonempty(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn media_url(value: &Value) -> Option<Url> {
    let url = Url::parse(nonempty(value)?).ok()?;
    (url.scheme() == "https" && url.username().is_empty() && url.password().is_none())
        .then_some(url)
}

fn signed_params(signature_base: &str, secret: &str) -> Params {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let timestamp = timestamp.to_string();
    let mut digest = Md5::new();
    digest.update(signature_base);
    digest.update(&timestamp);
    digest.update(secret);
    vec![
        ("request_ts", timestamp),
        ("request_sig", format!("{:x}", digest.finalize())),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identity_errors_distinguish_explicit_rejection_and_blocking() {
        for (status, message, code) in [
            (StatusCode::UNAUTHORIZED, "Invalid credentials from params", "account_rejected"),
            (StatusCode::BAD_REQUEST, "Invalid app_id", "invalid_app_id"),
            (StatusCode::BAD_REQUEST, "Invalid app_id or token", "account_rejected"),
            (StatusCode::UNAUTHORIZED, "Invalid user_auth_token", "invalid_token"),
            (StatusCode::FORBIDDEN, "Access denied", "provider_blocked"),
            (StatusCode::FORBIDDEN, "Token blocked", "provider_blocked"),
        ] {
            assert_eq!(identity_rejection(status, &json!({"message": message})).unwrap().code, code);
        }
        assert!(identity_rejection(StatusCode::TOO_MANY_REQUESTS, &json!({})).is_none());
    }

    #[test]
    fn email_session_token_is_sensitive_and_excluded_from_the_url() {
        let client = QobuzClient::new().unwrap();
        let credentials = Credentials {
            token: "synthetic-session-value".into(),
            app_id: "123456789".into(),
            app_secret: "fixture".into(),
            transport: TokenTransport::HeaderOnly,
            access_mode: AccessMode::Unknown,
            #[cfg(target_os = "macos")]
            app_pair_discovered: false,
        };
        for endpoint in ["user/get", "track/getFileUrl", "album/search"] {
            let request = client.authenticated_request(endpoint, vec![("limit", "1".into())], &credentials)
                .ok().unwrap().build().unwrap();
            assert!(!request.url().as_str().contains(&credentials.token));
            assert!(!request.url().query_pairs().any(|(key, _)| key == "user_auth_token"));
            let header = &request.headers()["x-user-auth-token"];
            assert_eq!(header, credentials.token.as_str());
            assert!(header.is_sensitive());
        }
    }

    #[test]
    fn distinguishes_rejections_without_returning_secrets_or_signed_urls() {
        let credentials = Credentials {
            token: "test-account-credentials".to_owned(),
            app_id: "999999999".to_owned(),
            app_secret: "test-app-secret".to_owned(),
            transport: TokenTransport::Query,
            access_mode: AccessMode::Unknown,
            #[cfg(target_os = "macos")]
            app_pair_discovered: false,
        };
        let bad_token = api_failure(
            StatusCode::BAD_REQUEST,
            &json!({
                "message": "The user_auth_token test-account-credentials is invalid."
            }),
            &credentials,
        );
        assert_eq!(bad_token.error.code, "invalid_token");
        assert!(!bad_token.error.message.contains(&credentials.token));
        let unavailable = api_failure(
            StatusCode::NOT_FOUND,
            &json!({
                "message": "Release is not available in your territory."
            }),
            &credentials,
        );
        assert_eq!(unavailable.error.code, "resource_unavailable");
        assert!(unavailable.error.message.contains("territory"));
        let response_with_url = api_failure(
            StatusCode::NOT_FOUND,
            &json!({
                "message": "Could not find https://example.test/stream?signature=private"
            }),
            &credentials,
        );
        assert!(!response_with_url.error.message.contains("example.test"));
    }
}
