use super::{API, Credentials, PLAYER, QobuzClient, QobuzError, TokenTransport, bounded_body};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use regex::Regex;
use reqwest::{RequestBuilder, StatusCode, header::HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{
    Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent, ipc::Channel,
    webview::NewWindowResponse,
};
use tokio::sync::oneshot;
use url::Url;
use zeroize::Zeroizing;

const HOSTED_SIGN_IN: &str = "https://www.qobuz.com/signin/oauth";
const WAIT_FOR_BROWSER: Duration = Duration::from_secs(300);
const MAX_EXCHANGE_RESPONSE: usize = 128 * 1024;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EmailSignInPhase {
    Waiting,
    Validating,
}

struct HostedAttempt {
    state: Zeroizing<String>,
    verifier: Zeroizing<String>,
    callback: Url,
    authorization: Url,
}

impl HostedAttempt {
    fn new(app_id: &str) -> Result<Self, QobuzError> {
        let state = random_nonce::<32>()?;
        let verifier = random_nonce::<48>()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        // The owned webview consumes this navigation before a request can reach loopback.
        let callback = Url::parse(&format!("http://localhost:43871/suonelo/qobuz/{}", &*state))
            .expect("fixed loopback origin and URL-safe state");
        let mut authorization = Url::parse(HOSTED_SIGN_IN).expect("fixed Qobuz sign-in URL");
        authorization
            .query_pairs_mut()
            .append_pair("ext_app_id", app_id)
            .append_pair("redirect_url", callback.as_str())
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(Self {
            state,
            verifier,
            callback,
            authorization,
        })
    }

    async fn code(
        &self,
        parent: WebviewWindow,
        phase: &Channel<EmailSignInPhase>,
    ) -> Result<BrowserWait, QobuzError> {
        let callback = self.callback.clone();
        let state = self.state.clone();
        let (send, receive) = oneshot::channel();
        let send = Arc::new(Mutex::new(Some(send)));
        let navigation = send.clone();
        let label = format!("qobuz-sign-in-{}", &*self.state);
        #[cfg(target_os = "macos")]
        let initial_url = Url::parse("about:blank").expect("fixed blank URL");
        #[cfg(not(target_os = "macos"))]
        let initial_url = self.authorization.clone();
        let window = WebviewWindowBuilder::new(
            parent.app_handle(),
            label,
            WebviewUrl::External(initial_url),
        )
        .parent(&parent)
        .map_err(|_| cannot_open())?
        .title("Sign in to Qobuz")
        .inner_size(580.0, 720.0)
        .min_inner_size(360.0, 480.0)
        .center()
        .incognito(true)
        .devtools(false)
        .on_new_window(|_, _| NewWindowResponse::Deny)
        .on_download(|_, _| false)
        .on_navigation(move |url| {
            if is_callback(url, &callback) {
                if let Some(sender) = navigation.lock().expect("sign-in callback mutex").take() {
                    let _ = sender.send(callback_code(url, &state));
                }
                return false;
            }
            provider_page(url)
        })
        .build()
        .map_err(|_| cannot_open())?;
        window.on_window_event(move |event| {
            if matches!(event, WindowEvent::Destroyed)
                && let Some(sender) = send.lock().expect("sign-in callback mutex").take()
            {
                let _ = sender.send(Err(window_closed()));
            }
        });
        let browser = BrowserWait { window, receive };
        #[cfg(target_os = "macos")]
        {
            // Install the frame policy before the provider document can load.
            super::hosted_macos::install(&browser.window, self.callback.clone())
                .await
                .map_err(|_| cannot_open())?;
            browser
                .window
                .navigate(self.authorization.clone())
                .map_err(|_| cannot_open())?;
        }
        phase
            .send(EmailSignInPhase::Waiting)
            .map_err(|_| window_closed())?;
        Ok(browser)
    }
}

struct BrowserWait {
    window: WebviewWindow,
    receive: oneshot::Receiver<Result<Zeroizing<String>, QobuzError>>,
}

impl Drop for BrowserWait {
    fn drop(&mut self) {
        let _ = self.window.destroy();
    }
}

fn random_nonce<const N: usize>() -> Result<Zeroizing<String>, QobuzError> {
    let mut bytes = Zeroizing::new([0_u8; N]);
    OsRng
        .try_fill_bytes(&mut bytes[..])
        .map_err(|_| cannot_open())?;
    Ok(Zeroizing::new(URL_SAFE_NO_PAD.encode(&bytes[..])))
}

pub(super) fn provider_page(url: &Url) -> bool {
    url.scheme() == "https"
        && matches!(url.host_str(), Some("www.qobuz.com" | "qobuz.com"))
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

pub(super) fn is_callback(url: &Url, callback: &Url) -> bool {
    url.origin() == callback.origin()
        && url.path() == callback.path()
        && url.username().is_empty()
        && url.password().is_none()
}

fn callback_code(url: &Url, state: &str) -> Result<Zeroizing<String>, QobuzError> {
    if url.fragment().is_some() || url.query().is_none_or(|query| query.len() > 8192) {
        return Err(invalid_callback());
    }
    let mut code = None;
    let mut echoed = false;
    for (field, value) in url.query_pairs() {
        match field.as_ref() {
            "code_autorisation"
                if code.is_none()
                    && !value.is_empty()
                    && value.len() <= 4096
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b)) =>
            {
                code = Some(Zeroizing::new(value.into_owned()));
            }
            // The nonce in the exact callback path binds the attempt even when
            // Qobuz sends only its authorization code. Any echoed state must agree.
            "state" if !echoed && value == state => echoed = true,
            "error" if value == "access_denied" => return Err(window_closed()),
            _ => return Err(invalid_callback()),
        }
    }
    code.ok_or_else(invalid_callback)
}

