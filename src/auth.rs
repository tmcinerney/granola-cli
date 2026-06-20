//! Authentication: credential discovery, keychain storage, WorkOS refresh.
//!
//! This module is a direct port of the upstream `src/lib/auth.ts` with the
//! `stored-accounts.json` fix from beaulebens/granola-cli#6.
//!
//! Lifecycle: `auth login` is the only path that reads files from the Granola
//! desktop app. After import, credentials live in the OS keychain and are
//! rotated via WorkOS refresh. If Granola desktop encrypts `stored-accounts.json`
//! in a future release, only new logins break — existing installs keep
//! working until their refresh token chain dies.

#[cfg(any(target_os = "macos", test))]
use aes::Aes128;
#[cfg(any(target_os = "macos", test))]
use aes_gcm::aead::{AeadInPlace, KeyInit};
#[cfg(any(target_os = "macos", test))]
use aes_gcm::{Aes256Gcm, Nonce, Tag};
#[cfg(target_os = "macos")]
use base64::prelude::{Engine as _, BASE64_STANDARD};
#[cfg(any(target_os = "macos", test))]
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
#[cfg(any(target_os = "macos", test))]
use cbc::Decryptor;
#[cfg(any(target_os = "macos", test))]
use pbkdf2::pbkdf2_hmac;
#[cfg(any(target_os = "macos", test))]
use sha1::Sha1;
use std::fs::{File, OpenOptions};
use std::io;
#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};

const SERVICE_NAME: &str = "com.granola.cli";
const ACCOUNT_NAME: &str = "credentials";
const DEFAULT_CLIENT_ID: &str = "client_GranolaMac";
const WORKOS_AUTH_URL: &str = "https://api.workos.com/user_management/authenticate";
#[cfg(target_os = "macos")]
const GRANOLA_SAFE_STORAGE_SERVICE: &str = "Granola Safe Storage";
#[cfg(target_os = "macos")]
const GRANOLA_SAFE_STORAGE_ACCOUNT: &str = "Granola Key";
#[cfg(any(target_os = "macos", test))]
const MAC_SAFE_STORAGE_PREFIX: &[u8] = b"v10";
#[cfg(any(target_os = "macos", test))]
const MAC_SAFE_STORAGE_SALT: &[u8] = b"saltysalt";
#[cfg(any(target_os = "macos", test))]
const MAC_SAFE_STORAGE_ITERATIONS: u32 = 1003;
#[cfg(any(target_os = "macos", test))]
const MAC_SAFE_STORAGE_KEY_LENGTH: usize = 16;
#[cfg(any(target_os = "macos", test))]
const MAC_SAFE_STORAGE_IV: [u8; 16] = [b' '; 16];
#[cfg(any(target_os = "macos", test))]
const GRANOLA_STORAGE_KEY_LENGTH: usize = 32;
#[cfg(any(target_os = "macos", test))]
const GRANOLA_STORAGE_IV_LENGTH: usize = 12;
#[cfg(any(target_os = "macos", test))]
const GRANOLA_STORAGE_AUTH_TAG_LENGTH: usize = 16;

