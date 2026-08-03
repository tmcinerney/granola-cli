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

// AIDEV-NOTE: these messages are deliberately front-end neutral — they state
// what happened, never how to fix it. The CLI and the MCP server recover
// differently ("run `granola auth login`" vs "call `granola_auth_login`"), so
// each front end appends its own recovery sentence from `RecoveryHint`. The
// previous hardcoded "run `granola auth login`" reached an MCP agent verbatim
// and was relayed to the user as terminal-only advice for something the server
// can now do itself.
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
    #[error("no Granola credentials are stored")]
    Unauthenticated,
    #[error(
        "stored credentials were rejected, and re-importing them needs one-time approval \
         for Granola's own Keychain item"
    )]
    NeedsKeychainApproval,
}

impl Error {
    /// Whether this failure means the credential chain is dead and only a
    /// re-import can fix it — as opposed to a network or protocol problem.
    pub fn needs_reauth(&self) -> bool {
        matches!(
            self,
            Error::Unauthenticated
                | Error::NeedsKeychainApproval
                | Error::Http { status: 401, .. }
                | Error::Auth(auth::Error::RefreshRejected { .. })
                | Error::Auth(auth::Error::NoCredentials)
        )
    }

    /// Whether this failure is about credentials at all, and so deserves a
    /// recovery hint.
    ///
    /// Broader than `needs_reauth`, and deliberately a separate predicate rather
    /// than a widening of it: `needs_reauth` answers "is the stored chain dead?",
    /// which selects the `stale_credentials` report code, and these extra
    /// variants are not that.
    ///
    /// AIDEV-NOTE: the gap this closes is the cold-start path. A fresh server
    /// with an empty keychain reaches `reimport(None)`, and if the desktop app has
    /// nothing to import the failure surfaces as
    /// `Auth(NoDesktopCredentials)` — which `needs_reauth` rejects, so the agent
    /// got a bare "could not locate Granola desktop credentials" with no next
    /// step. That is the same unactionable-message failure this whole change
    /// exists to remove, reached by a different route.
    pub fn is_auth_failure(&self) -> bool {
        if self.needs_reauth() {
            return true;
        }
        match self {
            Error::Auth(auth::Error::NoDesktopCredentials { .. }) => true,
            Error::Auth(auth::Error::Keyring(_)) => true,
            #[cfg(target_os = "macos")]
            Error::Auth(auth::Error::DesktopKeyMigrated) => true,
            _ => false,
        }
    }
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

/// How far `with_credentials` may go to obtain working credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Exchange the stored refresh token, nothing more.
    RefreshOnly,
    /// If the refresh token is also dead, re-import from the Granola desktop app.
    RefreshThenReimport,
}

/// Re-import credentials from the desktop app and seed the cache.
///
/// `rejected` is the access token that just failed, when there was one. If the
/// keychain now holds a different one, another process or a concurrent tool call
/// already rotated the chain, so use that instead of importing again — the same
/// reasoning as re-reading inside the lock in `auth::refresh_access_token`.
fn reimport(rejected: Option<&str>) -> Result<Client, Error> {
    if let Some(rejected) = rejected {
        if let Some(creds) = auth::get_credentials()? {
            if !creds.access_token.is_empty() && creds.access_token != rejected {
                let client = Client::new(creds.access_token);
                cache_client(client.clone());
                return Ok(client);
            }
        }
    }

    // AIDEV-NOTE: hang safety. Importing on an install that still has a wrapped
    // `storage.dek` must read Granola's own Keychain item, and `keyring` blocks
    // on that GUI dialog until a human answers. A data tool must never park
    // there, so this refuses and lets the caller route to `granola_auth_login`,
    // which bounds the wait. Installs past the DEK migration (no `storage.dek`)
    // never reach that read, which is why implicit recovery is safe for them.
    if auth::desktop_state().needs_cross_app_keychain {
        return Err(Error::NeedsKeychainApproval);
    }

    let (creds, _source) = auth::login()?;
    let client = Client::new(creds.access_token);
    cache_client(client.clone());
    Ok(client)
}

