use super::{
    ApiError, Credentials, QobuzClient, QobuzError, f64_value, media_url, signed_params, u32_value,
};
use crate::recovery::RetryBudget;
use serde_json::Value;
use url::Url;

// The URL is short-lived and stays on the Rust side of the process boundary.
pub struct StreamOffer {
    pub url: Url,
    pub tier: u8,
    pub format_id: u32,
    pub bit_depth: Option<u32>,
    pub sample_rate: Option<f64>,
    pub restrictions: Vec<String>,
    #[cfg(target_os = "macos")]
    pub is_preview: bool,
    #[cfg(target_os = "macos")]
    pub requires_full_duration: bool,
}

// Session-local signer cache holds no token and is never persisted as an
// identity validation result. Key changes isolate token, App ID and source pair.
#[cfg(target_os = "macos")]
pub(super) struct MediaSigner {
    key: [u8; 32],
    secret: zeroize::Zeroizing<String>,
}

#[cfg(target_os = "macos")]
fn signer_key(credentials: &Credentials) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for field in [
        &credentials.token,
        &credentials.app_id,
        &credentials.app_secret,
    ] {
        hash.update((field.len() as u64).to_le_bytes());
        hash.update(field.as_bytes());
    }
    hash.finalize().into()
}

const FORMATS: [u32; 4] = [5, 6, 7, 27];
pub(super) const MP3_FORMAT: u32 = FORMATS[0];

impl QobuzClient {
    #[cfg(any(target_os = "macos", test))]
    pub(super) async fn signed_stream_request(
        &self,
        track_id: &str,
        format_id: u32,
        credentials: &Credentials,
    ) -> Result<Value, ApiError> {
        self.signed_stream_request_with_budget(
            track_id,
            format_id,
            credentials,
            &RetryBudget::default(),
        )
        .await
    }

    pub(super) async fn signed_stream_request_with_budget(
        &self,
        track_id: &str,
        format_id: u32,
        credentials: &Credentials,
        budget: &RetryBudget,
    ) -> Result<Value, ApiError> {
        let signature_base =
            format!("trackgetFileUrlformat_id{format_id}intentstreamtrack_id{track_id}");
        self.request_with_budget(
            "track/getFileUrl",
            || {
                let mut params = signed_params(&signature_base, &credentials.app_secret);
                params.extend([
                    ("track_id", track_id.to_owned()),
                    ("format_id", format_id.to_string()),
                    ("intent", "stream".to_owned()),
                ]);
                params
            },
            credentials,
            budget,
        )
        .await
    }

    #[cfg(not(target_os = "macos"))]
    pub async fn negotiate_stream(
        &self,
        track_id: &str,
        requested_tier: u8,
        allow_fallback: bool,
        credentials: &Credentials,
        budget: &RetryBudget,
    ) -> Result<StreamOffer, QobuzError> {
        if credentials.access_mode.is_preview() {
            let unavailable = || {
                QobuzError::new(
                    "preview_unavailable",
                    "Qobuz did not offer a usable MP3 preview for this track.",
                    false,
                )
            };
            let response = self
                .signed_stream_request_with_budget(track_id, MP3_FORMAT, credentials, budget)
                .await;
            return match response {
                Ok(body) => read_mp3_preview(&body).ok_or_else(unavailable),
                Err(failure) if matches!(failure.error.code, "resource_unavailable" | "quality_unavailable") => Err(unavailable()),
                Err(failure) => Err(failure.error),
            };
        }
        if !(1..=4).contains(&requested_tier) {
            return Err(QobuzError::new(
                "invalid_quality",
                "Select a quality tier from 1 through 4.",
                false,
            ));
        }
        let floor = if allow_fallback {
            requested_tier.min(2)
        } else {
            requested_tier
        };
        let mut last_quality_error = None;
        for attempted in (floor..=requested_tier).rev() {
            let format_id = FORMATS[usize::from(attempted - 1)];
            let response = self
                .signed_stream_request_with_budget(track_id, format_id, credentials, budget)
                .await;
            let result = match response {
                Ok(body) if credentials.access_mode == super::AccessMode::Unknown => {
                    read_token_offer(&body, format_id, floor, attempted)
                }
                Ok(body) => read_offer(&body, floor, attempted),
                Err(failure) => Err(failure.error),
            };
            match result {
                Ok(offer) => return Ok(offer),
                Err(error) if error.code == "quality_unavailable" => {
                    last_quality_error = Some(error)
                }
                Err(error) => return Err(error),
            }
        }
        let message = if !allow_fallback && requested_tier > 1 {
            "No offer is available at the selected FLAC tier."
        } else if floor == 2 {
            "No eligible FLAC offer is available for this track."
        } else {
            "No eligible MP3 offer is available for this track."
        };
        let detail = last_quality_error.map(|error| format!("{message} {}", error.message));
        Err(QobuzError::new(
            "quality_unavailable",
            detail.as_deref().unwrap_or(message),
            false,
        ))
    }