#[cfg(any(target_os = "macos", test))]
type Aes128CbcDec = Decryptor<Aes128>;
#[cfg(target_os = "macos")]
type CredentialParser = fn(&str) -> Option<Credentials>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("keychain: {0}")]
    Keyring(#[from] keyring::Error),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("network: {0}")]
    Http(#[from] Box<ureq::Error>),
    #[error("no credentials in keychain — run `granola auth login`")]
    NoCredentials,
    #[error("refresh token rejected by WorkOS (HTTP {status})")]
    RefreshRejected { status: u16 },
    #[error("could not locate Granola desktop credentials — tried {tried:?}")]
    NoDesktopCredentials { tried: Vec<PathBuf> },
    #[cfg(any(target_os = "macos", test))]
    #[error("could not read encrypted Granola desktop credentials: {0}")]
    EncryptedDesktopCredentials(String),
    #[error("could not determine user home/cache directory")]
    NoHomeDir,
}

impl From<ureq::Error> for Error {
    fn from(e: ureq::Error) -> Self {
        Error::Http(Box::new(e))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Credentials {
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
    #[serde(rename = "accessToken", default)]
    pub access_token: String,
    #[serde(rename = "clientId")]
    pub client_id: String,
}

// ---- Keychain I/O -----------------------------------------------------------

fn entry() -> Result<keyring::Entry, Error> {
    Ok(keyring::Entry::new(SERVICE_NAME, ACCOUNT_NAME)?)
}

pub fn get_credentials() -> Result<Option<Credentials>, Error> {
    let e = entry()?;
    match e.get_password() {
        Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(other) => Err(other.into()),
    }
}

pub fn save_credentials(creds: &Credentials) -> Result<(), Error> {
    let s = serde_json::to_string(creds)?;
    entry()?.set_password(&s)?;
    Ok(())
}

pub fn delete_credentials() -> Result<(), Error> {
    match entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ---- Desktop credential discovery (the PR #6 fix) ---------------------------

/// Generic "string or already-parsed value" shape for fields that the Granola
/// desktop app sometimes ships as JSON-encoded strings and sometimes as the
/// raw object. Used for `accounts` and `tokens` in `stored-accounts.json`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MaybeStr<T> {
    Str(String),
    Val(T),
}

impl<T> MaybeStr<T>
where
    T: for<'de> Deserialize<'de>,
{
    fn into_parsed(self) -> Option<T> {
        match self {
            MaybeStr::Str(s) => serde_json::from_str(&s).ok(),
            MaybeStr::Val(v) => Some(v),
        }
    }
}

#[derive(Debug, Deserialize)]
struct StoredAccountsFile {
    accounts: Option<MaybeStr<Vec<StoredAccount>>>,
}

#[derive(Debug, Deserialize)]
struct StoredAccount {
    tokens: Option<MaybeStr<TokenBlob>>,
}

#[derive(Debug, Deserialize)]
struct TokenBlob {
    access_token: Option<String>,
    refresh_token: Option<String>,
    client_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SupabaseFile {
    workos_tokens: Option<MaybeStr<TokenBlob>>,
    cognito_tokens: Option<MaybeStr<TokenBlob>>,
    // Legacy root-level fallback fields
    refresh_token: Option<String>,
    access_token: Option<String>,
    client_id: Option<String>,
}

pub fn parse_stored_accounts(json: &str) -> Option<Credentials> {
    let file: StoredAccountsFile = serde_json::from_str(json).ok()?;
    let accounts = file.accounts?.into_parsed()?;
    let account = accounts.into_iter().next()?;
    let tokens = account.tokens?.into_parsed()?;
    let access = tokens.access_token?;
    Some(Credentials {
        refresh_token: tokens.refresh_token.unwrap_or_default(),
        access_token: access,
        client_id: tokens.client_id.unwrap_or_else(|| DEFAULT_CLIENT_ID.into()),
    })
}

pub fn parse_supabase(json: &str) -> Option<Credentials> {
    let file: SupabaseFile = serde_json::from_str(json).ok()?;

    // 1. WorkOS (newer auth system). Mirrors the upstream asymmetry: missing
    //    refresh_token is OK here (defaults to ""), unlike Cognito/legacy.
    if let Some(blob) = file.workos_tokens.and_then(MaybeStr::into_parsed) {
        if let Some(access) = blob.access_token {
            return Some(Credentials {
                refresh_token: blob.refresh_token.unwrap_or_default(),
                access_token: access,
                client_id: blob.client_id.unwrap_or_else(|| DEFAULT_CLIENT_ID.into()),
            });
        }
    }

    // 2. Cognito. Hard-fails without refresh_token (upstream auth.ts L126).
    if let Some(blob) = file.cognito_tokens.and_then(MaybeStr::into_parsed) {
        if let Some(refresh) = blob.refresh_token {
            return Some(Credentials {
                refresh_token: refresh,
                access_token: blob.access_token.unwrap_or_default(),
                client_id: blob.client_id.unwrap_or_else(|| DEFAULT_CLIENT_ID.into()),
            });
        }
        return None;
    }

    // 3. Legacy root-level. Hard-fails without refresh_token (upstream L137).
    let refresh = file.refresh_token?;
    Some(Credentials {
        refresh_token: refresh,
        access_token: file.access_token.unwrap_or_default(),
        client_id: file.client_id.unwrap_or_else(|| DEFAULT_CLIENT_ID.into()),
    })
}

fn granola_file(name: &str) -> Option<PathBuf> {
    let base = BaseDirs::new()?;
    // macOS: ~/Library/Application Support/Granola/<name>
    // Windows: %APPDATA%/Granola/<name>
    // Linux: ~/.config/granola/<name>
    #[cfg(target_os = "macos")]
    let path = base
        .home_dir()
        .join("Library")
        .join("Application Support")
        .join("Granola")
        .join(name);
    #[cfg(target_os = "windows")]
    let path = base.config_dir().join("Granola").join(name);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let path = base.home_dir().join(".config").join("granola").join(name);
    Some(path)
}

pub fn stored_accounts_path() -> Option<PathBuf> {
    granola_file("stored-accounts.json")
}

pub fn supabase_path() -> Option<PathBuf> {
    granola_file("supabase.json")
}

#[cfg(target_os = "macos")]
pub fn encrypted_stored_accounts_path() -> Option<PathBuf> {
    granola_file("stored-accounts.json.enc")
}

#[cfg(target_os = "macos")]
pub fn encrypted_supabase_path() -> Option<PathBuf> {
    granola_file("supabase.json.enc")
}

#[cfg(target_os = "macos")]
pub fn storage_dek_path() -> Option<PathBuf> {
    granola_file("storage.dek")
}

#[cfg(any(target_os = "macos", test))]
fn decrypt_mac_safe_storage_value(encrypted_value: &[u8], password: &str) -> Result<String, Error> {
    let payload = encrypted_value
        .strip_prefix(MAC_SAFE_STORAGE_PREFIX)
        .unwrap_or(encrypted_value);

    let mut key = [0_u8; MAC_SAFE_STORAGE_KEY_LENGTH];
    pbkdf2_hmac::<Sha1>(
        password.as_bytes(),
        MAC_SAFE_STORAGE_SALT,
        MAC_SAFE_STORAGE_ITERATIONS,
        &mut key,
    );

    let cipher = Aes128CbcDec::new_from_slices(&key, &MAC_SAFE_STORAGE_IV).map_err(|e| {
        Error::EncryptedDesktopCredentials(format!("invalid safe-storage cipher parameters: {e}"))
    })?;
    let decrypted = cipher
        .decrypt_padded_vec_mut::<Pkcs7>(payload)
        .map_err(|e| {
            Error::EncryptedDesktopCredentials(format!(
                "could not decrypt Granola safe-storage key: {e}"
            ))
        })?;

    String::from_utf8(decrypted).map_err(|e| {
        Error::EncryptedDesktopCredentials(format!(
            "Granola safe-storage key was not valid UTF-8: {e}"
        ))
    })
}

#[cfg(any(target_os = "macos", test))]
fn decrypt_granola_storage(encrypted_value: &[u8], dek: &[u8]) -> Result<String, Error> {
    if dek.len() != GRANOLA_STORAGE_KEY_LENGTH {
        return Err(Error::EncryptedDesktopCredentials(format!(
            "invalid Granola storage key length: expected {GRANOLA_STORAGE_KEY_LENGTH} bytes, got {}",
            dek.len()
        )));
    }
    if encrypted_value.len() < GRANOLA_STORAGE_IV_LENGTH + GRANOLA_STORAGE_AUTH_TAG_LENGTH {
        return Err(Error::EncryptedDesktopCredentials(
            "encrypted Granola storage payload was too short".into(),
        ));
    }

    let iv = &encrypted_value[..GRANOLA_STORAGE_IV_LENGTH];
    let auth_tag = &encrypted_value[encrypted_value.len() - GRANOLA_STORAGE_AUTH_TAG_LENGTH..];
    let encrypted_payload = &encrypted_value
        [GRANOLA_STORAGE_IV_LENGTH..encrypted_value.len() - GRANOLA_STORAGE_AUTH_TAG_LENGTH];

    let cipher = Aes256Gcm::new_from_slice(dek).map_err(|e| {
        Error::EncryptedDesktopCredentials(format!(
            "invalid Granola storage cipher parameters: {e}"
        ))
    })?;
    let mut decrypted = encrypted_payload.to_vec();
    cipher
        .decrypt_in_place_detached(
            Nonce::from_slice(iv),
            b"",
            &mut decrypted,
            Tag::from_slice(auth_tag),
        )
        .map_err(|e| {
            Error::EncryptedDesktopCredentials(format!(
                "could not decrypt Granola desktop storage: {e}"
            ))
        })?;

    String::from_utf8(decrypted).map_err(|e| {
        Error::EncryptedDesktopCredentials(format!(
            "Granola desktop storage was not valid UTF-8: {e}"
        ))
    })
}

#[cfg(target_os = "macos")]
fn read_granola_safe_storage_password() -> Result<String, Error> {
    let entry = keyring::Entry::new(GRANOLA_SAFE_STORAGE_SERVICE, GRANOLA_SAFE_STORAGE_ACCOUNT)?;
    match entry.get_password() {
        Ok(password) => Ok(password),
        Err(keyring::Error::NoEntry) => Err(Error::EncryptedDesktopCredentials(
            "missing Keychain item `Granola Safe Storage` / `Granola Key`".into(),
        )),
        Err(e) => Err(Error::EncryptedDesktopCredentials(format!(
            "could not read Keychain item `Granola Safe Storage` / `Granola Key`: {e}"
        ))),
    }
}

#[cfg(target_os = "macos")]
fn read_granola_storage_dek(dek_path: &Path) -> Result<Vec<u8>, Error> {
    let encrypted_dek = std::fs::read(dek_path)?;
    let password = read_granola_safe_storage_password()?;
    let dek_b64 = decrypt_mac_safe_storage_value(&encrypted_dek, &password)?;
    let dek = BASE64_STANDARD.decode(dek_b64.trim_end()).map_err(|e| {
        Error::EncryptedDesktopCredentials(format!("Granola storage key was not valid base64: {e}"))
    })?;
    if dek.len() != GRANOLA_STORAGE_KEY_LENGTH {
        return Err(Error::EncryptedDesktopCredentials(format!(
            "Granola storage key decoded to {} bytes; expected {GRANOLA_STORAGE_KEY_LENGTH}",
            dek.len()
        )));
    }
    Ok(dek)
}

#[cfg(target_os = "macos")]
fn load_encrypted_credentials_from_file(
    tried: &mut Vec<PathBuf>,
) -> Result<Option<Credentials>, Error> {
    let mut candidates: Vec<(PathBuf, CredentialParser)> = Vec::new();

    if let Some(p) = encrypted_stored_accounts_path() {
        tried.push(p.clone());
        candidates.push((p, parse_stored_accounts));
    }
    if let Some(p) = encrypted_supabase_path() {
        tried.push(p.clone());
        candidates.push((p, parse_supabase));
    }

    let existing: Vec<(PathBuf, CredentialParser)> = candidates
        .into_iter()
        .filter(|(p, _)| p.is_file())
        .collect();
    if existing.is_empty() {
        return Ok(None);
    }

    let dek_path = storage_dek_path().ok_or(Error::NoHomeDir)?;
    tried.push(dek_path.clone());

    // AIDEV-NOTE: Current Granola macOS builds keep plaintext auth files frozen
    // while rotating live tokens in *.enc files. If encrypted files exist, do
    // not silently fall back to plaintext or we can re-import a dead refresh token.
    let dek = read_granola_storage_dek(&dek_path)?;
    let mut failures = Vec::new();

    for (path, parser) in existing {
        let encrypted = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                failures.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let decrypted = match decrypt_granola_storage(&encrypted, &dek) {
            Ok(content) => content,
            Err(e) => {
                failures.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        if let Some(creds) = parser(&decrypted) {
            return Ok(Some(creds));
        }
        failures.push(format!(
            "{} decrypted but did not match the expected credential shape",
            path.display()
        ));
    }

    Err(Error::EncryptedDesktopCredentials(format!(
        "found encrypted Granola desktop credentials but could not import them: {}",
        failures.join("; ")
    )))
}

#[cfg(not(target_os = "macos"))]
fn load_encrypted_credentials_from_file(
    _tried: &mut Vec<PathBuf>,
) -> Result<Option<Credentials>, Error> {
    Ok(None)
}

/// Read credentials from the Granola desktop app.
///
/// Tries `stored-accounts.json` first (Granola desktop ≥7.162), falls back
/// to `supabase.json`. Called only by `granola auth login`. After import,
/// credentials live in the keychain.
pub fn load_credentials_from_file() -> Result<Credentials, Error> {
    let mut tried = Vec::new();

    if let Some(creds) = load_encrypted_credentials_from_file(&mut tried)? {
        return Ok(creds);
    }

    if let Some(p) = stored_accounts_path() {
        tried.push(p.clone());
        if let Ok(content) = std::fs::read_to_string(&p) {
            if let Some(creds) = parse_stored_accounts(&content) {
                return Ok(creds);
            }
        }
    }

    if let Some(p) = supabase_path() {
        tried.push(p.clone());
        if let Ok(content) = std::fs::read_to_string(&p) {
            if let Some(creds) = parse_supabase(&content) {
                return Ok(creds);
            }
        }
    }

    Err(Error::NoDesktopCredentials { tried })
}

// ---- Refresh (single-use, under file lock) ----------------------------------

fn refresh_lock_path() -> Result<PathBuf, Error> {
    let dirs = ProjectDirs::from("com", "granola", "granola-cli").ok_or(Error::NoHomeDir)?;
    let dir = dirs.cache_dir();
    std::fs::create_dir_all(dir)?;
    Ok(dir.join("refresh.lock"))
}

#[derive(Deserialize)]
struct WorkOsRefreshResponse {
    access_token: String,
    refresh_token: String,
}

/// Refresh the access token via WorkOS. Critical: WorkOS refresh tokens are
/// single-use — each call returns a new one that must be saved immediately,
/// or the chain is broken.
///
/// The read-creds → POST → save-creds sequence runs under an exclusive
/// `fd-lock` so two concurrent `granola` processes can't both consume the
/// same refresh token. (This can't protect against the desktop app
/// independently rotating the file — but the desktop app writes to the
/// file, not the keychain, so we're isolated post-login.)
pub fn refresh_access_token() -> Result<Credentials, Error> {
    let lock_path = refresh_lock_path()?;
    let file: File = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = lock.write()?;

    // Re-read inside the lock — another process may have refreshed already.
    let creds = get_credentials()?.ok_or(Error::NoCredentials)?;
    if creds.refresh_token.is_empty() {
        return Err(Error::NoCredentials);
    }

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();

    let body = serde_json::json!({
        "client_id": creds.client_id,
        "grant_type": "refresh_token",
        "refresh_token": creds.refresh_token,
    });

    let response = match agent.post(WORKOS_AUTH_URL).send_json(body) {
        Ok(r) => r,
        Err(ureq::Error::Status(status, _resp)) => {
            return Err(Error::RefreshRejected { status });
        }
        Err(e) => return Err(e.into()),
    };

    let parsed: WorkOsRefreshResponse = response.into_json()?;
    let new_creds = Credentials {
        refresh_token: parsed.refresh_token,
        access_token: parsed.access_token,
        client_id: creds.client_id,
    };
    save_credentials(&new_creds)?;
    Ok(new_creds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::Aes128;
    use aes_gcm::aead::{AeadInPlace, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use base64::prelude::BASE64_STANDARD;
    #[cfg(not(target_os = "macos"))]
    use base64::Engine as _;
    use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
    use cbc::Encryptor;
    use pbkdf2::pbkdf2_hmac;
    use sha1::Sha1;

    type Aes128CbcEnc = Encryptor<Aes128>;

    fn encrypt_mac_safe_storage_value(value: &str, password: &str) -> Vec<u8> {
        let mut key = [0_u8; MAC_SAFE_STORAGE_KEY_LENGTH];
        pbkdf2_hmac::<Sha1>(
            password.as_bytes(),
            MAC_SAFE_STORAGE_SALT,
            MAC_SAFE_STORAGE_ITERATIONS,
            &mut key,
        );

        let cipher = Aes128CbcEnc::new_from_slices(&key, &MAC_SAFE_STORAGE_IV).unwrap();
        let mut encrypted = MAC_SAFE_STORAGE_PREFIX.to_vec();
        encrypted.extend(cipher.encrypt_padded_vec_mut::<Pkcs7>(value.as_bytes()));
        encrypted
    }

    fn encrypt_granola_storage(value: &str, dek: &[u8]) -> Vec<u8> {
        let iv = [7_u8; GRANOLA_STORAGE_IV_LENGTH];
        let cipher = Aes256Gcm::new_from_slice(dek).unwrap();
        let mut encrypted = value.as_bytes().to_vec();
        let tag = cipher
            .encrypt_in_place_detached(Nonce::from_slice(&iv), b"", &mut encrypted)
            .unwrap();

        let mut blob = Vec::with_capacity(iv.len() + encrypted.len() + tag.len());
        blob.extend_from_slice(&iv);
        blob.extend_from_slice(&encrypted);
        blob.extend_from_slice(tag.as_slice());
        blob
    }

    #[test]
    fn stored_accounts_with_stringified_inner_fields() {
        // The shape Granola desktop ≥7.162 actually ships: both `accounts`
        // and `tokens` are JSON-encoded strings.
        let tokens =
            r#"{"access_token":"AT123","refresh_token":"RT123","client_id":"client_GranolaMac"}"#;
        let accounts_str = format!(
            r#"[{{"userId":"u1","email":"x@example.com","tokens":{}}}]"#,
            serde_json::to_string(tokens).unwrap()
        );
        let file = format!(
            r#"{{"accounts":{}}}"#,
            serde_json::to_string(&accounts_str).unwrap()
        );

        let creds = parse_stored_accounts(&file).expect("parse");
        assert_eq!(creds.access_token, "AT123");
        assert_eq!(creds.refresh_token, "RT123");
        assert_eq!(creds.client_id, "client_GranolaMac");
    }

    #[test]
    fn stored_accounts_with_parsed_inner_fields() {
        let file = serde_json::json!({
            "accounts": [
                {
                    "userId": "u1",
                    "tokens": {
                        "access_token": "AT123",
                        "refresh_token": "RT123",
                        "client_id": "client_GranolaMac"
                    }
                }
            ]
        });
        let creds = parse_stored_accounts(&file.to_string()).expect("parse");
        assert_eq!(creds.access_token, "AT123");
        assert_eq!(creds.refresh_token, "RT123");
    }

    #[test]
    fn stored_accounts_missing_access_token_returns_none() {
        let file = serde_json::json!({
            "accounts": [{ "tokens": { "refresh_token": "RT123" } }]
        });
        assert!(parse_stored_accounts(&file.to_string()).is_none());
    }

    #[test]
    fn supabase_workos_format() {
        let workos_tokens =
            r#"{"access_token":"AT","refresh_token":"RT","client_id":"client_GranolaMac"}"#;
        let file = format!(
            r#"{{"workos_tokens":{}}}"#,
            serde_json::to_string(workos_tokens).unwrap()
        );
        let creds = parse_supabase(&file).expect("parse");
        assert_eq!(creds.access_token, "AT");
        assert_eq!(creds.refresh_token, "RT");
    }

    #[test]
    fn supabase_workos_accepts_missing_refresh_token() {
        // Upstream asymmetry: WorkOS branch tolerates missing refresh_token.
        let workos_tokens = r#"{"access_token":"AT","client_id":"client_GranolaMac"}"#;
        let file = format!(
            r#"{{"workos_tokens":{}}}"#,
            serde_json::to_string(workos_tokens).unwrap()
        );
        let creds = parse_supabase(&file).expect("parse");
        assert_eq!(creds.refresh_token, "");
    }

    #[test]
    fn supabase_cognito_rejects_missing_refresh_token() {
        // Upstream asymmetry: Cognito branch hard-fails without refresh_token.
        let cognito_tokens = r#"{"access_token":"AT"}"#;
        let file = format!(
            r#"{{"cognito_tokens":{}}}"#,
            serde_json::to_string(cognito_tokens).unwrap()
        );
        assert!(parse_supabase(&file).is_none());
    }

    #[test]
    fn supabase_legacy_root_level() {
        let file = serde_json::json!({
            "refresh_token": "RT",
            "access_token": "AT",
            "client_id": "client_legacy"
        });
        let creds = parse_supabase(&file.to_string()).expect("parse");
        assert_eq!(creds.refresh_token, "RT");
        assert_eq!(creds.client_id, "client_legacy");
    }

    #[test]
    fn supabase_legacy_missing_refresh_token() {
        let file = serde_json::json!({ "access_token": "AT" });
        assert!(parse_supabase(&file.to_string()).is_none());
    }

    #[test]
    fn encrypted_stored_accounts_round_trip() {
        let dek = vec![0xAB; GRANOLA_STORAGE_KEY_LENGTH];
        let dek_b64 = BASE64_STANDARD.encode(&dek);
        let wrapped_dek = encrypt_mac_safe_storage_value(&dek_b64, "test-password");

        let tokens =
            r#"{"access_token":"AT123","refresh_token":"RT123","client_id":"client_GranolaMac"}"#;
        let accounts_str = format!(
            r#"[{{"userId":"u1","email":"x@example.com","tokens":{}}}]"#,
            serde_json::to_string(tokens).unwrap()
        );
        let file = format!(
            r#"{{"accounts":{}}}"#,
            serde_json::to_string(&accounts_str).unwrap()
        );
        let encrypted_file = encrypt_granola_storage(&file, &dek);

        let unwrapped_dek_b64 =
            decrypt_mac_safe_storage_value(&wrapped_dek, "test-password").expect("unwrap dek");
        assert_eq!(unwrapped_dek_b64, dek_b64);

        let decoded_dek = BASE64_STANDARD
            .decode(unwrapped_dek_b64)
            .expect("decode dek");
        let plaintext =
            decrypt_granola_storage(&encrypted_file, &decoded_dek).expect("decrypt storage");
        let creds = parse_stored_accounts(&plaintext).expect("parse stored-accounts");

        assert_eq!(creds.access_token, "AT123");
        assert_eq!(creds.refresh_token, "RT123");
        assert_eq!(creds.client_id, "client_GranolaMac");
    }

    #[test]
    fn encrypted_supabase_round_trip_prefers_workos_tokens() {
        let dek = vec![0xCD; GRANOLA_STORAGE_KEY_LENGTH];
        let file = serde_json::json!({
            "access_token": "STALE_TOP_LEVEL_ACCESS",
            "refresh_token": "STALE_TOP_LEVEL_REFRESH",
            "workos_tokens": serde_json::to_string(&serde_json::json!({
                "access_token": "CURRENT_WORKOS_ACCESS",
                "refresh_token": "CURRENT_WORKOS_REFRESH"
            }))
            .unwrap(),
        });

        let plaintext =
            decrypt_granola_storage(&encrypt_granola_storage(&file.to_string(), &dek), &dek)
                .expect("decrypt storage");
        let creds = parse_supabase(&plaintext).expect("parse supabase");

        assert_eq!(creds.access_token, "CURRENT_WORKOS_ACCESS");
        assert_eq!(creds.refresh_token, "CURRENT_WORKOS_REFRESH");
        assert_eq!(creds.client_id, DEFAULT_CLIENT_ID);
    }
}