/// Run `f` with valid credentials, refreshing once on a 401.
///
/// AIDEV-NOTE: a refreshed token replaces the cache, so a long-lived server
/// recovers without re-reading the keychain on every later call. The flip side
/// is that credentials rotated by another process are not picked up until this
/// one gets a 401 — acceptable, because that path then self-heals.
pub fn with_token_refresh<F, T>(f: F) -> Result<T, Error>
where
    F: FnMut(&Client) -> Result<T, Error>,
{
    with_credentials(Recovery::RefreshThenReimport, f)
}

/// Run `f` with valid credentials, recovering as far as `policy` allows.
///
/// AIDEV-NOTE: the policy exists to break a recursion. `authenticate()`
/// validates a freshly imported credential with an API call, and that call can
/// itself 401 — under `RefreshThenReimport` it would import again, validate
/// again, and loop. Login and status paths must pass `RefreshOnly`.
pub fn with_credentials<F, T>(policy: Recovery, mut f: F) -> Result<T, Error>
where
    F: FnMut(&Client) -> Result<T, Error>,
{
    let client = match cached_client() {
        Ok(client) => client,
        // Nothing stored at all. With reimport allowed this is recoverable
        // without ever reaching a request — the common case for a freshly
        // spawned MCP server whose keychain entry was never created.
        Err(Error::Unauthenticated) if policy == Recovery::RefreshThenReimport => reimport(None)?,
        Err(e) => return Err(e),
    };

    match f(&client) {
        Ok(v) => Ok(v),
        Err(Error::Http { status: 401, .. }) => {
            let rejected = client.access_token.clone();
            match auth::refresh_access_token() {
                Ok(new_creds) => {
                    let retry_client = Client::new(new_creds.access_token);
                    cache_client(retry_client.clone());
                    f(&retry_client)
                }
                Err(e) if should_reimport(policy, &e) => {
                    let retry_client = reimport(Some(&rejected))?;
                    f(&retry_client)
                }
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e),
    }
}

/// Whether a failed refresh should escalate to a full re-import.
///
/// Split out as a pure decision so it can be tested without a network call:
/// exercising it through `with_credentials` would rotate the developer's own
/// refresh token as a side effect of `cargo test`.
///
/// Two conditions. The policy must allow it, which is the recursion guard. And
/// the chain must actually be dead — a refresh that failed on the network or a
/// 5xx says nothing about the stored token, so re-importing would discard a
/// working credential (and, past the DEK migration, spend the one bootstrap
/// exchange) over a transient blip.
fn should_reimport(policy: Recovery, e: &auth::Error) -> bool {
    policy == Recovery::RefreshThenReimport
        && matches!(
            e,
            auth::Error::RefreshRejected { .. } | auth::Error::NoCredentials
        )
}

// ---- Auth reporting ---------------------------------------------------------
//
// AIDEV-NOTE: this lives in api.rs rather than auth.rs because a useful report
// needs both halves — the local credential state (auth) and whether the API
// actually accepts it (here). auth.rs cannot depend on api.rs without a cycle.

/// Stable machine-readable outcome codes. Callers branch on these, so treat them
/// as API: add rather than rename.
pub mod codes {
    pub const OK: &str = "ok";
    pub const UNAUTHENTICATED: &str = "unauthenticated";
    pub const STALE_CREDENTIALS: &str = "stale_credentials";
    pub const NO_DESKTOP_CREDENTIALS: &str = "no_desktop_credentials";
    pub const NO_BOOTSTRAP_CREDENTIALS: &str = "no_bootstrap_credentials";
    pub const BOOTSTRAP_REFRESH_REJECTED: &str = "bootstrap_refresh_rejected";
    pub const NEEDS_KEYCHAIN_APPROVAL: &str = "needs_keychain_approval";
    pub const KEYCHAIN_UNAVAILABLE: &str = "keychain_unavailable";
    pub const API_UNREACHABLE: &str = "api_unreachable";
}

/// What would fix the reported state.
///
/// AIDEV-NOTE: an enum, not a prepared sentence, because the two front ends must
/// phrase the same hint differently — the CLI says "run `granola auth login`",
/// the MCP server says "call `granola_auth_login`". Emitting one phrasing for
/// both is precisely the bug this work exists to fix: an MCP agent relayed
/// "run it in a terminal" to the user for a state the server could have repaired
/// on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryHint {
    /// Working; nothing to do.
    None,
    /// Re-import from the Granola desktop app. Available to either front end.
    Reimport,
    /// The desktop app has no usable credentials — sign in to it first.
    SignInToDesktop,
    /// Granola's own Keychain item needs one-time approval, which only a human
    /// at the machine can give.
    ApproveKeychain,
    /// The OS keychain itself is not readable by this process.
    FixKeychainAccess,
    /// Granola's API could not be reached; retry later.
    Retry,
    /// No local credential source can be exchanged any more. Not recoverable by
    /// either front end.
    DeadEnd,
}