    #[cfg(target_os = "macos")]
    pub async fn negotiate_stream(
        &self,
        track_id: &str,
        requested_tier: u8,
        allow_fallback: bool,
        credentials: &Credentials,
        budget: &RetryBudget,
    ) -> Result<StreamOffer, QobuzError> {
        let operation = async {
            let mut remaining = 16;
            if credentials.transport != super::TokenTransport::Query
                || !credentials.app_pair_discovered
            {
                return self
                    .negotiate_track(
                        track_id,
                        requested_tier,
                        allow_fallback,
                        credentials,
                        budget,
                        &mut remaining,
                    )
                    .await;
            }
            // Serialize discovery for an automatic pair. Transfers proceed after
            // negotiation releases this lock, and later tracks reuse its result.
            let mut cache = self.media_signer.lock().await;
            let key = signer_key(credentials);
            let mut effective = credentials.clone();
            if let Some(signer) = cache.as_ref().filter(|signer| signer.key == key) {
                use zeroize::Zeroize;
                effective.app_secret.zeroize();
                effective.app_secret.clone_from(&signer.secret);
            }
            let first = self
                .negotiate_track(
                    track_id,
                    requested_tier,
                    allow_fallback,
                    &effective,
                    budget,
                    &mut remaining,
                )
                .await;
            if !first
                .as_ref()
                .is_err_and(|error| error.code == "invalid_app_pair")
            {
                return first;
            }
            let candidates = self.discover_app_credentials().await?;
            // The identity validator accepted this App ID. A signing failure
            // cannot authorize changing it during a media request.
            if candidates.app_id != credentials.app_id {
                return first;
            }
            let previous_secret = zeroize::Zeroizing::new(effective.app_secret.clone());
            for secret in &candidates.secrets {
                if secret == &credentials.app_secret || secret == &*previous_secret {
                    continue;
                }
                use zeroize::Zeroize;
                effective.app_secret.zeroize();
                effective.app_secret.clone_from(secret);
                match self
                    .negotiate_track(
                        track_id,
                        requested_tier,
                        allow_fallback,
                        &effective,
                        budget,
                        &mut remaining,
                    )
                    .await
                {
                    Ok(offer) => {
                        *cache = Some(MediaSigner {
                            key,
                            secret: zeroize::Zeroizing::new(effective.app_secret.clone()),
                        });
                        return Ok(offer);
                    }
                    Err(error) if error.code == "invalid_app_pair" => (),
                    Err(error) => return Err(error),
                }
            }
            first
        };
        if credentials.transport == super::TokenTransport::Query {
            tokio::time::timeout(std::time::Duration::from_secs(45), operation)
                .await
                .map_err(|_| {
                    QobuzError::new(
                        "media_negotiation_timeout",
                        "Qobuz media negotiation timed out. Retry the track.",
                        true,
                    )
                })?
        } else {
            operation.await
        }
    }

    #[cfg(target_os = "macos")]
    async fn bounded_stream_request(
        &self,
        track_id: &str,
        format_id: u32,
        credentials: &Credentials,
        budget: &RetryBudget,
        remaining: &mut usize,
    ) -> Result<Value, ApiError> {
        *remaining = remaining.checked_sub(1).ok_or_else(|| {
            QobuzError::new(
                "media_negotiation_limit",
                "Qobuz could not provide an offer within the signing request limit.",
                false,
            )
        })?;
        self.signed_stream_request_with_budget(track_id, format_id, credentials, budget)
            .await
    }

