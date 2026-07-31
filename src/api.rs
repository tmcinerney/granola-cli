//! Granola HTTP API client.
//!
//! Sync (`ureq`) — this is a one-shot serial CLI. The `with_token_refresh`
//! pattern handles WorkOS refresh-on-401; retries 429/5xx with exponential
//! backoff (250ms base, 3 attempts) per the upstream `http.ts` defaults.

use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Mutex, OnceLock};

use crate::auth;

const BASE_URL: &str = "https://api.granola.ai";
const APP_VERSION: &str = auth::GRANOLA_CLIENT_VERSION;
const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRIES: u32 = 3;
const BASE_BACKOFF_MS: u64 = 250;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("auth: {0}")]
    Auth(#[from] auth::Error),
    #[error("HTTP {status}: {message}")]
    Http { status: u16, message: String },
    #[error("transport: {0}")]
    Transport(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not authenticated — run `granola auth login`")]
    Unauthenticated,
}

impl From<ureq::Error> for Error {
    fn from(e: ureq::Error) -> Self {
        match e {
            ureq::Error::Status(status, resp) => {
                let message = resp.into_string().unwrap_or_default();
                Error::Http { status, message }
            }
            ureq::Error::Transport(t) => Error::Transport(t.to_string()),
        }
    }
}

#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
    access_token: String,
}

impl Client {
    pub fn from_keychain() -> Result<Self, Error> {
        let creds = auth::get_credentials()?.ok_or(Error::Unauthenticated)?;
        Ok(Self::new(creds.access_token))
    }

    pub fn new(access_token: String) -> Self {
        Self {
            agent: ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build(),
            access_token,
        }
    }

