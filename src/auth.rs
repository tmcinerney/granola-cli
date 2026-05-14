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

use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};

const SERVICE_NAME: &str = "com.granola.cli";
const ACCOUNT_NAME: &str = "credentials";
const DEFAULT_CLIENT_ID: &str = "client_GranolaMac";
const WORKOS_AUTH_URL: &str = "https://api.workos.com/user_management/authenticate";

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

/// Read credentials from the Granola desktop app.
///
/// Tries `stored-accounts.json` first (Granola desktop ≥7.162), falls back
/// to `supabase.json`. Called only by `granola auth login`. After import,
/// credentials live in the keychain.
pub fn load_credentials_from_file() -> Result<Credentials, Error> {
    let mut tried = Vec::new();

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
}