    #[cfg(target_os = "macos")]
    async fn negotiate_track(
        &self,
        track_id: &str,
        requested_tier: u8,
        allow_fallback: bool,
        credentials: &Credentials,
        budget: &RetryBudget,
        remaining: &mut usize,
    ) -> Result<StreamOffer, QobuzError> {
        if !(1..=4).contains(&requested_tier) {
            return Err(QobuzError::new(
                "invalid_quality",
                "Select a quality tier from 1 through 4.",
                false,
            ));
        }
        if credentials.access_mode.is_preview() {
            return self
                .preview_offer(track_id, credentials, budget, remaining)
                .await;
        }
        let token_form = credentials.transport == super::TokenTransport::Query
            && credentials.access_mode == super::AccessMode::Unknown;
        let floor = if allow_fallback {
            requested_tier.min(2)
        } else {
            requested_tier
        };
        let mut last_quality_error = None;
        let mut full_offer_seen = false;
        for attempted in (floor..=requested_tier).rev() {
            let format_id = FORMATS[usize::from(attempted - 1)];
            let response = self
                .bounded_stream_request(track_id, format_id, credentials, budget, remaining)
                .await;
            let result = match response {
                Ok(body) => {
                    full_offer_seen |=
                        body.get("sample").is_none() || body["sample"].as_bool() == Some(false);
                    if token_form
                        && format_id == MP3_FORMAT
                        && let Some(preview) = read_mp3_preview(&body)
                    {
                        return Ok(preview);
                    }
                    read_offer(&body, floor, attempted)
                }
                Err(failure) => Err(failure.error),
            };
            match result {
                Ok(offer) => return Ok(offer),
                Err(error) if error.code == "quality_unavailable" => {
                    last_quality_error = Some(error)
                }
                Err(error)
                    if token_form
                        && matches!(error.code, "resource_unavailable" | "missing_entitlement") =>
                {
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if token_form && !full_offer_seen {
            return self
                .preview_offer(track_id, credentials, budget, remaining)
                .await;
        }
        let message = if !allow_fallback && requested_tier > 1 {
            "No offer is available at the selected FLAC tier."
        } else if floor == 2 {
            "No eligible FLAC offer is available for this track."
        } else {
            "No eligible MP3 offer is available for this track."
        };
        let detail = last_quality_error.map(|error| format!("{message} {}", error.message));
        Err(QobuzError::new(
            "quality_unavailable",
            detail.as_deref().unwrap_or(message),
            false,
        ))
    }

    #[cfg(target_os = "macos")]
    async fn preview_offer(
        &self,
        track_id: &str,
        credentials: &Credentials,
        budget: &RetryBudget,
        remaining: &mut usize,
    ) -> Result<StreamOffer, QobuzError> {
        let unavailable = || {
            QobuzError::new(
                "preview_unavailable",
                "Qobuz did not offer a usable MP3 preview for this track.",
                false,
            )
        };
        match self
            .bounded_stream_request(track_id, MP3_FORMAT, credentials, budget, remaining)
            .await
        {
            Ok(body) => read_mp3_preview(&body).ok_or_else(unavailable),
            Err(failure)
                if matches!(
                    failure.error.code,
                    "resource_unavailable" | "quality_unavailable" | "missing_entitlement"
                ) =>
            {
                Err(unavailable())
            }
            Err(failure) => Err(failure.error),
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn read_token_offer(
    body: &Value,
    requested_format: u32,
    floor: u8,
    ceiling: u8,
) -> Result<StreamOffer, QobuzError> {
    if read_mp3_preview(body).is_some() {
        return Err(QobuzError::new(
            "quality_unavailable",
            "Qobuz offered an MP3 preview for this track.",
            false,
        ));
    }
    // Signed full-track offers can omit the sample marker.
    if !matches!(body.get("sample"), None | Some(Value::Bool(false)))
        || u32_value(&body["format_id"]) != Some(requested_format)
    {
        return Err(QobuzError::new(
            "quality_unavailable",
            "Qobuz did not confirm a full stream in the requested format.",
            false,
        ));
    }
    read_offer(body, floor, ceiling)
}

pub(super) fn read_mp3_preview(body: &Value) -> Option<StreamOffer> {
    if body["sample"].as_bool() != Some(true) || u32_value(&body["format_id"]) != Some(MP3_FORMAT) {
        return None;
    }
    Some(StreamOffer {
        url: media_url(&body["url"])?,
        tier: 1,
        format_id: MP3_FORMAT,
        bit_depth: None,
        sample_rate: f64_value(&body["sampling_rate"]),
        restrictions: restriction_codes(&body["restrictions"]),
        #[cfg(target_os = "macos")]
        is_preview: true,
        #[cfg(target_os = "macos")]
        requires_full_duration: false,
    })
}

pub(super) fn read_offer(body: &Value, floor: u8, ceiling: u8) -> Result<StreamOffer, QobuzError> {
    let restrictions = restriction_codes(&body["restrictions"]);
    let quality_error = |reason: &str| QobuzError::new("quality_unavailable", reason, false);
    #[cfg(not(target_os = "macos"))]
    if body["sample"].as_bool() == Some(true) {
        return Err(quality_error("Qobuz offered a preview of the track."));
    }
    #[cfg(target_os = "macos")]
    let requires_full_duration = match body.get("sample") {
        Some(Value::Bool(false)) => false,
        None => true, // Legacy full responses omit sample; verify transferred duration.
        _ => {
            return Err(quality_error(
                "Qobuz did not provide a usable full-track offer.",
            ));
        }
    };
    let format_id = u32_value(&body["format_id"])
        .ok_or_else(|| quality_error("Qobuz did not identify the offered audio format."))?;
    let tier = FORMATS
        .iter()
        .position(|format| *format == format_id)
        .map(|index| index as u8 + 1)
        .filter(|tier| (floor..=ceiling).contains(tier))
        .ok_or_else(|| {
            quality_error("Qobuz offered a format outside the allowed quality range.")
        })?;
    let url = media_url(&body["url"]).ok_or_else(|| {
        if restrictions.iter().any(|code| {
            let code = code.to_ascii_lowercase();
            code.contains("format") || code.contains("quality")
        }) {
            quality_error("Qobuz restricted access to this format.")
        } else {
            QobuzError::new(
                "resource_unavailable",
                "Qobuz did not provide a usable full-track stream URL.",
                false,
            )
        }
    })?;
    Ok(StreamOffer {
        url,
        tier,
        format_id,
        bit_depth: u32_value(&body["bit_depth"]).filter(|depth| *depth > 0),
        sample_rate: f64_value(&body["sampling_rate"]),
        restrictions,
        #[cfg(target_os = "macos")]
        is_preview: false,
        #[cfg(target_os = "macos")]
        requires_full_duration,
    })
}

fn restriction_codes(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|restriction| {
            let code = restriction
                .as_str()
                .or_else(|| restriction["code"].as_str())?;
            (code.len() <= 80 && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
                .then(|| code.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn token_media_accepts_mp3_and_flac_without_a_sample_field() {
        let errors: Vec<_> = [(5, 1), (6, 2), (7, 3), (27, 4)]
            .into_iter()
            .filter_map(|(format, tier)| {
                let body = json!({"format_id":format,"url":"https://example.test/stream"});
                read_token_offer(&body, format, tier, tier)
                    .map(|offer| assert_eq!((offer.format_id, offer.tier), (format, tier)))
                    .err()
                    .map(|error| (format, error.message))
            })
            .collect();
        assert!(errors.is_empty(), "signed offers were rejected: {errors:?}");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn token_media_rejects_previews_malformed_offers_and_wrong_formats() {
        let mut body = json!({"format_id":5,"url":"https://example.test/stream"});
        body["sample"] = json!(true);
        assert!(
            read_token_offer(&body, 5, 1, 1)
                .err()
                .unwrap()
                .message
                .contains("MP3 preview")
        );
        for invalid in [Value::Null, json!("false"), json!(0)] {
            body["sample"] = invalid;
            assert!(read_token_offer(&body, 5, 1, 1).is_err());
        }
        body["sample"] = json!(false);
        assert!(read_token_offer(&body, 5, 1, 1).is_ok());
        assert!(read_token_offer(&body, 6, 1, 2).is_err());
        body["url"] = json!("http://example.test/stream");
        assert!(read_token_offer(&body, 5, 1, 1).is_err());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn accepts_explicit_flac_downgrade_with_properties() {
        let offer = read_offer(
            &json!({
                "format_id": "6", "url": "https://example.test/signed?signature=abc",
                "sampling_rate": 44.1, "bit_depth": 16,
                "restrictions": [{"code": "FormatRestrictedByFormatAvailability"}],
            }),
            2,
            4,
        )
        .expect("CD FLAC should be allowed");
        assert_eq!(offer.tier, 2);
        assert_eq!(offer.format_id, 6);
        assert_eq!(offer.sample_rate, Some(44.1));
        assert_eq!(offer.bit_depth, Some(16));
        assert_eq!(offer.restrictions, ["FormatRestrictedByFormatAvailability"]);
    }
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn token_download_recovers_the_signer_and_accepts_a_labeled_preview() {
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(Vec::new()));
        let discoveries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let salt = "z".repeat(44);
        let mut client = QobuzClient::new().unwrap();
        client.fixture = Some(Arc::new(crate::qobuz::ProviderFixture {
            api: Box::new({
                let calls = calls.clone();
                move |endpoint, params, credentials| {
                    calls.lock().unwrap().push(endpoint);
                    if endpoint == "user/get" {
                        return Ok(json!({"id":42}));
                    }
                    assert_eq!(endpoint, "track/getFileUrl");
                    if credentials.app_secret == "test-secret" {
                        return Err(crate::qobuz::api_failure(
                            reqwest::StatusCode::BAD_REQUEST,
                            &json!({"message":"Invalid request signature"}),
                            credentials,
                        ));
                    }
                    assert_eq!(credentials.app_secret, "alt-key");
                    let format = params
                        .iter()
                        .find(|(key, _)| *key == "format_id")
                        .unwrap()
                        .1
                        .as_str();
                    if format != "5" {
                        return Err(QobuzError::new("missing_entitlement", "fixture", false).into());
                    }
                    Ok(json!({"sample":true,"format_id":5,"url":"https://example.test/preview"}))
                }
            }),
            player_text: Box::new({
                let discoveries = discoveries.clone();
                move |path| match path {
                    "/login" => {
                        discoveries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(zeroize::Zeroizing::new(
                            r#"<script src="/resources/v1/bundle.js"></script>"#.into(),
                        ))
                    }
                    "/resources/v1/bundle.js" => Ok(zeroize::Zeroizing::new(format!(
                        r#"production:{{api:{{appId:"123456789"}}}},
                    a.initialSeed("YWx0",window.utimezone.oslo),
                    b.initialSeed("dGVz",window.utimezone.paris),
                    {{name:"Europe/Oslo",info:"LWtl",extras:"eQ=={salt}"}},
                    {{name:"Europe/Paris",info:"dC1z",extras:"ZWNyZXQ={salt}"}}"#
                    ))),
                    _ => panic!("unexpected fixture path"),
                }
            }),
        }));
        let mut input = Credentials::default();
        input.token = "fixture-token".into();
        let (session, _) = client.validate_account(&input, false, None).await.unwrap();
        assert_eq!(session.access_mode, crate::qobuz::AccessMode::Unknown);
        assert_eq!(*calls.lock().unwrap(), ["user/get"]);
        let offer = client
            .negotiate_stream("42", 4, false, &session, &RetryBudget::validation())
            .await
            .unwrap_or_else(|error| panic!("token download failed: {}", error.message));
        assert_eq!(
            (offer.format_id, offer.tier, offer.is_preview),
            (5, 1, true)
        );
        assert_eq!(discoveries.load(std::sync::atomic::Ordering::SeqCst), 2); // identity + one media rediscovery
        assert!(
            client
                .negotiate_stream("43", 4, false, &session, &RetryBudget::validation())
                .await
                .unwrap()
                .is_preview
        );
        assert_eq!(discoveries.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(session.app_secret, "test-secret"); // no identity/Keychain mutation
    }

    #[cfg(target_os = "macos")]
    fn token_session(automatic: bool) -> Credentials {
        Credentials {
            token: "fixture-token".into(),
            app_id: "123456789".into(),
            app_secret: "provisional".into(),
            app_pair_discovered: automatic,
            ..Credentials::default()
        }
    }

    #[cfg(target_os = "macos")]
    fn fixture_client(
        api: impl Fn(&'static str, super::super::Params, &Credentials) -> Result<Value, ApiError>
        + Send
        + Sync
        + 'static,
    ) -> QobuzClient {
        let mut client = QobuzClient::new().unwrap();
        client.fixture = Some(std::sync::Arc::new(super::super::ProviderFixture {
            api: Box::new(api),
            player_text: Box::new(|_| panic!("this failure must not trigger discovery")),
        }));
        client
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn token_offers_are_per_track_and_keep_the_requested_quality() {
        let client = fixture_client(|_, params, _| {
            let param = |key| {
                params
                    .iter()
                    .find(|(name, _)| *name == key)
                    .unwrap()
                    .1
                    .as_str()
            };
            match (param("track_id"), param("format_id")) {
                ("full", "27") => Ok(
                    json!({"sample":false,"format_id":27,"bit_depth":24,"sampling_rate":192,"url":"https://example.test/full"}),
                ),
                ("preview", "5") => {
                    Ok(json!({"sample":true,"format_id":5,"url":"https://example.test/preview"}))
                }
                ("absent", "5") => Ok(json!({"sample":false,"format_id":5,"url":null})),
                _ => Err(QobuzError::new("missing_entitlement", "fixture", false).into()),
            }
        });
        let credentials = token_session(false);
        let budget = RetryBudget::validation();
        let full = client
            .negotiate_stream("full", 4, false, &credentials, &budget)
            .await
            .unwrap();
        let preview = client
            .negotiate_stream("preview", 4, false, &credentials, &budget)
            .await
            .unwrap();
        assert_eq!((full.tier, full.is_preview), (4, false));
        assert_eq!((preview.tier, preview.is_preview), (1, true));
        let error = client
            .negotiate_stream("absent", 4, false, &credentials, &budget)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "preview_unavailable");
        assert!(!error.retryable);
        assert_eq!(credentials.access_mode, crate::qobuz::AccessMode::Unknown);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn subscribed_full_offer_without_sample_flag_does_not_fall_back_to_preview() {
        let client = fixture_client(|_, params, _| {
            assert_eq!(
                params
                    .iter()
                    .find(|(key, _)| *key == "format_id")
                    .unwrap()
                    .1,
                "6"
            );
            Ok(
                json!({"format_id":6,"url":"https://example.test/full","bit_depth":16,"sampling_rate":44.1}),
            )
        });
        let offer = client
            .negotiate_stream(
                "42",
                2,
                false,
                &token_session(false),
                &RetryBudget::validation(),
            )
            .await
            .unwrap_or_else(|error| panic!("subscribed full download failed: {}", error.message));
        assert_eq!(offer.tier, 2);
        assert!(!offer.is_preview);
        assert!(offer.requires_full_duration);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn full_offer_rejection_must_not_be_replaced_by_preview_error() {
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(Vec::new()));
        let client = fixture_client({
            let calls = calls.clone();
            move |_, params, _| {
                let format = params
                    .iter()
                    .find(|(key, _)| *key == "format_id")
                    .unwrap()
                    .1
                    .clone();
                calls.lock().unwrap().push(format.clone());
                Ok(
                    json!({"sample":false,"format_id":if format=="5" {5} else {6},"url":"https://example.test/full","bit_depth":16,"sampling_rate":44.1}),
                )
            }
        });
        let error = client
            .negotiate_stream(
                "42",
                4,
                false,
                &token_session(false),
                &RetryBudget::validation(),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.code, "quality_unavailable",
            "full-track rejection was masked: {}",
            error.message
        );
        assert_eq!(*calls.lock().unwrap(), ["27"]);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn provider_failures_never_authorize_preview_or_discovery() {
        for code in [
            "invalid_token",
            "invalid_app_id",
            "account_rejected",
            "provider_blocked",
            "network",
            "rate_limit",
            "api_timeout",
        ] {
            let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let client = fixture_client({
                let calls = calls.clone();
                move |_, _, _| {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Err(QobuzError::new(code, "fixture", false).into())
                }
            });
            let error = client
                .negotiate_stream(
                    "42",
                    4,
                    true,
                    &token_session(true),
                    &RetryBudget::validation(),
                )
                .await
                .err()
                .unwrap();
            assert_eq!(error.code, code);
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn manual_and_hosted_pairs_are_never_rotated_by_media() {
        for transport in [
            super::super::TokenTransport::Query,
            super::super::TokenTransport::HeaderOnly,
        ] {
            let client = fixture_client(|_, _, _| {
                Err(QobuzError::new("invalid_app_pair", "fixture", false).into())
            });
            let mut credentials = token_session(false);
            credentials.transport = transport;
            let error = client
                .negotiate_stream("42", 4, true, &credentials, &RetryBudget::validation())
                .await
                .err()
                .unwrap();
            assert_eq!(error.code, "invalid_app_pair");
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn automatic_signing_rotation_stops_at_the_request_limit() {
        use base64::Engine;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let mut bundle = String::from(r#"production:{api:{appId:"123456789"}},"#);
        for i in 0..20 {
            let zone = format!("Zone{}", char::from(b'a' + i));
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(format!("secret-{i:02}"));
            bundle.push_str(&format!(r#"a.initialSeed("{}",window.utimezone.{}),{{name:"Europe/{}",info:"{}",extras:"{}{}"}},"#, &encoded[..4],zone.to_lowercase(),zone,&encoded[4..8],&encoded[8..],"z".repeat(44)));
        }
        let mut client = QobuzClient::new().unwrap();
        client.fixture = Some(Arc::new(super::super::ProviderFixture {
            api: Box::new({
                let calls = calls.clone();
                move |_, _, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err(QobuzError::new("invalid_app_pair", "fixture", false).into())
                }
            }),
            player_text: Box::new(move |path| {
                Ok(zeroize::Zeroizing::new(if path == "/login" {
                    r#"<script src="/resources/v1/bundle.js"></script>"#.into()
                } else {
                    bundle.clone()
                }))
            }),
        }));
        let error = client
            .negotiate_stream(
                "42",
                4,
                false,
                &token_session(true),
                &RetryBudget::validation(),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "media_negotiation_limit");
        assert_eq!(calls.load(Ordering::SeqCst), 16);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test(start_paused = true)]
    async fn token_media_deadline_includes_waiting_for_the_shared_signer() {
        let client = fixture_client(|_, _, _| panic!("lock is held"));
        let _guard = client.media_signer.lock().await;
        let start = tokio::time::Instant::now();
        let error = client
            .negotiate_stream(
                "42",
                4,
                false,
                &token_session(true),
                &RetryBudget::validation(),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "media_negotiation_timeout");
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(45));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn legacy_pairs_default_to_manual_and_full_requires_explicit_proof() {
        let legacy: Credentials = serde_json::from_value(
            json!({"token":"fixture","app_id":"123456789","app_secret":"secret"}),
        )
        .unwrap();
        assert!(!legacy.app_pair_discovered);
        let body = json!({"sample":false,"format_id":5,"url":"https://example.test/full"});
        assert!(read_offer(&body, 1, 1).is_ok());
        for sample in [Value::Null, json!(true), json!("false"), json!(0)] {
            let mut invalid = body.clone();
            invalid["sample"] = sample;
            assert!(read_offer(&invalid, 1, 1).is_err());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn accepts_explicit_flac_downgrade_with_properties() {
        let offer = read_offer(
            &json!({
                "sample": false, "format_id": "6", "url": "https://example.test/signed?signature=abc",
                "sampling_rate": 44.1, "bit_depth": 16,
                "restrictions": [{"code": "FormatRestrictedByFormatAvailability"}],
            }),
            2,
            4,
        )
        .expect("CD FLAC should be allowed");
        assert_eq!(offer.tier, 2);
        assert_eq!(offer.format_id, 6);
        assert_eq!(offer.sample_rate, Some(44.1));
        assert_eq!(offer.bit_depth, Some(16));
        assert_eq!(offer.restrictions, ["FormatRestrictedByFormatAvailability"]);
    }

    #[test]
    fn rejects_mp3_fallbacks_higher_tiers_and_previews() {
        let mut offer = json!({"format_id": 5, "url": "https://example.test/signed"});
        assert!(read_offer(&offer, 2, 4).is_err());
        offer["format_id"] = json!(27);
        assert!(read_offer(&offer, 2, 3).is_err());
        offer["sample"] = json!(true);
        assert!(read_offer(&offer, 2, 4).is_err());
    }

    #[test]
    fn previews_require_explicit_sample_mp3_and_https() {
        let preview = json!({"sample":true,"format_id":5,"url":"https://example.test/preview"});
        let offer = read_mp3_preview(&preview).unwrap();
        assert_eq!((offer.tier, offer.format_id), (1, 5));
        assert!(read_offer(&preview, 1, 1).is_err());
        for (key, value) in [
            ("sample", Value::Null),
            ("sample", json!(false)),
            ("format_id", json!(6)),
            ("url", Value::Null),
            ("url", json!("http://example.test/preview")),
            ("url", json!("https://user@example.test/preview")),
        ] {
            let mut invalid = preview.clone();
            invalid[key] = value;
            assert!(read_mp3_preview(&invalid).is_none());
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    #[ignore = "requires a live Qobuz free account and SUONELO_PREVIEW_CREDENTIALS"]
    async fn live_uk_mp3_preview() {
        use crate::qobuz::bounded_body;
        use regex::Regex;
        use reqwest::StatusCode;
        use std::{env, fs, time::Duration};
        use zeroize::Zeroizing;

        let path = env::var_os("SUONELO_PREVIEW_CREDENTIALS")
            .or_else(|| env::var_os("QBDLX_PREVIEW_CREDENTIALS"))
            .expect("set SUONELO_PREVIEW_CREDENTIALS to the supplied local credentials file");
        let contents = Zeroizing::new(fs::read_to_string(path).expect("read preview credentials"));
        let field = |label: &str| {
            contents
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.trim()
                        .trim_matches(['"', '\''])
                        .eq_ignore_ascii_case(label)
                        .then(|| {
                            value
                                .trim()
                                .trim_end_matches(',')
                                .trim()
                                .trim_matches(['"', '\''])
                        })
                })
                .unwrap_or_default()
        };
        let token = field("token");
        let app_id = field("app id");
        let candidates = Regex::new(r"\b[0-9A-Fa-f]{32}\b").unwrap();
        assert!(
            !token.is_empty() && app_id.len() == 9,
            "preview token or app ID is missing"
        );
        let client = QobuzClient::new().expect("initialize Qobuz API client");
        let mut signed = None;
        let track_id = match env::var("SUONELO_PREVIEW_TRACK_ID")
            .or_else(|_| env::var("QBDLX_PREVIEW_TRACK_ID"))
        {
            Ok(track_id) => track_id,
            Err(_) => {
                // Pick a preview from a publicly listed UK release.
                let catalog_credentials = Credentials {
                    token: token.to_owned(),
                    app_id: app_id.to_owned(),
                    app_secret: String::new(),
                    transport: crate::qobuz::TokenTransport::Query,
                    access_mode: crate::qobuz::AccessMode::Unknown,
                };
                let album = client
                    .metadata(
                        "album/get",
                        vec![("album_id", "0886443927087".to_owned())],
                        &catalog_credentials,
                    )
                    .await
                    .unwrap_or_else(|error| {
                        panic!("preview catalog metadata failed with {}", error.code)
                    });
                album["tracks"]["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|track| track["sampleable"].as_bool() != Some(false))
                    .find_map(|track| u32_value(&track["id"]))
                    .expect("the UK release should offer a preview track")
                    .to_string()
            }
        };
        assert!(!track_id.is_empty() && track_id.bytes().all(|byte| byte.is_ascii_digit()));
        for secret in candidates.find_iter(field("secret keys")) {
            let credentials = Credentials {
                token: token.to_owned(),
                app_id: app_id.to_owned(),
                app_secret: secret.as_str().to_owned(),
                transport: crate::qobuz::TokenTransport::Query,
                access_mode: crate::qobuz::AccessMode::Unknown,
            };
            match client
                .signed_stream_request(&track_id, 5, &credentials)
                .await
            {
                Ok(offer) => {
                    signed = Some((credentials, offer));
                    break;
                }
                Err(failure)
                    if failure.error.code == "invalid_app_pair"
                        || failure.status == Some(StatusCode::BAD_REQUEST) => {}
                Err(failure) => panic!("preview request failed with {}", failure.error.code),
            }
        }
        let Some((credentials, offer)) = signed else {
            panic!("no preview candidate produced a signed offer")
        };
        match client.validate_account(&credentials).await {
            Err(error) => assert_eq!(error.code, "missing_entitlement"),
            Ok(_) => panic!("free preview credentials must fail the full-track entitlement gate"),
        }
        let track = client
            .metadata(
                "track/get",
                vec![("track_id", track_id.clone())],
                &credentials,
            )
            .await
            .unwrap_or_else(|error| panic!("track metadata failed with {}", error.code));
        assert_eq!(
            u32_value(&track["id"]),
            track_id.parse().ok(),
            "preview track identity differed"
        );
        assert_eq!(
            u32_value(&offer["format_id"]),
            Some(5),
            "Qobuz must offer MP3 format 5"
        );
        assert_eq!(
            offer["sample"].as_bool(),
            Some(true),
            "only sample offers can be fetched"
        );
        assert!(
            f64_value(&offer["duration"]).is_some_and(|seconds| seconds <= 60.0),
            "the preview must advertise a duration of at most 60 seconds"
        );
        let Some(url) = media_url(&offer["url"]) else {
            panic!("preview lacks an HTTPS URL")
        };
        let response = client
            .http_client()
            .get(url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .unwrap_or_else(|_| panic!("preview transfer failed"));
        assert!(response.status().is_success(), "preview CDN request failed");
        let bytes = bounded_body(response, 3 * 1024 * 1024)
            .await
            .expect("read bounded preview");
        let mpeg_sync =
            bytes.first() == Some(&0xff) && bytes.get(1).is_some_and(|byte| byte & 0xe0 == 0xe0);
        assert!(
            bytes.len() >= 1024 && (bytes.starts_with(b"ID3") || mpeg_sync),
            "preview must carry MP3 data"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "requires a live Qobuz free account and SUONELO_PREVIEW_CREDENTIALS"]
    async fn live_uk_mp3_preview() {
        use crate::qobuz::bounded_body;
        use regex::Regex;
        use reqwest::StatusCode;
        use std::{env, fs, time::Duration};
        use zeroize::Zeroizing;

        let path = env::var_os("SUONELO_PREVIEW_CREDENTIALS")
            .or_else(|| env::var_os("QBDLX_PREVIEW_CREDENTIALS"))
            .expect("set SUONELO_PREVIEW_CREDENTIALS to the supplied local credentials file");
        let contents = Zeroizing::new(fs::read_to_string(path).expect("read preview credentials"));
        let field = |label: &str| {
            contents
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.trim()
                        .trim_matches(['"', '\''])
                        .eq_ignore_ascii_case(label)
                        .then(|| {
                            value
                                .trim()
                                .trim_end_matches(',')
                                .trim()
                                .trim_matches(['"', '\''])
                        })
                })
                .unwrap_or_default()
        };
        let token = field("token");
        let app_id = field("app id");
        let candidates = Regex::new(r"\b[0-9A-Fa-f]{32}\b").unwrap();
        assert!(
            !token.is_empty() && app_id.len() == 9,
            "preview token or app ID is missing"
        );
        let client = QobuzClient::new().expect("initialize Qobuz API client");
        let mut signed = None;
        let track_id = match env::var("SUONELO_PREVIEW_TRACK_ID")
            .or_else(|_| env::var("QBDLX_PREVIEW_TRACK_ID"))
        {
            Ok(track_id) => track_id,
            Err(_) => {
                // Pick a preview from a publicly listed UK release.
                let catalog_credentials = Credentials {
                    token: token.to_owned(),
                    app_id: app_id.to_owned(),
                    app_secret: String::new(),
                    transport: crate::qobuz::TokenTransport::Query,
                    access_mode: crate::qobuz::AccessMode::Unknown,
                    app_pair_discovered: false,
                };
                let album = client
                    .metadata(
                        "album/get",
                        vec![("album_id", "0886443927087".to_owned())],
                        &catalog_credentials,
                    )
                    .await
                    .unwrap_or_else(|error| {
                        panic!("preview catalog metadata failed with {}", error.code)
                    });
                album["tracks"]["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|track| track["sampleable"].as_bool() != Some(false))
                    .find_map(|track| u32_value(&track["id"]))
                    .expect("the UK release should offer a preview track")
                    .to_string()
            }
        };
        assert!(!track_id.is_empty() && track_id.bytes().all(|byte| byte.is_ascii_digit()));
        for secret in candidates.find_iter(field("secret keys")) {
            let credentials = Credentials {
                token: token.to_owned(),
                app_id: app_id.to_owned(),
                app_secret: secret.as_str().to_owned(),
                transport: crate::qobuz::TokenTransport::Query,
                access_mode: crate::qobuz::AccessMode::Unknown,
                app_pair_discovered: false,
            };
            match client
                .signed_stream_request(&track_id, 5, &credentials)
                .await
            {
                Ok(offer) => {
                    signed = Some((credentials, offer));
                    break;
                }
                Err(failure)
                    if failure.error.code == "invalid_app_pair"
                        || failure.status == Some(StatusCode::BAD_REQUEST) => {}
                Err(failure) => panic!("preview request failed with {}", failure.error.code),
            }
        }
        let Some((credentials, offer)) = signed else {
            panic!("no preview candidate produced a signed offer")
        };
        let (validated, _) = client
            .validate_account(&credentials, false, None)
            .await
            .unwrap_or_else(|error| panic!("token identity check failed with {}", error.code));
        assert_eq!(validated.access_mode, crate::qobuz::AccessMode::Unknown);
        let track = client
            .metadata(
                "track/get",
                vec![("track_id", track_id.clone())],
                &credentials,
            )
            .await
            .unwrap_or_else(|error| panic!("track metadata failed with {}", error.code));
        assert_eq!(
            u32_value(&track["id"]),
            track_id.parse().ok(),
            "preview track identity differed"
        );
        assert_eq!(
            u32_value(&offer["format_id"]),
            Some(5),
            "Qobuz must offer MP3 format 5"
        );
        assert_eq!(
            offer["sample"].as_bool(),
            Some(true),
            "only sample offers can be fetched"
        );
        assert!(
            f64_value(&offer["duration"]).is_some_and(|seconds| seconds <= 60.0),
            "the preview must advertise a duration of at most 60 seconds"
        );
        let Some(url) = media_url(&offer["url"]) else {
            panic!("preview lacks an HTTPS URL")
        };
        let response = client
            .http_client()
            .get(url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .unwrap_or_else(|_| panic!("preview transfer failed"));
        assert!(response.status().is_success(), "preview CDN request failed");
        let bytes = bounded_body(response, 3 * 1024 * 1024)
            .await
            .expect("read bounded preview");
        let mpeg_sync =
            bytes.first() == Some(&0xff) && bytes.get(1).is_some_and(|byte| byte & 0xe0 == 0xe0);
        assert!(
            bytes.len() >= 1024 && (bytes.starts_with(b"ID3") || mpeg_sync),
            "preview must carry MP3 data"
        );
    }
}