/// Credential state plus whether Granola accepts it.
#[derive(Debug, Clone, Serialize)]
pub struct AuthReport {
    pub ok: bool,
    pub code: &'static str,
    /// What happened. Deliberately carries no "run X" instruction — see
    /// `RecoveryHint`.
    pub message: String,
    pub credentials_present: bool,
    pub validated: bool,
    pub source: Option<auth::CredentialSource>,
    pub desktop: auth::DesktopState,
    pub recovery: RecoveryHint,
}

impl AuthReport {
    fn failure(
        code: &'static str,
        message: impl Into<String>,
        recovery: RecoveryHint,
        desktop: auth::DesktopState,
        credentials_present: bool,
    ) -> Self {
        Self {
            ok: false,
            code,
            message: message.into(),
            credentials_present,
            validated: false,
            source: None,
            desktop,
            recovery,
        }
    }
}

/// Validate whatever is in the keychain and describe the result.
///
/// Reports; never imports. Refreshing an expired access token is still in scope
/// (that is what validation means here), so this can rotate the stored refresh
/// token — hence no `read_only_hint` on the MCP tool that wraps it.
pub fn auth_report() -> AuthReport {
    let desktop = auth::desktop_state();

    match auth::get_credentials() {
        Ok(None) => {
            return AuthReport::failure(
                codes::UNAUTHENTICATED,
                "No Granola credentials are stored in the OS keychain.",
                reimport_hint(&desktop),
                desktop,
                false,
            )
        }
        Err(e) => {
            return AuthReport::failure(
                codes::KEYCHAIN_UNAVAILABLE,
                format!("The OS keychain could not be read: {e}"),
                RecoveryHint::FixKeychainAccess,
                desktop,
                false,
            )
        }
        Ok(Some(_)) => {}
    }

    match with_credentials(Recovery::RefreshOnly, |c| c.get_workspaces()) {
        Ok(_) => AuthReport {
            ok: true,
            code: codes::OK,
            message: "Stored credentials are valid.".into(),
            credentials_present: true,
            validated: true,
            source: Some(auth::CredentialSource::Keychain),
            desktop,
            recovery: RecoveryHint::None,
        },
        Err(e) if e.needs_reauth() => AuthReport::failure(
            codes::STALE_CREDENTIALS,
            format!("Stored credentials were rejected by Granola: {e}"),
            reimport_hint(&desktop),
            desktop,
            true,
        ),
        Err(e) => AuthReport::failure(
            codes::API_UNREACHABLE,
            format!("Could not reach Granola to validate credentials: {e}"),
            RecoveryHint::Retry,
            desktop,
            true,
        ),
    }
}

