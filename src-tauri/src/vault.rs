use crate::error::{AppError, Result};
#[cfg(not(target_os = "macos"))]
use crate::qobuz::{AccessMode, Credentials, TokenTransport};
#[cfg(target_os = "macos")]
use crate::qobuz::{AccessMode, AppPair, Credentials, TokenTransport, valid_app_id, valid_token};
use hmac::{Hmac, Mac};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
#[cfg(not(target_os = "macos"))]
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
#[cfg(not(target_os = "macos"))]
use tempfile::NamedTempFile;
#[cfg(target_os = "macos")]
use std::time::{SystemTime, UNIX_EPOCH};
use zeroize::{Zeroize, Zeroizing};

const REUSE_WINDOW_MS: u64 = 12 * 60 * 60 * 1000;
const MAX_RECORD: u64 = 1024 * 1024;
#[cfg(not(target_os = "macos"))]
const EMAIL_RECORD_MAGIC: &[u8] = b"SUONELO-EMAIL-V1\0";

#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "com.suonelo.desktop.qobuz.v1";
#[cfg(target_os = "macos")]
const KEYCHAIN_RECORD_MAGIC: &[u8] = b"SUONELO-KEYCHAIN-V1\0";
#[cfg(target_os = "macos")]
const KEYCHAIN_ITEM_NOT_FOUND: i32 = -25300;

#[derive(Serialize, Deserialize)]
struct SecretRecord {
    fingerprint_key: [u8; 32],
    credentials: Credentials,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "read_token_timestamp")]
    token_committed_at_ms: Option<u64>,
    #[cfg(target_os = "macos")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remembered_app_pair: Option<AppPair>,
}

fn read_token_timestamp<'de, D>(value: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(serde_json::Value::deserialize(value)?.as_u64().filter(|time| *time > 0))
}

impl Drop for SecretRecord {
    fn drop(&mut self) {
        self.fingerprint_key.zeroize();
    }
}