fn exchange_key(bundle: &str) -> Option<Zeroizing<String>> {
    // This exchange value is delivered by Qobuz to every web player.
    // Read it from the same bundle as the app ID instead of embedding a value.
    let anchor = Regex::new(
        r#"authenticate\(\{\s*privateKey\s*:\s*"([A-Za-z0-9_-]{8,128})"\s*,\s*code\s*:"#,
    )
    .ok()?;
    let mut values = anchor
        .captures_iter(bundle)
        .filter_map(|capture| capture.get(1).map(|value| value.as_str()));
    let key = values.next()?;
    values
        .all(|value| value == key)
        .then(|| Zeroizing::new(key.to_owned()))
}

#[derive(Deserialize)]
struct ExchangeResponse {
    #[serde(alias = "user_auth_token")]
    token: String,
    user_id: Value,
}

impl Drop for ExchangeResponse {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.token);
    }
}

impl QobuzClient {
    pub async fn sign_in_with_email(
        &self,
        parent: WebviewWindow,
        phase: Channel<EmailSignInPhase>,
    ) -> Result<(Credentials, String), QobuzError> {
        let bundle = tokio::time::timeout(Duration::from_secs(45), self.player_bundle())
            .await
            .map_err(|_| timed_out())??;
        let candidates =
            super::auth::parse_player_bundle(&bundle).ok_or_else(automatic_setup_failed)?;
        let key = exchange_key(&bundle).ok_or_else(automatic_setup_failed)?;
        drop(bundle);

        let attempt = HostedAttempt::new(&candidates.app_id)?;
        let mut browser = attempt.code(parent, &phase).await?;
        let code = tokio::time::timeout(WAIT_FOR_BROWSER, &mut browser.receive)
            .await
            .map_err(|_| timed_out())?
            .map_err(|_| window_closed())??;
        drop(browser);
        phase
            .send(EmailSignInPhase::Validating)
            .map_err(|_| window_closed())?;

        tokio::time::timeout(Duration::from_secs(60), async {
            let (session, issued_id) = self
                .exchange_email_code(&candidates.app_id, &key, &code, &attempt.verifier)
                .await?;
            let (session, verified_id) = self
                .validate_with_candidates(&session, Some(candidates))
                .await?;
            if verified_id != issued_id {
                return Err(QobuzError::new(
                    "email_identity_mismatch",
                    "Qobuz returned conflicting account identities. Sign in again.",
                    true,
                ));
            }
            Ok((session, verified_id))
        })
        .await
        .map_err(|_| timed_out())?
    }

    fn email_exchange_request(
        &self,
        app_id: &str,
        key: &str,
        code: &str,
        verifier: &str,
    ) -> RequestBuilder {
        self.api
            .post(format!("{API}oauth/callback"))
            .header("X-App-Id", app_id)
            .header("Origin", PLAYER)
            .header("Referer", format!("{PLAYER}/"))
            .header("Accept", "application/json")
            .form(&[
                ("code", code),
                ("private_key", key),
                ("code_verifier", verifier),
            ])
    }

