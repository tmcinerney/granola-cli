//! Authentication: credential discovery, keychain storage, and token refresh.
//!
//! This module is a direct port of the upstream `src/lib/auth.ts` with the
//! `stored-accounts.json` fix from beaulebens/granola-cli#6.
//!
//! Lifecycle: `auth login` is the only path that reads files from the Granola
//! desktop app. After import, credentials live in the OS keychain. Granola
//! 7.427+ moved its macOS DEK into an app-only Keychain access group, so an
//! upgraded install bootstraps once from its leftover plaintext refresh token
//! and then owns a separately persisted rotation chain.

#[cfg(any(target_os = "macos", test))]
use aes::Aes128;
#[cfg(any(target_os = "macos", test))]
use aes_gcm::aead::{AeadInOut, KeyInit};
#[cfg(any(target_os = "macos", test))]
use aes_gcm::{Aes256Gcm, Nonce, Tag};
#[cfg(target_os = "macos")]
use base64::prelude::{Engine as _, BASE64_STANDARD};
#[cfg(any(target_os = "macos", test))]
use cbc::cipher::{block_padding::Pkcs7, BlockModeDecrypt, KeyIvInit};
#[cfg(any(target_os = "macos", test))]
use cbc::Decryptor;
#[cfg(any(target_os = "macos", test))]
use pbkdf2::pbkdf2_hmac;
#[cfg(any(target_os = "macos", test))]
use sha1::Sha1;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};

const SERVICE_NAME: &str = "com.granola.cli";
const ACCOUNT_NAME: &str = "credentials";
const DEFAULT_CLIENT_ID: &str = "client_GranolaMac";
#[cfg(target_os = "macos")]
const GRANOLA_REFRESH_URL: &str = "https://api.granola.ai/v1/refresh-access-token";
#[cfg(not(target_os = "macos"))]
const WORKOS_AUTH_URL: &str = "https://api.workos.com/user_management/authenticate";
pub(crate) const GRANOLA_CLIENT_VERSION: &str = "7.427.3";
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

// Names of the credential files the Granola desktop app writes. Named because
// `desktop_state_at` probes the same set the import paths read, and a drifting
// literal there would misreport which recovery is possible.
const STORED_ACCOUNTS_FILE: &str = "stored-accounts.json";
const SUPABASE_FILE: &str = "supabase.json";
const ENCRYPTED_STORED_ACCOUNTS_FILE: &str = "stored-accounts.json.enc";
const ENCRYPTED_SUPABASE_FILE: &str = "supabase.json.enc";
const STORAGE_DEK_FILE: &str = "storage.dek";

#[cfg(any(target_os = "macos", test))]
type Aes128CbcDec = Decryptor<Aes128>;
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
    // Front-end neutral: this text reaches both a terminal user and an MCP
    // agent, and they recover differently. See `api::RecoveryHint`.
    #[error("no usable credentials in the OS keychain")]
    NoCredentials,
    #[error("refresh token rejected by authentication provider (HTTP {status})")]
    RefreshRejected { status: u16 },
    #[error("could not locate Granola desktop credentials — tried {tried:?}")]
    NoDesktopCredentials { tried: Vec<PathBuf> },
    #[cfg(target_os = "macos")]
    #[error("Granola moved its desktop encryption key into an app-only Keychain access group")]
    DesktopKeyMigrated,
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

fn granola_dir() -> Option<PathBuf> {
    let base = BaseDirs::new()?;
    // macOS: ~/Library/Application Support/Granola/
    // Windows: %APPDATA%/Granola/
    // Linux: ~/.config/granola/
    #[cfg(target_os = "macos")]
    let dir = base
        .home_dir()
        .join("Library")
        .join("Application Support")
        .join("Granola");
    #[cfg(target_os = "windows")]
    let dir = base.config_dir().join("Granola");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let dir = base.home_dir().join(".config").join("granola");
    Some(dir)
}

fn granola_file(name: &str) -> Option<PathBuf> {
    Some(granola_dir()?.join(name))
}