pub struct Vault {
    #[cfg(not(target_os = "macos"))]
    path: PathBuf,
    #[cfg(target_os = "macos")]
    keychain_account: String,
    #[cfg(target_os = "macos")]
    keychain_loading: bool,
    record: Option<SecretRecord>,
    load_error: Option<AppError>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountDto {
    pub state: &'static str,
    pub epoch: u64,
    pub has_token: bool,
    pub has_saved_credentials: bool,
    pub has_app_id: bool,
    pub has_secret: bool,
    #[cfg(target_os = "macos")]
    pub has_remembered_app_pair: bool,
    #[cfg(target_os = "macos")]
    pub can_remember_saved_app_pair: bool,
    pub access_mode: AccessMode,
    pub reason: Option<String>,
    pub reason_code: Option<&'static str>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialInput {
    pub token: String,
    pub app_id: Option<String>,
    #[serde(alias = "appSecret")]
    pub secret: Option<String>,
    #[serde(default)]
    pub force_discovery: bool,
    #[cfg(target_os = "macos")]
    #[serde(default)]
    pub remember_saved_app_pair: bool,
}

#[cfg(target_os = "macos")]
#[derive(Default)]
pub struct PreparedToken {
    pub credentials: Credentials,
    pub retry_reused_pair: bool,
    pub remembered_fallback: Option<AppPair>,
    pub remember_pair: bool,
}

impl CredentialInput {
    fn prepare(
        &self,
        saved: Option<&Credentials>,
        committed_at_ms: Option<u64>,
        now_ms: Option<u64>,
    ) -> Result<(Credentials, bool)> {
        let token = self.token.trim();
        let app_id = self.app_id.as_deref().unwrap_or_default().trim();
        let secret = self.secret.as_deref().unwrap_or_default().trim();
        let automatic = app_id.is_empty() && secret.is_empty();
        #[cfg(target_os = "macos")]
        let automatic = self.force_discovery || automatic;
        if !self.force_discovery && app_id.is_empty() != secret.is_empty() {
            return Err(AppError::new(
                "incomplete_app_pair",
                "account",
                "Enter both app fields, or clear both for automatic setup.",
                false,
            ));
        }
        if let Some(saved) = saved.filter(|saved| saved.transport == TokenTransport::HeaderOnly)
            && (token.is_empty() || token == stored_token(saved))
        {
            if automatic && !self.force_discovery {
                return Ok((saved.clone(), false));
            }
            return Err(AppError::invalid(
                "account",
                "Sign in with email again or enter a different token.",
            ));
        }
        let token = if token.is_empty() && automatic {
            saved.map(stored_token).unwrap_or_default()
        } else {
            token
        };
        if token.is_empty() {
            return Err(AppError::invalid("account", "Enter your Qobuz token."));
        }
        #[cfg(target_os = "macos")]
        if !valid_token(token) {
            return Err(AppError::new(
                "invalid_token", "account", "Enter a valid Qobuz account token.", false,
            ));
        }
        let mut input = Credentials::default();
        input.token = token.to_owned();
        if self.force_discovery {
            return Ok((input, false));
        }
        if !automatic {
            #[cfg(target_os = "macos")]
            if !valid_app_id(app_id) {
                return Err(AppError::new(
                    "invalid_app_id", "account", "Enter a nine-digit Qobuz App ID.", false,
                ));
            }
            input.app_id = app_id.to_owned();
            input.app_secret = secret.to_owned();
            return Ok((input, false));
        }
        let complete_pair = |saved: &&Credentials| {
            #[cfg(target_os = "macos")]
            {
                saved.has_token_app_pair()
            }
            #[cfg(not(target_os = "macos"))]
            {
                saved.transport == TokenTransport::Query
                    && saved.app_id.len() == 9
                    && saved.app_id.bytes().all(|byte| byte.is_ascii_digit())
                    && !saved.app_secret.trim().is_empty()
            }
        };
        if let Some(saved) = saved.filter(complete_pair) {
            let replacement = stored_token(saved) != token;
            let age = now_ms
                .zip(committed_at_ms.filter(|committed| *committed > 0))
                .and_then(|(now, committed)| now.checked_sub(committed));
            if !replacement || age.is_some_and(|age| age <= REUSE_WINDOW_MS) {
                #[cfg(target_os = "macos")]
                {
                    input.app_id = saved.app_id.trim().to_owned();
                    input.app_secret = saved.app_secret.trim().to_owned();
                    input.app_pair_discovered = saved.app_pair_discovered;
                }
                #[cfg(not(target_os = "macos"))]
                {
                    input.app_id.clone_from(&saved.app_id);
                    input.app_secret.clone_from(&saved.app_secret);
                }
                return Ok((input, replacement));
            }
        }
        Ok((input, false))
    }
}

fn stored_token(credentials: &Credentials) -> &str {
    #[cfg(target_os = "macos")]
    {
        credentials.token.trim()
    }
    #[cfg(not(target_os = "macos"))]
    {
        &credentials.token
    }
}

fn unix_time_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

impl Drop for CredentialInput {
    fn drop(&mut self) {
        self.token.zeroize();
        self.app_id.zeroize();
        self.secret.zeroize();
    }
}

impl Vault {
    #[cfg(not(target_os = "macos"))]
    pub fn new(path: PathBuf) -> Self {
        let result = match fs::File::open(&path) {
            Ok(file) => read_record(file).map(Some).ok_or_else(vault_load_error),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(vault_load_error()),
        };
        let (record, load_error) = match result {
            Ok(record) => (record, None),
            Err(error) => (None, Some(error)),
        };
        Self {
            path,
            record,
            load_error,
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn prepare_token(&self, input: CredentialInput) -> Result<(Credentials, bool)> {
        input.prepare(
            self.credentials(),
            self.record
                .as_ref()
                .and_then(|record| record.token_committed_at_ms),
            unix_time_ms(),
        )
    }

    #[cfg(target_os = "macos")]
    pub fn keychain(qa_name: Option<&str>) -> Self {
        let keychain_account = qa_name.map_or_else(|| "default".to_owned(), |name| format!("qa:{name}"));
        Self {
            keychain_account,
            keychain_loading: true,
            record: None,
            load_error: None,
        }
    }

    #[cfg(target_os = "macos")]
    pub fn pending_keychain_account(&self) -> Option<String> {
        self.keychain_loading.then(|| self.keychain_account.clone())
    }

    #[cfg(target_os = "macos")]
    pub fn read_keychain(keychain_account: String) -> Self {
        let (record, load_error) = match keychain_read(&keychain_account) {
            Ok(record) => (record, None),
            Err(error) => (None, Some(error)),
        };
        Self {
            keychain_account,
            keychain_loading: false,
            record,
            load_error,
        }
    }

    #[cfg(target_os = "macos")]
    pub fn finish_keychain_load(&mut self, loaded: std::result::Result<Self, tokio::task::JoinError>) {
        match loaded {
            Ok(vault) => *self = vault,
            Err(_) => {
                self.keychain_loading = false;
                self.load_error = Some(vault_io());
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub fn is_loading(&self) -> bool {
        self.keychain_loading
    }

    #[cfg(target_os = "macos")]
    pub fn ensure_loaded(&self) -> Result<()> {
        if self.keychain_loading {
            return Err(keychain_loading_error());
        }
        Ok(())
    }

    #[cfg(all(target_os = "macos", test))]
    pub fn new(path: std::path::PathBuf) -> Self {
        use sha2::Digest;

        // Portable fixtures start empty and never read a Windows credential file.
        Self {
            keychain_account: format!("test:{}", hex::encode(Sha256::digest(path.as_os_str().as_encoded_bytes()))),
            keychain_loading: false,
            record: None,
            load_error: None,
        }
    }

    #[cfg(target_os = "macos")]
    pub fn prepare_token(&self, input: CredentialInput) -> Result<PreparedToken> {
        self.prepare_token_at(input, unix_time_ms())
    }

    #[cfg(target_os = "macos")]
    fn prepare_token_at(&self, input: CredentialInput, now_ms: Option<u64>) -> Result<PreparedToken> {
        let saved = self.credentials();
        let has_manual_input = [&input.app_id, &input.secret]
            .iter()
            .any(|value| value.as_deref().is_some_and(|text| !text.trim().is_empty()));
        if input.remember_saved_app_pair {
            if input.force_discovery || has_manual_input || !input.token.trim().is_empty() {
                return Err(AppError::invalid("account", "Remember the saved pair separately from entering new credentials."));
            }
            let credentials = saved
                .filter(|credentials| credentials.has_token_app_pair())
                .cloned()
                .ok_or_else(|| AppError::invalid("account", "There is no saved token app pair to remember."))?;
            return Ok(PreparedToken {
                credentials,
                remember_pair: true,
                ..PreparedToken::default()
            });
        }

        let (credentials, retry_reused_pair) = input.prepare(
            saved,
            self.record.as_ref().and_then(|record| record.token_committed_at_ms),
            now_ms,
        )?;
        let remember_pair = has_manual_input && !input.force_discovery;
        let may_discover = credentials.transport == TokenTransport::Query
            && (credentials.app_id.is_empty() || retry_reused_pair);
        let remembered_fallback = if may_discover {
            self.remembered_app_pair().cloned()
        } else {
            None
        };
        Ok(PreparedToken { credentials, retry_reused_pair, remembered_fallback, remember_pair })
    }

    #[cfg(target_os = "macos")]
    fn remembered_app_pair(&self) -> Option<&AppPair> {
        self.record.as_ref()?.remembered_app_pair.as_ref().filter(|pair| pair.is_complete())
    }

    #[cfg(target_os = "macos")]
    pub fn has_remembered_app_pair(&self) -> bool {
        self.remembered_app_pair().is_some()
    }

    #[cfg(target_os = "macos")]
    pub fn can_remember_saved_app_pair(&self) -> bool {
        self.credentials().is_some_and(|credentials| {
            credentials.has_token_app_pair()
                && self.remembered_app_pair().is_none_or(|pair| !pair.matches(credentials))
        })
    }

    pub fn load_error(&self) -> Option<&AppError> {
        self.load_error.as_ref()
    }

    pub fn credentials(&self) -> Option<&Credentials> {
        self.record
            .as_ref()
            .map(|record| &record.credentials)
            .filter(|credentials| !credentials.token.is_empty())
    }

    pub fn account_id(&self) -> Option<&str> {
        self.record.as_ref()?.account_id.as_deref()
    }

    pub fn has_saved_credentials(&self) -> bool {
        self.credentials().is_some() || self.load_error.is_some()
    }

    #[cfg(not(target_os = "macos"))]
    pub fn save(&mut self, credentials: Credentials, account_id: String) -> Result<String> {
        self.save_at(credentials, account_id, unix_time_ms())
    }

    #[cfg(not(target_os = "macos"))]
    fn save_at(
        &mut self,
        credentials: Credentials,
        account_id: String,
        now_ms: Option<u64>,
    ) -> Result<String> {
        self.save_with(credentials, account_id, now_ms, Self::persist)
    }

    pub fn replacement_reason(&self) -> Option<String> {
        #[cfg(target_os = "macos")]
        if self.keychain_loading {
            return Some(keychain_loading_error().message);
        }
        self.load_error.as_ref().map(|error| error.message.clone())
    }

    pub fn reset(&mut self) -> Result<()> {
        #[cfg(target_os = "macos")]
        self.ensure_keychain_access()?;
        if let Some(record) = &self.record
            && record.account_id.is_some()
        {
            // Retain only the verified identity so signing out cannot bypass
            // the account switch warning for queued work.
            let signed_out = SecretRecord {
                fingerprint_key: record.fingerprint_key,
                credentials: Credentials::default(),
                account_id: record.account_id.clone(),
                token_committed_at_ms: None,
                #[cfg(target_os = "macos")]
                remembered_app_pair: None,
            };
            self.persist(&signed_out)?;
            self.record = Some(signed_out);
            return Ok(());
        }
        #[cfg(not(target_os = "macos"))]
        match fs::remove_file(&self.path) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err(vault_io()),
        }
        #[cfg(target_os = "macos")]
        keychain_remove(&self.keychain_account)?;
        self.record = None;
        self.load_error = None;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub fn save(&mut self, credentials: Credentials, account_id: String, remember_pair: bool) -> Result<String> {
        self.save_with(credentials, account_id, remember_pair, unix_time_ms(), Self::persist)
    }

    fn save_with(
        &mut self,
        mut credentials: Credentials,
        account_id: String,
        #[cfg(target_os = "macos")]
        remember_pair: bool,
        now_ms: Option<u64>,
        persist: impl FnOnce(&Self, &SecretRecord) -> Result<()>,
    ) -> Result<String> {
        credentials.access_mode = AccessMode::Unknown;
        #[cfg(target_os = "macos")]
        self.ensure_keychain_access()?;
        #[cfg(target_os = "macos")]
        let remembered_app_pair = if remember_pair {
            Some(AppPair::from_token_credentials(&credentials).ok_or_else(vault_io)?)
        } else {
            self.remembered_app_pair().cloned()
        };
        if let Some(record) = &self.record
            && record.credentials == credentials
            && record.account_id.as_deref() == Some(account_id.as_str())
        {
            #[cfg(not(target_os = "macos"))]
            return Ok(fingerprint(record));
            #[cfg(target_os = "macos")]
            if record.remembered_app_pair == remembered_app_pair {
                return Ok(fingerprint(record));
            }
        }
        let mut fingerprint_key = [0; 32];
        if let Some(record) = &self.record {
            fingerprint_key.copy_from_slice(&record.fingerprint_key);
        } else {
            OsRng
                .try_fill_bytes(&mut fingerprint_key)
                .map_err(|_| vault_io())?;
        }
        let token_committed_at_ms = if credentials.transport == TokenTransport::Query {
            match self.record.as_ref() {
                Some(record)
                    if record.credentials.transport == TokenTransport::Query
                        && stored_token(&record.credentials) == credentials.token =>
                {
                    record.token_committed_at_ms
                }
                _ => now_ms,
            }
        } else {
            None
        };
        let record = SecretRecord {
            fingerprint_key,
            credentials,
            account_id: Some(account_id),
            token_committed_at_ms,
            #[cfg(target_os = "macos")]
            remembered_app_pair,
        };
        persist(self, &record)?;
        let result = fingerprint(&record);
        self.record = Some(record);
        self.load_error = None;
        Ok(result)
    }

    pub fn clear_memory(&mut self) {
        self.record = None;
    }

    fn persist(&self, record: &SecretRecord) -> Result<()> {
        #[cfg(not(target_os = "macos"))]
        return persist(&self.path, record);
        #[cfg(target_os = "macos")]
        return keychain_write(&self.keychain_account, record);
    }

    #[cfg(target_os = "macos")]
    fn ensure_keychain_access(&self) -> Result<()> {
        self.ensure_loaded()?;
        // An unavailable item may still hold a verified identity. Read it on a
        // fresh launch before replacing it or clearing the account.
        match &self.load_error {
            Some(error) if error.code != "vault_invalid" => Err(error.clone()),
            _ => Ok(()),
        }
    }
}

#[cfg(target_os = "macos")]
fn keychain_loading_error() -> AppError {
    AppError::new(
        "vault_loading",
        "account",
        "Waiting for macOS Keychain to open your saved credentials.",
        true,
    )
}

fn fingerprint(record: &SecretRecord) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(&record.fingerprint_key).unwrap();
    for value in [
        &record.credentials.token,
        &record.credentials.app_id,
        &record.credentials.app_secret,
    ] {
        mac.update(&(value.len() as u64).to_be_bytes());
        mac.update(value.as_bytes());
    }
    hex::encode(mac.finalize().into_bytes())
}

#[cfg(not(target_os = "macos"))]
fn read_record(file: fs::File) -> Option<SecretRecord> {
    let mut encrypted = Vec::new();
    file.take(MAX_RECORD + 1).read_to_end(&mut encrypted).ok()?;
    if encrypted.len() as u64 > MAX_RECORD || encrypted.starts_with(b"QBDLXVT1") {
        return None;
    }
    let bytes = protect(&mut encrypted, false).ok()?;
    let (transport, payload) = if let Some(payload) = bytes.strip_prefix(EMAIL_RECORD_MAGIC) {
        (TokenTransport::HeaderOnly, payload)
    } else {
        (TokenTransport::Query, bytes.as_slice())
    };
    let record: SecretRecord = serde_json::from_slice(payload).ok()?;
    if record.credentials.transport != transport {
        return None;
    }
    let verified_identity = record.account_id.as_deref().is_some_and(|id| {
        id.parse::<u64>()
            .is_ok_and(|number| number > 0 && number.to_string() == id)
    });
    if transport == TokenTransport::HeaderOnly {
        let credentials = &record.credentials;
        return (verified_identity
            && !credentials.token.is_empty()
            && reqwest::header::HeaderValue::from_str(&credentials.token).is_ok()
            && credentials.app_id.len() == 9
            && credentials.app_id.bytes().all(|byte| byte.is_ascii_digit())
            && !credentials.app_secret.is_empty())
        .then_some(record);
    }
    (!record.credentials.token.is_empty() || verified_identity).then_some(record)
}

#[cfg(not(target_os = "macos"))]
fn persist(path: &Path, record: &SecretRecord) -> Result<()> {
    let parent = path.parent().ok_or_else(vault_io)?;
    let mut bytes = Zeroizing::new(Vec::new());
    if record.credentials.transport == TokenTransport::HeaderOnly {
        // Older versions deserialize a plain JSON record and reject this envelope.
        bytes.extend_from_slice(EMAIL_RECORD_MAGIC);
    }
    serde_json::to_writer(&mut *bytes, record).map_err(|_| vault_io())?;
    let protected = protect(&mut bytes, true)?;
    let mut bundle = NamedTempFile::new_in(parent).map_err(|_| vault_io())?;
    bundle.write_all(&protected).map_err(|_| vault_io())?;
    bundle.as_file().sync_all().map_err(|_| vault_io())?;
    bundle.persist(path).map_err(|_| vault_io())?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
#[cfg(windows)]
fn protect(bytes: &mut [u8], encrypt: bool) -> Result<Zeroizing<Vec<u8>>> {
    use windows::{
        Win32::{
            Foundation::{HLOCAL, LocalFree},
            Security::Cryptography::{
                CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
            },
        },
        core::w,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(bytes.len()).map_err(|_| vault_io())?,
        pbData: bytes.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // DPAPI allocates the output. Copy it out, then clear and free the native buffer.
    unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                w!("suonelo credentials"),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
        .map_err(|_| vault_io())?;
        if output.pbData.is_null() {
            return Err(vault_io());
        }
        let buffer = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
        let result = Zeroizing::new(buffer.to_vec());
        buffer.zeroize();
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(result)
    }
}

#[cfg(not(target_os = "macos"))]
#[cfg(not(windows))]
fn protect(_: &mut [u8], _: bool) -> Result<Zeroizing<Vec<u8>>> {
    Err(vault_io())
}

#[cfg(not(target_os = "macos"))]
fn vault_load_error() -> AppError {
    AppError::new(
        "vault_unreadable",
        "account",
        "Windows could not read the saved credentials. Sign in again to replace them.",
        true,
    )
}

#[cfg(not(target_os = "macos"))]
fn vault_io() -> AppError {
    AppError::new(
        "vault_storage",
        "account",
        "Windows could not read or save your credentials.",
        true,
    )
}

#[cfg(target_os = "macos")]
fn verified_identity(record: &SecretRecord) -> bool {
    record.account_id.as_deref().is_some_and(|id| {
        id.parse::<u64>().is_ok_and(|number| number > 0 && number.to_string() == id)
    })
}

#[cfg(target_os = "macos")]
fn vault_io() -> AppError {
    AppError::new(
        "vault_storage",
        "account",
        if cfg!(target_os = "macos") {
            "macOS Keychain could not read or save your Qobuz credentials. Reopen suonelo after checking Keychain access."
        } else {
            "Windows could not read or save your credentials."
        },
        true,
    )
}

#[cfg(target_os = "macos")]
fn keychain_read(account: &str) -> Result<Option<SecretRecord>> {
    use security_framework::passwords::{PasswordOptions, generic_password};

    match generic_password(PasswordOptions::new_generic_password(KEYCHAIN_SERVICE, account)) {
        Ok(bytes) => {
            let bytes = Zeroizing::new(bytes);
            decode_keychain_record(&bytes).map(Some).ok_or_else(|| AppError::new(
                "vault_invalid",
                "account",
                "The saved Qobuz credentials in Keychain could not be read. Sign in to replace them.",
                true,
            ))
        }
        Err(error) if error.code() == KEYCHAIN_ITEM_NOT_FOUND => Ok(None),
        Err(error) => Err(keychain_error(error.code())),
    }
}

#[cfg(target_os = "macos")]
fn decode_keychain_record(bytes: &[u8]) -> Option<SecretRecord> {
    if bytes.len() as u64 > MAX_RECORD {
        return None;
    }
    let payload = bytes.strip_prefix(KEYCHAIN_RECORD_MAGIC)?;
    let record: SecretRecord = serde_json::from_slice(payload).ok()?;
    valid_keychain_record(&record).then_some(record)
}

#[cfg(target_os = "macos")]
fn valid_keychain_record(record: &SecretRecord) -> bool {
    let credentials = &record.credentials;
    verified_identity(record)
        && (credentials == &Credentials::default()
            || (!credentials.token.is_empty()
                && credentials.token.len() <= 4096
                && !credentials.token.chars().any(char::is_whitespace)
                && (credentials.transport != TokenTransport::HeaderOnly
                    || reqwest::header::HeaderValue::from_str(&credentials.token).is_ok())
                && credentials.app_id.len() == 9
                && credentials.app_id.bytes().all(|byte| byte.is_ascii_digit())
                && !credentials.app_secret.is_empty()))
}

#[cfg(target_os = "macos")]
fn encode_keychain_record(record: &SecretRecord) -> Result<Zeroizing<Vec<u8>>> {
    if !valid_keychain_record(record) {
        return Err(vault_io());
    }
    let mut bytes = Zeroizing::new(KEYCHAIN_RECORD_MAGIC.to_vec());
    serde_json::to_writer(&mut *bytes, record).map_err(|_| vault_io())?;
    if bytes.len() as u64 > MAX_RECORD {
        return Err(vault_io());
    }
    Ok(bytes)
}

#[cfg(target_os = "macos")]
fn keychain_write(account: &str, record: &SecretRecord) -> Result<()> {
    let bytes = encode_keychain_record(record)?;
    // Security.framework adds the generic password or atomically updates its data
    // on a duplicate item. It never deletes the previous item before writing.
    security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, account, &bytes)
        .map_err(|error| match error.code() {
            KEYCHAIN_ITEM_NOT_FOUND => keychain_changed(),
            status => keychain_error(status),
        })
}

#[cfg(target_os = "macos")]
fn keychain_remove(account: &str) -> Result<()> {
    match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, account) {
        Ok(()) => Ok(()),
        Err(error) if error.code() == KEYCHAIN_ITEM_NOT_FOUND => Ok(()),
        Err(error) => Err(keychain_error(error.code())),
    }
}

#[cfg(target_os = "macos")]
fn keychain_error(status: i32) -> AppError {
    // OSStatus values from Security/SecBase.h.
    match status {
        -128 | -25291 | -25293 | -25308 | -25315 => AppError::new(
            "vault_access",
            "account",
            "Keychain access is unavailable. Unlock Keychain, allow access, and reopen suonelo.",
            true,
        ),
        -25299 => keychain_changed(),
        _ => vault_io(),
    }
}

#[cfg(target_os = "macos")]
fn keychain_changed() -> AppError {
    AppError::new(
        "vault_changed",
        "account",
        "The Keychain item changed while saving. Reopen suonelo and sign in again.",
        true,
    )
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[cfg(not(target_os = "macos"))]
    fn token_input(token: &str, force_discovery: bool) -> CredentialInput {
        CredentialInput {
            token: token.into(),
            app_id: None,
            secret: None,
            force_discovery,
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn replacement_reuse_includes_exactly_twelve_hours() {
        let mut saved = Credentials::default();
        saved.token = "saved".into();
        saved.app_id = "123456789".into();
        saved.app_secret = "saved-key".into();
        let start = 1_700_000_000_000;
        for (offset, reuse) in [
            (43_199_000, true),
            (43_200_000, true),
            (43_200_001, false),
            (43_201_000, false),
        ] {
            let (selected, retry) = token_input("replacement", false)
                .prepare(Some(&saved), Some(start), Some(start + offset))
                .unwrap();
            assert_eq!(!selected.app_id.is_empty(), reuse);
            assert_eq!(retry, reuse);
            assert_eq!(selected.token, "replacement");
        }
        for stamp in [None, Some(0), Some(start + 1)] {
            let (selected, retry) = token_input("replacement", false)
                .prepare(Some(&saved), stamp, Some(start))
                .unwrap();
            assert!(selected.app_id.is_empty());
            assert!(!retry);
        }
        let (same, retry) = token_input("saved", false)
            .prepare(Some(&saved), Some(start), Some(start + REUSE_WINDOW_MS + 1))
            .unwrap();
        assert_eq!(same.app_id, "123456789");
        assert!(!retry);
        let (fresh, retry) = token_input("saved", true)
            .prepare(Some(&saved), Some(start), Some(start))
            .unwrap();
        assert_eq!(fresh.token, "saved");
        assert!(fresh.app_id.is_empty());
        assert!(!retry);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn token_commit_timestamp_survives_restart_and_same_token_pair_change() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.vault");
        let mut vault = Vault::new(path.clone());
        assert!(vault.load_error().is_none());
        let mut input = Credentials::default();
        input.token = "first".into();
        input.app_id = "123456789".into();
        input.app_secret = "test-key".into();
        vault
            .save_at(input.clone(), "42".into(), Some(1000))
            .unwrap();
        let mut vault = Vault::new(path.clone());
        assert_eq!(
            vault.record.as_ref().unwrap().token_committed_at_ms,
            Some(1000)
        );
        input.app_id = "987654321".into();
        vault
            .save_at(input.clone(), "42".into(), Some(2000))
            .unwrap();
        assert_eq!(
            vault.record.as_ref().unwrap().token_committed_at_ms,
            Some(1000)
        );
        input.token = "second".into();
        vault.save_at(input, "42".into(), Some(3000)).unwrap();
        assert_eq!(
            Vault::new(path).record.unwrap().token_committed_at_ms,
            Some(3000)
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn email_vault_is_header_only_and_older_reader_rejects_its_payload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.vault");
        let mut vault = Vault::new(path.clone());
        let credentials = Credentials {
            token: "fixture-session".into(),
            app_id: "123456789".into(),
            app_secret: "fixture-key".into(),
            transport: TokenTransport::HeaderOnly,
            access_mode: AccessMode::Preview,
        };
        vault.save(credentials.clone(), "123".into()).unwrap();
        let mut encrypted = fs::read(&path).unwrap();
        let bytes = protect(&mut encrypted, false).unwrap();
        assert!(serde_json::from_slice::<SecretRecord>(&bytes).is_err());
        assert!(bytes.starts_with(EMAIL_RECORD_MAGIC));
        let reloaded = Vault::new(path.clone());
        assert_eq!(
            reloaded.credentials().unwrap().access_mode,
            AccessMode::Unknown
        );
        assert_eq!(
            reloaded.credentials().unwrap().transport,
            TokenTransport::HeaderOnly
        );
        let retry = CredentialInput {
            token: String::new(),
            app_id: None,
            secret: None,
            force_discovery: false,
        };
        assert_eq!(
            reloaded.prepare_token(retry).unwrap().0.transport,
            TokenTransport::HeaderOnly
        );
        let update = CredentialInput {
            token: String::new(),
            app_id: Some("123456789".into()),
            secret: Some("fixture-key".into()),
            force_discovery: false,
        };
        assert!(reloaded.prepare_token(update).is_err());

        // Removing or changing the envelope cannot convert a protected email token to legacy URL transport.
        let mut payload = Zeroizing::new(bytes[EMAIL_RECORD_MAGIC.len()..].to_vec());
        fs::write(&path, &*protect(&mut payload, true).unwrap()).unwrap();
        assert!(Vault::new(path).credentials().is_none());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn explicit_token_discovery_does_not_reuse_the_saved_app_pair() {
        let saved = Credentials {
            token: "saved".into(),
            app_id: "123456789".into(),
            app_secret: "saved-secret".into(),
            transport: TokenTransport::Query,
            access_mode: AccessMode::Unknown,
        };
        let explicit = |token: &str, app_id: Option<&str>, secret: Option<&str>| CredentialInput {
            token: token.into(),
            app_id: app_id.map(str::to_owned),
            secret: secret.map(str::to_owned),
            force_discovery: false,
        };
        let fresh = explicit("new", None, None)
            .prepare(Some(&saved), None, None)
            .map(|(value, _)| value)
            .unwrap();
        assert!(fresh.app_id.is_empty() && fresh.app_secret.is_empty());
        let retry = explicit("", None, None)
            .prepare(Some(&saved), None, None)
            .map(|(value, _)| value)
            .unwrap();
        assert!(retry == saved);
        let manual = explicit("new", Some("987654321"), Some("manual"))
            .prepare(Some(&saved), None, None)
            .map(|(value, _)| value)
            .unwrap();
        assert_eq!(manual.app_id, "987654321");
        assert_eq!(manual.app_secret, "manual");
        assert!(
            explicit("new", Some("987654321"), None)
                .prepare(Some(&saved), None, None)
                .map(|(value, _)| value)
                .is_err()
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn preserves_legacy_record_until_replaced_and_round_trips_dpapi() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("credentials.vault");
        let legacy = b"QBDLXVT1 encrypted legacy record";
        fs::write(&path, legacy).unwrap();
        let mut vault = Vault::new(path.clone());
        assert!(vault.credentials().is_none());
        assert!(vault.replacement_reason().is_some());
        assert_eq!(fs::read(&path).unwrap(), legacy);
        let credentials = Credentials {
            token: "example-sensitive-token".into(),
            app_id: "000000001".into(),
            app_secret: "example-sensitive-secret".into(),
            transport: TokenTransport::Query,
            access_mode: AccessMode::Unknown,
        };
        let key = vault.save(credentials.clone(), "123".into()).unwrap();
        let encrypted = fs::read(&path).unwrap();
        assert!(
            !encrypted
                .windows(credentials.token.len())
                .any(|bytes| bytes == credentials.token.as_bytes())
        );

        let mut restarted = Vault::new(path.clone());
        assert_eq!(restarted.credentials().unwrap().token, credentials.token);
        assert_eq!(restarted.account_id(), Some("123"));
        assert_eq!(restarted.save(credentials, "123".into()).unwrap(), key);
        restarted.reset().unwrap();
        let signed_out = Vault::new(path);
        assert!(!signed_out.has_saved_credentials());
        assert!(signed_out.credentials().is_none());
        assert_eq!(signed_out.account_id(), Some("123"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn email_vault_is_header_only_and_older_reader_rejects_its_payload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.vault");
        let mut vault = Vault::new(path.clone());
        let credentials = Credentials {
            token: "fixture-session".into(),
            app_id: "123456789".into(),
            app_secret: "fixture-key".into(),
            transport: TokenTransport::HeaderOnly,
            access_mode: AccessMode::Unknown,
            app_pair_discovered: false,
        };
        vault.save(credentials.clone(), "123".into(), false).unwrap();
        let mut encrypted = fs::read(&path).unwrap();
        let bytes = protect(&mut encrypted, false).unwrap();
        assert!(serde_json::from_slice::<SecretRecord>(&bytes).is_err());
        assert!(bytes.starts_with(EMAIL_RECORD_MAGIC));
        let reloaded = Vault::new(path.clone());
        assert!(reloaded.credentials() == Some(&credentials));
        let retry = CredentialInput { token: String::new(), app_id: None, secret: None, force_discovery: false, remember_saved_app_pair: false };
        assert!(reloaded.prepare_token(retry).unwrap().credentials == credentials);
        let update = CredentialInput {
            token: String::new(), app_id: Some("123456789".into()), secret: Some("fixture-key".into()), force_discovery: false, remember_saved_app_pair: false,
        };
        assert!(reloaded.prepare_token(update).is_err());

        // Removing or changing the envelope cannot convert a protected email token to legacy URL transport.
        let mut payload = Zeroizing::new(bytes[EMAIL_RECORD_MAGIC.len()..].to_vec());
        fs::write(&path, &*protect(&mut payload, true).unwrap()).unwrap();
        assert!(Vault::new(path).credentials().is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn preserves_legacy_record_until_replaced_and_round_trips_dpapi() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("credentials.vault");
        let legacy = b"QBDLXVT1 encrypted legacy record";
        fs::write(&path, legacy).unwrap();
        let mut vault = Vault::new(path.clone());
        assert!(vault.credentials().is_none());
        assert!(vault.replacement_reason().is_some());
        assert_eq!(fs::read(&path).unwrap(), legacy);
        let credentials = Credentials {
            token: "example-sensitive-token".into(),
            app_id: "000000001".into(),
            app_secret: "example-sensitive-secret".into(),
            transport: TokenTransport::Query,
            access_mode: AccessMode::Unknown,
            app_pair_discovered: false,
        };
        let key = vault.save(credentials.clone(), "123".into(), false).unwrap();
        let encrypted = fs::read(&path).unwrap();
        assert!(
            !encrypted
                .windows(credentials.token.len())
                .any(|bytes| bytes == credentials.token.as_bytes())
        );

        let mut restarted = Vault::new(path.clone());
        assert_eq!(restarted.credentials().unwrap().token, credentials.token);
        assert_eq!(restarted.account_id(), Some("123"));
        assert_eq!(restarted.save(credentials, "123".into(), false).unwrap(), key);
        restarted.reset().unwrap();
        let signed_out = Vault::new(path);
        assert!(!signed_out.has_saved_credentials());
        assert!(signed_out.credentials().is_none());
        assert_eq!(signed_out.account_id(), Some("123"));
    }
}

#[cfg(target_os = "macos")]
#[cfg(test)]
mod token_tests {
    use super::*;

    const COMMITTED: u64 = 1_000_000;
    const NOW: u64 = COMMITTED + REUSE_WINDOW_MS;

    fn input(token: &str) -> CredentialInput {
        CredentialInput {
            token: token.into(),
            app_id: None,
            secret: None,
            force_discovery: false,
            remember_saved_app_pair: false,
        }
    }

    fn pair(token: &str, transport: TokenTransport) -> Credentials {
        Credentials {
            token: token.into(),
            app_id: "123456789".into(),
            app_secret: "fixture-key".into(),
            transport,
            access_mode: AccessMode::Unknown,
            app_pair_discovered: false,
        }
    }

    #[test]
    fn replacement_reuse_is_inclusive_and_rejects_unknown_or_future_timestamps() {
        let saved = pair("saved-token", TokenTransport::Query);
        for (committed, elapsed, reuse) in [
            (Some(COMMITTED), Some(NOW - 1_000), true),
            (Some(COMMITTED), Some(NOW), true),
            (Some(COMMITTED), Some(NOW + 1_000), false),
            (Some(COMMITTED), None, false),
            (None, Some(NOW), false),
            (Some(0), Some(NOW), false),
            (Some(NOW + 1), Some(NOW), false),
            (Some(u64::MAX), Some(NOW), false),
        ] {
            let (selected, retry) = input(" replacement-token ")
                .prepare(Some(&saved), committed, elapsed).unwrap();
            assert_eq!(selected.token, "replacement-token");
            assert_eq!((!selected.app_id.is_empty(), !selected.app_secret.is_empty(), retry), (reuse, reuse, reuse));
        }

        let (same, retry) = input(" saved-token ").prepare(Some(&saved), None, Some(NOW)).unwrap();
        assert!(same == saved && !retry);
        let (empty, retry) = input("").prepare(Some(&saved), None, Some(NOW)).unwrap();
        assert!(empty == saved && !retry);
        let (fresh, retry) = input("first-token").prepare(None, None, Some(NOW)).unwrap();
        assert!(fresh.app_id.is_empty() && fresh.app_secret.is_empty() && !retry);
    }

    #[test]
    fn manual_pair_has_priority_and_forced_discovery_ignores_manual_drafts() {
        let saved = pair("saved-token", TokenTransport::Query);
        let mut draft = input("new-token");
        draft.app_id = Some(" 987654321 ".into());
        assert_eq!(draft.prepare(Some(&saved), Some(COMMITTED), Some(NOW)).err().unwrap().code, "incomplete_app_pair");
        draft.secret = Some(" manual-key ".into());
        let (manual, retry) = draft.prepare(Some(&saved), Some(COMMITTED), Some(NOW)).unwrap();
        assert_eq!((manual.app_id.as_str(), manual.app_secret.as_str(), retry), ("987654321", "manual-key", false));

        draft.app_id = Some("not-an-id".into());
        assert_eq!(draft.prepare(Some(&saved), Some(COMMITTED), Some(NOW)).err().unwrap().code, "invalid_app_id");
        draft.force_discovery = true;
        let (forced, retry) = draft.prepare(Some(&saved), Some(COMMITTED), Some(NOW)).unwrap();
        assert!(forced.app_id.is_empty() && forced.app_secret.is_empty() && !retry);
        let mut blank = input("");
        blank.app_id = Some("unsubmitted-draft".into());
        blank.force_discovery = true;
        assert_eq!(blank.prepare(Some(&saved), None, None).unwrap().0.token, "saved-token");
        for token in ["bad token".to_owned(), "t".repeat(4097)] {
            assert_eq!(input(&token).prepare(None, None, None).err().unwrap().code, "invalid_token");
        }
    }

    #[test]
    fn saved_hosted_token_cannot_be_converted_to_query_transport() {
        let saved = pair("hosted-token", TokenTransport::HeaderOnly);
        for token in ["", " hosted-token "] {
            let mut draft = input(token);
            let (reused, retry) = draft.prepare(Some(&saved), None, None).unwrap();
            assert!(reused == saved && !retry);
            draft.app_id = Some("987654321".into());
            draft.secret = Some("manual-key".into());
            assert!(draft.prepare(Some(&saved), None, None).is_err());
            draft.force_discovery = true;
            assert!(draft.prepare(Some(&saved), None, None).is_err());
        }
        let (different, retry) = input("different-token").prepare(Some(&saved), Some(COMMITTED), Some(NOW)).unwrap();
        assert_eq!(different.transport, TokenTransport::Query);
        assert!(!retry && different.app_id.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn remembers_a_legacy_pair_in_keychain_without_changing_the_token_commit_time() {
        let original = pair("legacy-token", TokenTransport::Query);
        let mut vault = Vault::new("fixture".into());
        let fingerprint = vault.save_with(original, "42".into(), false, Some(COMMITTED), |_, _| Ok(())).unwrap();
        assert!(vault.can_remember_saved_app_pair());

        let mut adopt = input("");
        adopt.remember_saved_app_pair = true;
        let prepared = vault.prepare_token_at(adopt, Some(NOW)).unwrap();
        assert!(prepared.remember_pair && prepared.remembered_fallback.is_none());
        let mut protected = None;
        let adopted = vault.save_with(prepared.credentials, "42".into(), true, Some(NOW), |_, record| {
            protected = Some(encode_keychain_record(record)?);
            Ok(())
        }).unwrap();
        assert_eq!(adopted, fingerprint);
        vault.record = decode_keychain_record(&protected.unwrap());
        assert_eq!(vault.record.as_ref().unwrap().token_committed_at_ms, Some(COMMITTED));
        assert!(vault.has_remembered_app_pair());
        assert!(!vault.can_remember_saved_app_pair());

        let same = vault.prepare_token_at(input("legacy-token"), Some(NOW + 1)).unwrap();
        assert!(same.remembered_fallback.is_none() && !same.retry_reused_pair);
        let replacement = vault.prepare_token_at(input("replacement"), Some(NOW + 1)).unwrap();
        assert!(replacement.credentials.app_id.is_empty());
        assert!(replacement.remembered_fallback.as_ref().is_some_and(|pair| pair.matches(vault.credentials().unwrap())));
        let mut forced = input("legacy-token");
        forced.force_discovery = true;
        assert!(vault.prepare_token_at(forced, Some(NOW)).unwrap().remembered_fallback.is_some());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn protects_the_remembered_pair_across_account_changes_and_failed_writes() {
        let mut vault = Vault::new("fixture".into());
        let mut manual = input("original");
        manual.app_id = Some("123456789".into());
        manual.secret = Some("fixture-key".into());
        let prepared = vault.prepare_token(manual).unwrap();
        assert!(prepared.remember_pair && prepared.remembered_fallback.is_none());
        vault.save_with(prepared.credentials, "42".into(), true, Some(COMMITTED), |_, _| Ok(())).unwrap();

        let mut replacement = pair("replacement", TokenTransport::Query);
        replacement.app_id = "987654321".into();
        assert!(vault.save_with(replacement.clone(), "43".into(), true, Some(NOW), |_, _| Err(vault_io())).is_err());
        assert_eq!(vault.account_id(), Some("42"));
        assert!(vault.remembered_app_pair().unwrap().matches(vault.credentials().unwrap()));

        vault.save_with(replacement, "43".into(), false, Some(NOW), |_, _| Ok(())).unwrap();
        assert_eq!(vault.account_id(), Some("43"));
        assert!(!vault.remembered_app_pair().unwrap().matches(vault.credentials().unwrap()));
        vault.save_with(pair("hosted-token", TokenTransport::HeaderOnly), "43".into(), false, Some(NOW), |_, _| Ok(())).unwrap();
        let hosted = vault.prepare_token(input("")).unwrap();
        assert_eq!(hosted.credentials.transport, TokenTransport::HeaderOnly);
        assert!(hosted.remembered_fallback.is_none());
        let next = vault.prepare_token(input("next-token")).unwrap();
        assert!(next.remembered_fallback.as_ref().is_some_and(|remembered| {
            remembered.matches(&pair("ignored", TokenTransport::Query))
        }));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_metadata_write_preserves_legacy_and_current_token_timestamps() {
        for timestamp in [None, Some(COMMITTED)] {
            let mut previous = pair("saved-token", TokenTransport::Query);
            previous.app_secret = " fixture-key ".into();
            let bytes = encode_keychain_record(&SecretRecord {
                fingerprint_key: [7; 32],
                credentials: previous,
                account_id: Some("42".into()),
                token_committed_at_ms: timestamp,
                remembered_app_pair: None,
            }).unwrap();
            let mut vault = Vault::new("fixture".into());
            vault.record = decode_keychain_record(&bytes);
            let verified = pair("saved-token", TokenTransport::Query);
            let mut stored = None;
            let fingerprint = vault.save_with(verified.clone(), "42".into(), false, Some(NOW), |_, record| {
                stored = Some(encode_keychain_record(record)?);
                Ok(())
            }).unwrap();
            let stored = stored.expect("normalized, verified metadata must be persisted");
            let restored = decode_keychain_record(&stored).unwrap();
            assert_eq!(restored.fingerprint_key, [7; 32]);
            assert_eq!(restored.token_committed_at_ms, timestamp);
            assert!(restored.credentials == verified);

            vault.record = Some(restored);
            assert_eq!(vault.save_with(verified, "42".into(), false, Some(NOW), |_, _| {
                panic!("identical verified record should not be rewritten")
            }).unwrap(), fingerprint);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_timestamp_is_written_only_with_a_successful_token_commit() {
        let mut vault = Vault::new("fixture".into());
        let mut writes = 0;
        let mut check_write = |_: &Vault, record: &SecretRecord| {
            let stored = encode_keychain_record(record)?;
            let decoded = decode_keychain_record(&stored).unwrap();
            assert_eq!(decoded.credentials.access_mode, AccessMode::Unknown);
            writes += 1;
            Ok(())
        };
        vault.save_with(pair("first-token", TokenTransport::Query), "42".into(), false, Some(COMMITTED), &mut check_write).unwrap();
        assert_eq!(vault.record.as_ref().unwrap().token_committed_at_ms, Some(COMMITTED));

        let replacement = pair("next-token", TokenTransport::Query);
        let error = vault.save_with(replacement.clone(), "43".into(), false, Some(NOW), |_, _| Err(vault_io())).unwrap_err();
        assert_eq!(error.code, vault_io().code);
        assert_eq!(vault.credentials().unwrap().token, "first-token");
        assert_eq!(vault.account_id(), Some("42"));
        assert_eq!(vault.record.as_ref().unwrap().token_committed_at_ms, Some(COMMITTED));

        vault.save_with(replacement, "43".into(), false, Some(NOW), &mut check_write).unwrap();
        assert_eq!(vault.record.as_ref().unwrap().token_committed_at_ms, Some(NOW));
        let mut hosted = pair("hosted-token", TokenTransport::HeaderOnly);
        hosted.access_mode = AccessMode::Preview;
        vault.save_with(hosted, "43".into(), false, Some(NOW + 1), &mut check_write).unwrap();
        assert_eq!(vault.record.as_ref().unwrap().token_committed_at_ms, None);
        assert_eq!(writes, 3);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_ignores_unusable_timestamp_values_without_dropping_the_record() {
        let mut data = serde_json::json!({
            "fingerprint_key": vec![0; 32],
            "credentials": pair("old-token", TokenTransport::Query),
            "account_id": "42",
        });
        for value in [serde_json::Value::Null, serde_json::json!(0), serde_json::json!(-1),
            serde_json::json!("unexpected"), serde_json::json!({"wrong": 1}), serde_json::json!(1.5)] {
            data["token_committed_at_ms"] = value;
            let mut bytes = KEYCHAIN_RECORD_MAGIC.to_vec();
            serde_json::to_writer(&mut bytes, &data).unwrap();
            assert_eq!(decode_keychain_record(&bytes).unwrap().token_committed_at_ms, None);
        }
        data["credentials"] = serde_json::to_value(Credentials::default()).unwrap();
        data.as_object_mut().unwrap().remove("token_committed_at_ms");
        let mut bytes = KEYCHAIN_RECORD_MAGIC.to_vec();
        serde_json::to_writer(&mut bytes, &data).unwrap();
        let signed_out = decode_keychain_record(&bytes).unwrap();
        assert!(signed_out.credentials.token.is_empty());
        assert_eq!(signed_out.account_id.as_deref(), Some("42"));
        assert_eq!(signed_out.token_committed_at_ms, None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_access_denial_blocks_replacement_and_removal() {
        let mut vault = Vault::new("fixture".into());
        vault.load_error = Some(keychain_error(-25293));
        let candidate = vault.prepare_token(input("new-token")).unwrap().credentials;
        assert_eq!(vault.save_with(candidate, "42".into(), false, Some(NOW), |_, _| {
            panic!("access denial must block the write")
        }).unwrap_err().code, "vault_access");
        assert_eq!(vault.reset().unwrap_err().code, "vault_access");
        assert!(vault.record.is_none());
    }
}