    async fn exchange_email_code(
        &self,
        app_id: &str,
        key: &str,
        code: &str,
        verifier: &str,
    ) -> Result<(Credentials, String), QobuzError> {
        const API_HOST: &str = "www.qobuz.com";
        let throttled = || {
            QobuzError::new(
                "rate_limit",
                "Qobuz is limiting sign-in requests. Try again after the cooldown.",
                true,
            )
        };
        if self.recovery.host_is_throttled(API_HOST) {
            return Err(throttled());
        }
        let response = self
            .email_exchange_request(app_id, key, code, verifier)
            .send()
            .await
            .map_err(|error| super::network_failure(&error))?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|header| header.to_str().ok());
            self.recovery.rate_limited_host(API_HOST, retry_after);
            return Err(throttled());
        }
        if !response.status().is_success() {
            return Err(QobuzError::new(
                "email_exchange_rejected",
                "Qobuz could not complete this email sign-in. Open the sign-in window again to retry.",
                true,
            ));
        }
        let bytes = Zeroizing::new(bounded_body(response, MAX_EXCHANGE_RESPONSE).await?);
        let mut exchanged: ExchangeResponse =
            serde_json::from_slice(&bytes).map_err(|_| invalid_callback())?;
        let issued_id = exchanged
            .user_id
            .as_u64()
            .or_else(|| exchanged.user_id.as_str()?.parse().ok())
            .filter(|id| *id > 0)
            .ok_or_else(invalid_callback)?;
        if exchanged.token.is_empty()
            || exchanged.token.len() > 4096
            || exchanged.token.chars().any(char::is_whitespace)
            || HeaderValue::from_str(&exchanged.token).is_err()
        {
            return Err(invalid_callback());
        }
        Ok((
            Credentials {
                token: std::mem::take(&mut exchanged.token),
                app_id: app_id.to_owned(),
                app_secret: String::new(),
                transport: TokenTransport::HeaderOnly,
                access_mode: super::AccessMode::Unknown,
                #[cfg(target_os = "macos")]
                app_pair_discovered: false,
            },
            issued_id.to_string(),
        ))
    }
}

fn cannot_open() -> QobuzError {
    QobuzError::new(
        "email_window_unavailable",
        "Suonelo could not open the Qobuz sign-in window.",
        true,
    )
}

fn window_closed() -> QobuzError {
    QobuzError::new("email_cancelled", "Qobuz sign-in was cancelled.", true)
}

fn timed_out() -> QobuzError {
    QobuzError::new(
        "email_timeout",
        "Qobuz sign-in timed out. Open the sign-in window to try again.",
        true,
    )
}

fn invalid_callback() -> QobuzError {
    QobuzError::new(
        "email_callback_invalid",
        "Qobuz returned an unexpected sign-in response. Sign in again to retry.",
        true,
    )
}

fn automatic_setup_failed() -> QobuzError {
    QobuzError::new(
        "credential_discovery",
        "Suonelo could not set up the Qobuz connection automatically. Try again shortly.",
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_the_exact_nonce_bound_destination_and_code_only() {
        let attempt = HostedAttempt::new("123456789").unwrap();
        let mut callback = attempt.callback.clone();
        callback
            .query_pairs_mut()
            .append_pair("code_autorisation", "fixture-code")
            .append_pair("state", &attempt.state);
        assert!(is_callback(&callback, &attempt.callback));
        assert_eq!(
            &*callback_code(&callback, &attempt.state).unwrap(),
            "fixture-code"
        );
        let mut different_port = callback.clone();
        different_port.set_port(Some(43872)).unwrap();
        assert!(!is_callback(&different_port, &attempt.callback));
        assert!(provider_page(
            &Url::parse("https://www.qobuz.com/signin").unwrap()
        ));
        assert!(!provider_page(
            &Url::parse("https://www.qobuz.com.example.test/signin").unwrap()
        ));
        assert!(!provider_page(&callback));
        for query in [
            "code_autorisation=x&state=wrong",
            "code_autorisation=x&code_autorisation=y",
            "code_autorisation=x&token=fixture",
            "user_auth_token=fixture",
        ] {
            callback.set_query(Some(query));
            assert!(callback_code(&callback, &attempt.state).is_err());
        }
    }

    #[test]
    fn code_and_exchange_material_are_sent_in_a_form_body() {
        let client = QobuzClient::new().unwrap();
        let request = client
            .email_exchange_request(
                "123456789",
                "exchange-fixture",
                "code-fixture",
                "verifier-fixture",
            )
            .build()
            .unwrap();
        assert_eq!(request.method(), reqwest::Method::POST);
        assert!(request.url().query().is_none());
        assert_eq!(
            request.headers()["content-type"],
            "application/x-www-form-urlencoded"
        );
        assert_eq!(
            request.body().unwrap().as_bytes().unwrap(),
            b"code=code-fixture&private_key=exchange-fixture&code_verifier=verifier-fixture"
        );
        let attempt = HostedAttempt::new("123456789").unwrap();
        let auth = attempt
            .authorization
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(auth["code_challenge_method"], "S256");
        assert_eq!(
            auth["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(attempt.verifier.as_bytes()))
        );
        assert!(!attempt.authorization.as_str().contains(&**attempt.verifier));
    }
}