pub fn stored_accounts_path() -> Option<PathBuf> {
    granola_file(STORED_ACCOUNTS_FILE)
}

pub fn supabase_path() -> Option<PathBuf> {
    granola_file(SUPABASE_FILE)
}

#[cfg(target_os = "macos")]
pub fn encrypted_stored_accounts_path() -> Option<PathBuf> {
    granola_file(ENCRYPTED_STORED_ACCOUNTS_FILE)
}

#[cfg(target_os = "macos")]
pub fn encrypted_supabase_path() -> Option<PathBuf> {
    granola_file(ENCRYPTED_SUPABASE_FILE)
}

#[cfg(target_os = "macos")]
pub fn storage_dek_path() -> Option<PathBuf> {
    granola_file(STORAGE_DEK_FILE)
}

// ---- Desktop state probe ----------------------------------------------------

/// Which Granola desktop credential sources exist on disk.
///
/// AIDEV-NOTE: presence only — never token values. This is serialised into
/// `granola auth status` and `granola_auth_status` output, and it is the
/// diagnostic that says *which* recovery is possible. Reporting it is the point:
/// without it a dead bootstrap token and a transient 401 look identical, which
/// is how "run `granola auth login`, it opens a browser flow" became the advice
/// given for a state no browser flow exists for.
///
/// Every field exists on every platform so the JSON shape cannot fork across
/// the Linux/Windows/macOS CI matrix. The fields state file facts only; whether
/// a given import path applies is the caller's judgement, because the encrypted
/// and bootstrap paths are macOS-only.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DesktopState {
    /// Plaintext credential files present.
    pub plaintext_files: Vec<String>,
    /// Encrypted (`*.enc`) credential files present.
    pub encrypted_files: Vec<String>,
    /// Whether `storage.dek` sits beside the encrypted files. Absent means
    /// Granola has moved its key into app-only Keychain storage.
    pub storage_dek_present: bool,
    /// Whether a plaintext file still carries a non-empty refresh token — what
    /// the post-migration bootstrap exchanges.
    pub plaintext_refresh_token_present: bool,
    /// Whether importing would have to decrypt the DEK through Granola's own
    /// Keychain item, which blocks on a GUI dialog until answered.
    pub needs_cross_app_keychain: bool,
}

/// `DesktopState` for an explicit directory. Split out so it is testable
/// without a `$HOME` that happens to contain a Granola install.
fn desktop_state_at(dir: &Path) -> DesktopState {
    let present = |name: &str| dir.join(name).is_file();
    let existing = |names: &[&str]| -> Vec<String> {
        names
            .iter()
            .filter(|n| present(n))
            .map(|n| (*n).to_string())
            .collect()
    };

    let encrypted_files = existing(&[ENCRYPTED_STORED_ACCOUNTS_FILE, ENCRYPTED_SUPABASE_FILE]);
    let storage_dek_present = present(STORAGE_DEK_FILE);

    DesktopState {
        plaintext_files: existing(&[STORED_ACCOUNTS_FILE, SUPABASE_FILE]),
        // AIDEV-NOTE: the cross-app Keychain read happens only when there is
        // something encrypted to decrypt *and* a wrapped DEK to unwrap. With
        // `.enc` files but no `storage.dek`, `load_encrypted_credentials_from_file`
        // returns DesktopKeyMigrated before touching the Keychain — so that state
        // cannot raise a dialog, and implicit recovery is safe there.
        needs_cross_app_keychain: !encrypted_files.is_empty() && storage_dek_present,
        encrypted_files,
        storage_dek_present,
        plaintext_refresh_token_present: plaintext_refresh_token_at(dir),
    }
}

/// Whether either plaintext file parses to a non-empty refresh token.
///
/// Mirrors what `load_legacy_refresh_credentials` accepts, without returning the
/// token or the paths it tried.
fn plaintext_refresh_token_at(dir: &Path) -> bool {
    let candidates: [(&str, CredentialParser); 2] = [
        (STORED_ACCOUNTS_FILE, parse_stored_accounts),
        (SUPABASE_FILE, parse_supabase),
    ];
    candidates.iter().any(|(name, parse)| {
        std::fs::read_to_string(dir.join(name))
            .ok()
            .and_then(|content| parse(&content))
            .is_some_and(|creds| !creds.refresh_token.is_empty())
    })
}