/// Re-import credentials from the Granola desktop app, validate them, and
/// describe the result. Seeds the process credential cache on success.
pub fn authenticate() -> AuthReport {
    let desktop = auth::desktop_state();

    let (creds, source) = match auth::login() {
        Ok(imported) => imported,
        Err(e) => return import_failure(e, desktop),
    };

    // AIDEV-NOTE: seed the cache *after* a successful import rather than
    // clearing it before. In a long-lived server, clearing up front leaves the
    // cache empty when the import fails — so the next call re-reads the keychain
    // and can raise another prompt — and unseeded when it succeeds.
    cache_client(Client::new(creds.access_token));

    // RefreshOnly: validating under RefreshThenReimport would recurse back into
    // this function.
    match with_credentials(Recovery::RefreshOnly, |c| c.get_workspaces()) {
        Ok(_) => AuthReport {
            ok: true,
            code: codes::OK,
            message: "Credentials imported from the Granola desktop app and validated.".into(),
            credentials_present: true,
            validated: true,
            source: Some(source),
            desktop,
            recovery: RecoveryHint::None,
        },
        Err(e) if e.needs_reauth() => AuthReport::failure(
            codes::STALE_CREDENTIALS,
            format!(
                "Credentials were imported but Granola rejected them: {e}. \
                 The Granola desktop app's own session is probably stale."
            ),
            RecoveryHint::SignInToDesktop,
            desktop,
            true,
        ),
        Err(e) => AuthReport::failure(
            codes::API_UNREACHABLE,
            format!("Credentials were imported but Granola could not be reached: {e}"),
            RecoveryHint::Retry,
            desktop,
            true,
        ),
    }
}

/// Report for an import that was abandoned because it blocked too long.
///
/// AIDEV-NOTE: in practice this means macOS is holding a Keychain dialog for
/// Granola's own key that nobody is going to answer — the one part of the import
/// that can block indefinitely. A slow refresh POST cannot reach here, since
/// `exchange_refresh_token` has its own shorter timeout. Constructed here rather
/// than in mcp.rs so every code/hint pairing stays in one place.
pub fn import_timed_out() -> AuthReport {
    AuthReport::failure(
        codes::NEEDS_KEYCHAIN_APPROVAL,
        "Importing credentials did not finish in time. macOS is most likely waiting on a \
         Keychain dialog to release Granola's encryption key, which needs a person at the \
         machine to approve it.",
        RecoveryHint::ApproveKeychain,
        auth::desktop_state(),
        auth::get_credentials().ok().flatten().is_some(),
    )
}

/// Map an import failure onto a code that distinguishes "sign in to Granola"
/// from the post-migration dead end.
fn import_failure(e: auth::Error, desktop: auth::DesktopState) -> AuthReport {
    // Past the DEK migration there is no plaintext token left to exchange, so
    // `load_legacy_refresh_credentials` reports the same NoDesktopCredentials as
    // a plain missing-file case. Only the desktop state separates them, and the
    // difference matters: one is fixed by signing in, the other cannot be fixed.
    let migrated = !desktop.encrypted_files.is_empty() && !desktop.storage_dek_present;

    match e {
        auth::Error::NoDesktopCredentials { ref tried } if migrated => AuthReport::failure(
            codes::NO_BOOTSTRAP_CREDENTIALS,
            format!(
                "Granola has moved its desktop encryption key into app-only Keychain storage \
                 and no leftover plaintext refresh token remains to bootstrap from. Looked in: {}.",
                display_paths(tried)
            ),
            RecoveryHint::DeadEnd,
            desktop,
            false,
        ),
        auth::Error::NoDesktopCredentials { ref tried } => AuthReport::failure(
            codes::NO_DESKTOP_CREDENTIALS,
            format!(
                "No Granola desktop credentials were found. Looked in: {}. \
                 Is the Granola desktop app installed and signed in?",
                display_paths(tried)
            ),
            RecoveryHint::SignInToDesktop,
            desktop,
            false,
        ),
        auth::Error::RefreshRejected { status } => AuthReport::failure(
            codes::BOOTSTRAP_REFRESH_REJECTED,
            format!(
                "Granola rejected the leftover desktop refresh token (HTTP {status}). \
                 This install can no longer bootstrap credentials from local desktop state."
            ),
            RecoveryHint::DeadEnd,
            desktop,
            false,
        ),
        auth::Error::Keyring(e) => AuthReport::failure(
            codes::KEYCHAIN_UNAVAILABLE,
            format!("The OS keychain could not be used: {e}"),
            RecoveryHint::FixKeychainAccess,
            desktop,
            false,
        ),
        auth::Error::Http(e) => AuthReport::failure(
            codes::API_UNREACHABLE,
            format!("Could not reach Granola to exchange the desktop refresh token: {e}"),
            RecoveryHint::Retry,
            desktop,
            false,
        ),
        other => AuthReport::failure(
            codes::NO_DESKTOP_CREDENTIALS,
            format!("Could not import Granola desktop credentials: {other}"),
            RecoveryHint::SignInToDesktop,
            desktop,
            false,
        ),
    }
}