    fn post<B: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<R, Error> {
        let mut last_err: Option<Error> = None;

        for attempt in 0..=MAX_RETRIES {
            let url = format!("{BASE_URL}{path}");
            let req = self
                .agent
                .post(&url)
                .set("Authorization", &format!("Bearer {}", self.access_token))
                .set("Content-Type", "application/json")
                .set("X-App-Version", APP_VERSION)
                .set("X-Client-Version", APP_VERSION)
                .set("X-Client-Type", "cli")
                .set("X-Client-Platform", std::env::consts::OS)
                .set("X-Client-Architecture", std::env::consts::ARCH)
                .set("X-Client-Id", &format!("granola-cli-{CLI_VERSION}"))
                .set(
                    "User-Agent",
                    &format!(
                        "Granola/{APP_VERSION} granola-cli/{CLI_VERSION} ({} {})",
                        std::env::consts::OS,
                        std::env::consts::ARCH
                    ),
                );

            match req.send_json(serde_json::to_value(body)?) {
                Ok(resp) => {
                    return resp
                        .into_json()
                        .map_err(|e| Error::Transport(e.to_string()))
                }
                Err(ureq::Error::Status(status, _)) if status == 401 => {
                    return Err(Error::Http {
                        status,
                        message: "unauthorized".into(),
                    });
                }
                Err(ureq::Error::Status(status, _resp))
                    if matches!(status, 429 | 500 | 502 | 503 | 504) && attempt < MAX_RETRIES =>
                {
                    let delay = BASE_BACKOFF_MS * 2u64.pow(attempt);
                    thread::sleep(Duration::from_millis(delay));
                    last_err = Some(Error::Http {
                        status,
                        message: "retryable".into(),
                    });
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }

        Err(last_err.unwrap_or(Error::Transport("retries exhausted".into())))
    }
}

/// Run `f` with automatic token refresh on a single 401. If `f` returns 401,
/// refresh the token (which saves it to the keychain), rebuild a `Client`,
/// and retry once. Any second 401 propagates.
/// Process-wide credential cache.
///
/// AIDEV-NOTE: this exists to stop the keychain being read once per operation.
/// It matters most for `granola mcp`, which is long-lived: every tool call went
/// through with_token_refresh, and get_meeting_context did so three times, so a
/// working session meant a steady stream of keychain reads. On macOS those can
/// prompt for the login password — the release binaries are ad-hoc/linker-signed
/// with no stable Team identity, and the code hash changes every build, so the
/// keychain treats each version as a new binary and cannot persist an
/// "always allow" grant. Caching cannot fix that (only a stable Developer ID
/// signature can) but it reduces the prompts from per-call to at most one per
/// process, and it removes the burst of concurrent reads that was intermittently
/// failing with "Platform secure storage failure".
fn credential_cache() -> &'static Mutex<Option<Client>> {
    static CACHE: OnceLock<Mutex<Option<Client>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Replace the cached client. Used after a token refresh, and by tests.
pub(crate) fn cache_client(client: Client) {
    let mut guard = credential_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(client);
}

/// Drop any cached client, so the next call re-reads the keychain.
///
/// AIDEV-NOTE: called by the auth subcommands. Largely belt-and-braces because
/// each CLI invocation is its own process, but it keeps the cache honest if a
/// single process ever both changes credentials and makes requests.
pub(crate) fn clear_cached_client() {
    let mut guard = credential_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = None;
}

/// The cached client, loading it from the keychain on first use.
///
/// The lock is deliberately held across the keychain read so that concurrent
/// first-callers queue behind one read instead of racing into several.
fn cached_client() -> Result<Client, Error> {
    let mut guard = credential_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(client) = guard.as_ref() {
        return Ok(client.clone());
    }
    let client = Client::from_keychain()?;
    *guard = Some(client.clone());
    Ok(client)
}

/// Run `f` with valid credentials, refreshing once on a 401.
///
/// AIDEV-NOTE: a refreshed token replaces the cache, so a long-lived server
/// recovers without re-reading the keychain on every later call. The flip side
/// is that credentials rotated by another process are not picked up until this
/// one gets a 401 — acceptable, because that path then self-heals.
pub fn with_token_refresh<F, T>(mut f: F) -> Result<T, Error>
where
    F: FnMut(&Client) -> Result<T, Error>,
{
    let client = cached_client()?;
    match f(&client) {
        Ok(v) => Ok(v),
        Err(Error::Http { status: 401, .. }) => {
            let new_creds = auth::refresh_access_token()?;
            let retry_client = Client::new(new_creds.access_token);
            cache_client(retry_client.clone());
            f(&retry_client)
        }
        Err(e) => Err(e),
    }
}

// ---- Endpoint methods -------------------------------------------------------

impl Client {
    pub fn get_workspaces(&self) -> Result<Value, Error> {
        self.post::<_, Value>("/v1/get-workspaces", &serde_json::json!({}))
    }

    pub fn get_documents(
        &self,
        limit: u32,
        offset: u32,
        include_panel: bool,
    ) -> Result<Value, Error> {
        let body = serde_json::json!({
            "limit": limit,
            "offset": offset,
            "include_last_viewed_panel": include_panel,
        });
        self.post::<_, Value>("/v2/get-documents", &body)
    }

    pub fn get_document_lists(&self) -> Result<Value, Error> {
        self.post::<_, Value>("/v2/get-document-lists", &serde_json::json!({}))
    }

    pub fn get_documents_batch(&self, ids: &[String], include_panel: bool) -> Result<Value, Error> {
        let body = serde_json::json!({
            "document_ids": ids,
            "include_last_viewed_panel": include_panel,
        });
        self.post::<_, Value>("/v1/get-documents-batch", &body)
    }

    // Kept available for callers that want only attendees/conferencing/creator/url
    // without paying for the full document body. Notes-content paths use
    // `get_documents_batch` instead, which is more reliable across account types.
    #[allow(dead_code)]
    pub fn get_document_metadata(&self, id: &str) -> Result<Value, Error> {
        let body = serde_json::json!({ "document_id": id });
        self.post::<_, Value>("/v1/get-document-metadata", &body)
    }

    pub fn get_document_transcript(&self, id: &str) -> Result<Value, Error> {
        let body = serde_json::json!({ "document_id": id });
        self.post::<_, Value>("/v1/get-document-transcript", &body)
    }
}

#[cfg(test)]
mod tests {
    use super::{cache_client, clear_cached_client, with_token_refresh, Client};

    /// A seeded cache must be used without touching the keychain.
    ///
    /// AIDEV-NOTE: this is a real assertion on CI (Linux/Windows), where there
    /// is no stored credential — if caching regressed, `with_token_refresh`
    /// would fall through to the keychain and return Err instead of the value.
    #[test]
    fn a_cached_client_is_reused_without_reading_the_keychain() {
        cache_client(Client::new("seeded-token".into()));

        let calls = std::cell::Cell::new(0);
        for _ in 0..3 {
            let got = with_token_refresh(|_client| {
                calls.set(calls.get() + 1);
                Ok(7)
            })
            .expect("cached credentials must satisfy with_token_refresh");
            assert_eq!(got, 7);
        }
        assert_eq!(calls.get(), 3, "closure should run once per call");

        clear_cached_client();
    }
}