/// `DesktopState` for the real Granola application-support directory.
pub fn desktop_state() -> DesktopState {
    match granola_dir() {
        Some(dir) => desktop_state_at(&dir),
        // No home directory: nothing is discoverable, which is what an
        // all-empty state already says.
        None => desktop_state_at(Path::new("")),
    }
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
    let decrypted = cipher.decrypt_padded_vec::<Pkcs7>(payload).map_err(|e| {
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
    // AIDEV-NOTE: aead 0.6 moved the slice-based detached helpers onto the
    // deprecated AeadInPlace trait, and CI builds with `-D warnings`, so this
    // uses the current `decrypt_inout_detached` + InOutBuf form. Nonce/Tag are
    // fixed-size arrays now, hence TryFrom rather than the deprecated
    // `from_slice` (which panicked on a length mismatch instead of erroring).
    let nonce = Nonce::try_from(iv).map_err(|_| {
        Error::EncryptedDesktopCredentials(format!(
            "Granola storage nonce was {} bytes, expected {GRANOLA_STORAGE_IV_LENGTH}",
            iv.len()
        ))
    })?;
    let tag = Tag::try_from(auth_tag).map_err(|_| {
        Error::EncryptedDesktopCredentials(format!(
            "Granola storage auth tag was {} bytes, expected {GRANOLA_STORAGE_AUTH_TAG_LENGTH}",
            auth_tag.len()
        ))
    })?;
    let mut decrypted = encrypted_payload.to_vec();
    cipher
        .decrypt_inout_detached(&nonce, b"", decrypted.as_mut_slice().into(), &tag)
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

    if !dek_path.is_file() {
        return Err(Error::DesktopKeyMigrated);
    }

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

/// Load a leftover plaintext refresh token for the one-time post-migration
/// bootstrap. The access token in these files is intentionally ignored: it is
/// stale, but Granola's refresh proxy may still accept the refresh token and
/// return a new independently rotated credential pair.
#[cfg(target_os = "macos")]
pub fn load_legacy_refresh_credentials() -> Result<Credentials, Error> {
    let mut tried = Vec::new();

    if let Some(p) = stored_accounts_path() {
        tried.push(p.clone());
        if let Ok(content) = std::fs::read_to_string(&p) {
            if let Some(creds) = parse_stored_accounts(&content) {
                if !creds.refresh_token.is_empty() {
                    return Ok(creds);
                }
            }
        }
    }

    if let Some(p) = supabase_path() {
        tried.push(p.clone());
        if let Ok(content) = std::fs::read_to_string(&p) {
            if let Some(creds) = parse_supabase(&content) {
                if !creds.refresh_token.is_empty() {
                    return Ok(creds);
                }
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

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Deserialize)]
struct GranolaRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[cfg(not(target_os = "macos"))]
#[derive(Deserialize)]
struct WorkOsRefreshResponse {
    access_token: String,
    refresh_token: String,
}

#[cfg(any(target_os = "macos", test))]
fn credentials_from_granola_refresh(
    current: &Credentials,
    refreshed: GranolaRefreshResponse,
) -> Credentials {
    Credentials {
        refresh_token: refreshed
            .refresh_token
            .filter(|token| !token.is_empty())
            .unwrap_or_else(|| current.refresh_token.clone()),
        access_token: refreshed.access_token,
        client_id: current.client_id.clone(),
    }
}

#[cfg(target_os = "macos")]
fn exchange_refresh_token(creds: &Credentials) -> Result<Credentials, Error> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();

    let body = serde_json::json!({
        "refresh_token": creds.refresh_token,
    });

    let response = match agent
        .post(GRANOLA_REFRESH_URL)
        .set("Accept", "*/*")
        .set("User-Agent", &format!("Granola/{GRANOLA_CLIENT_VERSION}"))
        .set("X-Client-Version", GRANOLA_CLIENT_VERSION)
        .set("X-Granola-Platform", "darwin")
        .send_json(body)
    {
        Ok(r) => r,
        Err(ureq::Error::Status(status, _resp)) => {
            return Err(Error::RefreshRejected { status });
        }
        Err(e) => return Err(e.into()),
    };

    let parsed: GranolaRefreshResponse = response.into_json()?;
    Ok(credentials_from_granola_refresh(creds, parsed))
}

#[cfg(not(target_os = "macos"))]
fn exchange_refresh_token(creds: &Credentials) -> Result<Credentials, Error> {
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
    Ok(Credentials {
        refresh_token: parsed.refresh_token,
        access_token: parsed.access_token,
        client_id: creds.client_id.clone(),
    })
}

fn with_refresh_lock<T>(f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    let lock_path = refresh_lock_path()?;
    let file: File = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = lock.write()?;
    f()
}

/// Exchange a leftover plaintext refresh token after Granola migrates its DEK
/// into an app-only Keychain access group, then persist the rotated credential
/// pair before returning its access token to the caller.
///
/// Assumes the refresh lock is held; see `login`.
#[cfg(target_os = "macos")]
fn bootstrap_migrated_credentials_locked() -> Result<Credentials, Error> {
    let creds = load_legacy_refresh_credentials()?;
    let new_creds = exchange_refresh_token(&creds)?;
    save_credentials(&new_creds)?;
    Ok(new_creds)
}

/// Where a set of credentials came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSource {
    /// Already in the OS keychain.
    Keychain,
    /// Imported from the Granola desktop app's credential files.
    DesktopImport,
    /// Obtained by exchanging a leftover plaintext refresh token after Granola
    /// moved its desktop encryption key into app-only Keychain storage.
    ///
    /// AIDEV-NOTE: only `login_locked`'s macOS arm constructs this, so off macOS
    /// it is unconstructible and `-D warnings` fails the Linux/Windows CI legs on
    /// dead_code. Suppressed only on those targets rather than unconditionally, so
    /// macOS still fails if the bootstrap path ever stops being reachable. The
    /// variant stays declared everywhere to keep the serialised shape identical
    /// across platforms.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Bootstrap,
}

/// Import credentials from the Granola desktop app and persist them.
///
/// Assumes the refresh lock is held.
fn login_locked() -> Result<(Credentials, CredentialSource), Error> {
    match load_credentials_from_file() {
        Ok(creds) => {
            save_credentials(&creds)?;
            Ok((creds, CredentialSource::DesktopImport))
        }
        // AIDEV-NOTE: gated because both the variant and the bootstrap it calls
        // are macOS-only. An ungated arm fails the Linux/Windows CI legs, which
        // build with `-D warnings`.
        #[cfg(target_os = "macos")]
        Err(Error::DesktopKeyMigrated) => Ok((
            bootstrap_migrated_credentials_locked()?,
            CredentialSource::Bootstrap,
        )),
        Err(e) => Err(e),
    }
}

/// Re-import credentials from the Granola desktop app, persist them, and report
/// which path supplied them.
///
/// AIDEV-NOTE: this is deliberately not a browser or device-code flow — it is
/// local file I/O plus, on a migrated install, one refresh-token POST. That is
/// what makes it callable from `granola mcp`, which has no terminal. Do not
/// reintroduce a "must be run interactively" claim here.
///
/// AIDEV-NOTE: the whole read-import-save sequence holds the refresh lock, so it
/// cannot interleave with `refresh_access_token` in another process, and two
/// concurrent MCP `granola_auth_login` calls cannot stack Keychain prompts.
/// Everything it calls must therefore be the non-locking `*_locked` form —
/// `fd_lock` opens a fresh fd per acquisition, so a nested `with_refresh_lock`
/// in the same process deadlocks against itself rather than reentering.
pub fn login() -> Result<(Credentials, CredentialSource), Error> {
    with_refresh_lock(login_locked)
}

/// Refresh the access token via Granola's desktop refresh proxy. The returned
/// refresh token rotates and must be saved before the caller uses the access
/// token, or a crash can strand the credential chain.
///
/// The read-creds → POST → save-creds sequence runs under an exclusive
/// `fd-lock` so two concurrent `granola` processes can't both consume the
/// same refresh token. (This can't protect against the desktop app
/// independently rotating the file — but the desktop app writes to the
/// file, not the keychain, so we're isolated post-login.)
pub fn refresh_access_token() -> Result<Credentials, Error> {
    with_refresh_lock(|| {
        // Re-read inside the lock — another process may have refreshed already.
        let creds = get_credentials()?.ok_or(Error::NoCredentials)?;
        if creds.refresh_token.is_empty() {
            return Err(Error::NoCredentials);
        }

        let new_creds = exchange_refresh_token(&creds)?;
        save_credentials(&new_creds)?;
        Ok(new_creds)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::Aes128;
    use aes_gcm::aead::{AeadInOut, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use base64::prelude::BASE64_STANDARD;
    #[cfg(not(target_os = "macos"))]
    use base64::Engine as _;
    use cbc::cipher::{block_padding::Pkcs7, BlockModeEncrypt, KeyIvInit};
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
        encrypted.extend(cipher.encrypt_padded_vec::<Pkcs7>(value.as_bytes()));
        encrypted
    }

    fn encrypt_granola_storage(value: &str, dek: &[u8]) -> Vec<u8> {
        let iv = [7_u8; GRANOLA_STORAGE_IV_LENGTH];
        let cipher = Aes256Gcm::new_from_slice(dek).unwrap();
        let mut encrypted = value.as_bytes().to_vec();
        let nonce = Nonce::try_from(&iv[..]).unwrap();
        let tag = cipher
            .encrypt_inout_detached(&nonce, b"", encrypted.as_mut_slice().into())
            .unwrap();

        let mut blob = Vec::with_capacity(iv.len() + encrypted.len() + tag.len());
        blob.extend_from_slice(&iv);
        blob.extend_from_slice(&encrypted);
        blob.extend_from_slice(tag.as_slice());
        blob
    }

    /// A scratch directory that cleans itself up, so `desktop_state_at` can be
    /// tested without a `$HOME` that happens to contain a real Granola install.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "granola-cli-desktop-state-{}-{name}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            ScratchDir(dir)
        }

        fn write(&self, name: &str, contents: &str) {
            std::fs::write(self.0.join(name), contents).expect("write scratch file");
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn plaintext_stored_accounts(refresh_token: &str) -> String {
        let tokens = serde_json::json!({
            "access_token": "AT_PLAINTEXT",
            "refresh_token": refresh_token,
            "client_id": "client_GranolaMac",
        });
        let accounts = serde_json::json!([{ "userId": "u1", "tokens": tokens }]);
        serde_json::json!({ "accounts": accounts }).to_string()
    }

    #[test]
    fn desktop_state_reports_nothing_for_an_empty_directory() {
        let dir = ScratchDir::new("empty");
        let state = desktop_state_at(dir.path());

        assert!(state.plaintext_files.is_empty());
        assert!(state.encrypted_files.is_empty());
        assert!(!state.storage_dek_present);
        assert!(!state.plaintext_refresh_token_present);
        assert!(!state.needs_cross_app_keychain);
    }

    #[test]
    fn desktop_state_reports_a_plaintext_only_install() {
        let dir = ScratchDir::new("plaintext");
        dir.write(STORED_ACCOUNTS_FILE, &plaintext_stored_accounts("RT_LIVE"));
        let state = desktop_state_at(dir.path());

        assert_eq!(state.plaintext_files, vec![STORED_ACCOUNTS_FILE]);
        assert!(state.plaintext_refresh_token_present);
        // Nothing encrypted, so importing never reads Granola's Keychain item.
        assert!(!state.needs_cross_app_keychain);
    }

    /// The pre-migration encrypted layout: importing has to unwrap the DEK
    /// through Granola's own Keychain item, which can block on a GUI dialog.
    #[test]
    fn desktop_state_flags_the_cross_app_keychain_read_when_a_dek_is_present() {
        let dir = ScratchDir::new("dek");
        dir.write(ENCRYPTED_STORED_ACCOUNTS_FILE, "not-really-encrypted");
        dir.write(ENCRYPTED_SUPABASE_FILE, "not-really-encrypted");
        dir.write(STORAGE_DEK_FILE, "wrapped-key");
        let state = desktop_state_at(dir.path());

        assert_eq!(
            state.encrypted_files,
            vec![ENCRYPTED_STORED_ACCOUNTS_FILE, ENCRYPTED_SUPABASE_FILE]
        );
        assert!(state.storage_dek_present);
        assert!(state.needs_cross_app_keychain);
    }

    /// The post-migration layout: encrypted files but no `storage.dek`, with a
    /// frozen plaintext file left behind. `load_encrypted_credentials_from_file`
    /// returns DesktopKeyMigrated before reaching the Keychain, so this state
    /// cannot raise a dialog — which is what makes implicit recovery safe here.
    #[test]
    fn desktop_state_does_not_flag_a_keychain_read_once_the_dek_is_gone() {
        let dir = ScratchDir::new("migrated");
        dir.write(ENCRYPTED_STORED_ACCOUNTS_FILE, "not-really-encrypted");
        dir.write(
            STORED_ACCOUNTS_FILE,
            &plaintext_stored_accounts("RT_FROZEN"),
        );
        let state = desktop_state_at(dir.path());

        assert!(!state.storage_dek_present);
        assert!(!state.needs_cross_app_keychain);
        // The bootstrap exchange has something to work with.
        assert!(state.plaintext_refresh_token_present);
    }

    #[test]
    fn desktop_state_ignores_a_plaintext_file_with_no_refresh_token() {
        let dir = ScratchDir::new("no-refresh");
        dir.write(STORED_ACCOUNTS_FILE, &plaintext_stored_accounts(""));
        let state = desktop_state_at(dir.path());

        assert_eq!(state.plaintext_files, vec![STORED_ACCOUNTS_FILE]);
        assert!(!state.plaintext_refresh_token_present);
    }

    /// Regression guard: the state probe is serialised into `auth status` and
    /// `granola_auth_status` output, so it must never carry token values out of
    /// the files it reads.
    #[test]
    fn desktop_state_never_serialises_token_values() {
        let dir = ScratchDir::new("no-leak");
        dir.write(
            STORED_ACCOUNTS_FILE,
            &plaintext_stored_accounts("SUPER_SECRET_REFRESH"),
        );
        let json = serde_json::to_string(&desktop_state_at(dir.path())).expect("serialise");

        assert!(!json.contains("SUPER_SECRET_REFRESH"), "leaked: {json}");
        assert!(!json.contains("AT_PLAINTEXT"), "leaked: {json}");
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
    fn granola_refresh_rotates_refresh_token() {
        let current = Credentials {
            refresh_token: "RT_OLD".into(),
            access_token: "AT_OLD".into(),
            client_id: "client".into(),
        };
        let refreshed = GranolaRefreshResponse {
            access_token: "AT_NEW".into(),
            refresh_token: Some("RT_NEW".into()),
        };

        assert_eq!(
            credentials_from_granola_refresh(&current, refreshed),
            Credentials {
                refresh_token: "RT_NEW".into(),
                access_token: "AT_NEW".into(),
                client_id: "client".into(),
            }
        );
    }

    #[test]
    fn granola_refresh_preserves_token_when_response_omits_rotation() {
        let current = Credentials {
            refresh_token: "RT_OLD".into(),
            access_token: "AT_OLD".into(),
            client_id: "client".into(),
        };
        let refreshed = GranolaRefreshResponse {
            access_token: "AT_NEW".into(),
            refresh_token: None,
        };

        assert_eq!(
            credentials_from_granola_refresh(&current, refreshed).refresh_token,
            "RT_OLD"
        );
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