/// Whether a re-import is worth suggesting, given what is on disk.
fn reimport_hint(desktop: &auth::DesktopState) -> RecoveryHint {
    if desktop.needs_cross_app_keychain {
        RecoveryHint::ApproveKeychain
    } else if desktop.plaintext_refresh_token_present || !desktop.plaintext_files.is_empty() {
        RecoveryHint::Reimport
    } else {
        RecoveryHint::SignInToDesktop
    }
}

fn display_paths(paths: &[std::path::PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
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
    use super::{
        cache_client, clear_cached_client, codes, import_failure, reimport_hint, should_reimport,
        with_token_refresh, Client, Error, Recovery, RecoveryHint,
    };
    use crate::auth;
    use std::path::PathBuf;

    fn desktop(
        plaintext: &[&str],
        encrypted: &[&str],
        dek: bool,
        refresh_token: bool,
    ) -> auth::DesktopState {
        auth::DesktopState {
            plaintext_files: plaintext.iter().map(|s| s.to_string()).collect(),
            encrypted_files: encrypted.iter().map(|s| s.to_string()).collect(),
            storage_dek_present: dek,
            plaintext_refresh_token_present: refresh_token,
            needs_cross_app_keychain: !encrypted.is_empty() && dek,
        }
    }

    /// The recursion guard. `authenticate` validates a fresh credential with an
    /// API call that can itself 401; if that path were allowed to re-import, it
    /// would import, validate, 401, and loop.
    ///
    /// AIDEV-NOTE: asserted on this pure decision rather than through
    /// `with_credentials`, because driving a real 401 through there would call
    /// `refresh_access_token` and rotate the developer's own refresh token as a
    /// side effect of `cargo test`.
    #[test]
    fn refresh_only_never_escalates_to_a_reimport() {
        let dead = auth::Error::RefreshRejected { status: 401 };
        assert!(!should_reimport(Recovery::RefreshOnly, &dead));
        assert!(should_reimport(Recovery::RefreshThenReimport, &dead));

        assert!(!should_reimport(
            Recovery::RefreshOnly,
            &auth::Error::NoCredentials
        ));
        assert!(should_reimport(
            Recovery::RefreshThenReimport,
            &auth::Error::NoCredentials
        ));
    }

    /// A transient refresh failure must not spend the re-import. Past the DEK
    /// migration that would burn the one bootstrap exchange over a network blip.
    #[test]
    fn a_transient_refresh_failure_does_not_trigger_a_reimport() {
        for e in [
            auth::Error::NoHomeDir,
            auth::Error::NoDesktopCredentials { tried: Vec::new() },
        ] {
            assert!(
                !should_reimport(Recovery::RefreshThenReimport, &e),
                "should not reimport for {e}"
            );
        }
    }

    #[test]
    fn only_a_dead_chain_counts_as_needing_reauth() {
        assert!(Error::Unauthenticated.needs_reauth());
        assert!(Error::NeedsKeychainApproval.needs_reauth());
        assert!(Error::Http {
            status: 401,
            message: String::new()
        }
        .needs_reauth());
        assert!(Error::Auth(auth::Error::RefreshRejected { status: 401 }).needs_reauth());

        // Not auth problems: these must not attract a "re-import" hint.
        assert!(!Error::Transport("dns".into()).needs_reauth());
        assert!(!Error::Http {
            status: 500,
            message: String::new()
        }
        .needs_reauth());
    }

    /// Regression: a cold start with an empty keychain and nothing to import
    /// used to reach the caller as a bare "could not locate Granola desktop
    /// credentials" with no recovery hint, because `needs_reauth` — which
    /// selects the `stale_credentials` code — correctly rejects it. Attaching a
    /// hint is a broader question and has its own predicate.
    #[test]
    fn a_failed_import_still_counts_as_an_auth_failure_worth_hinting() {
        let cold_start = Error::Auth(auth::Error::NoDesktopCredentials { tried: Vec::new() });
        assert!(
            !cold_start.needs_reauth(),
            "a failed import is not a dead stored chain"
        );
        assert!(
            cold_start.is_auth_failure(),
            "but the caller must still be told how to recover"
        );

        // Everything needs_reauth covers is also an auth failure.
        assert!(Error::Unauthenticated.is_auth_failure());
        assert!(Error::NeedsKeychainApproval.is_auth_failure());

        // Non-auth failures must not attract an auth hint either way.
        assert!(!Error::Transport("dns".into()).is_auth_failure());
        assert!(!Error::Http {
            status: 503,
            message: String::new()
        }
        .is_auth_failure());
    }

    /// The post-migration dead end and a plain "Granola isn't signed in" both
    /// surface as NoDesktopCredentials, and only the desktop state separates
    /// them. Reporting them identically is what made a permanent failure look
    /// like a transient 401.
    #[test]
    fn a_migrated_install_with_nothing_to_bootstrap_is_reported_as_a_dead_end() {
        let tried = vec![PathBuf::from("/tmp/stored-accounts.json")];
        let migrated = desktop(&[], &["stored-accounts.json.enc"], false, false);

        let report = import_failure(auth::Error::NoDesktopCredentials { tried }, migrated);
        assert_eq!(report.code, codes::NO_BOOTSTRAP_CREDENTIALS);
        assert_eq!(report.recovery, RecoveryHint::DeadEnd);
        assert!(!report.ok);
    }

    #[test]
    fn a_missing_desktop_install_is_reported_as_needing_sign_in() {
        let report = import_failure(
            auth::Error::NoDesktopCredentials { tried: Vec::new() },
            desktop(&[], &[], false, false),
        );
        assert_eq!(report.code, codes::NO_DESKTOP_CREDENTIALS);
        assert_eq!(report.recovery, RecoveryHint::SignInToDesktop);
    }

    #[test]
    fn a_rejected_bootstrap_token_is_reported_as_a_dead_end() {
        let report = import_failure(
            auth::Error::RefreshRejected { status: 400 },
            desktop(&["stored-accounts.json"], &[], false, true),
        );
        assert_eq!(report.code, codes::BOOTSTRAP_REFRESH_REJECTED);
        assert_eq!(report.recovery, RecoveryHint::DeadEnd);
    }

    #[test]
    fn recovery_hints_follow_what_is_actually_on_disk() {
        // A wrapped DEK means only a human can release the key.
        assert_eq!(
            reimport_hint(&desktop(&[], &["supabase.json.enc"], true, false)),
            RecoveryHint::ApproveKeychain
        );
        // Plaintext present: either front end can re-import unattended.
        assert_eq!(
            reimport_hint(&desktop(&["stored-accounts.json"], &[], false, true)),
            RecoveryHint::Reimport
        );
        // Nothing to import from.
        assert_eq!(
            reimport_hint(&desktop(&[], &[], false, false)),
            RecoveryHint::SignInToDesktop
        );
    }

    /// No failure report may carry a "run it in a terminal" style instruction in
    /// its message — the front ends phrase recovery from `RecoveryHint`, and
    /// baking one phrasing into the shared message is the original bug.
    #[test]
    fn report_messages_carry_no_front_end_specific_instruction() {
        let reports = [
            import_failure(
                auth::Error::NoDesktopCredentials { tried: Vec::new() },
                desktop(&[], &[], false, false),
            ),
            import_failure(
                auth::Error::RefreshRejected { status: 401 },
                desktop(&[], &[], false, false),
            ),
        ];
        for report in reports {
            let message = report.message.to_lowercase();
            for banned in ["granola auth login", "terminal", "browser"] {
                assert!(
                    !message.contains(banned),
                    "report {} leaked front-end phrasing {banned:?}: {}",
                    report.code,
                    report.message
                );
            }
        }
    }

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
