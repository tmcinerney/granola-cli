//! MCP server exposing Granola meeting data over stdio (`granola mcp`).
//!
//! AIDEV-NOTE: stdout is the JSON-RPC stream. Nothing in this module — or in
//! anything it calls — may print to stdout. Diagnostics go to stderr, which the
//! client shows as server logs. This is why the data functions live in
//! `meetings.rs` (print-free) rather than being shared with the CLI's printing
//! paths in `main.rs`.
//!
//! AIDEV-NOTE: tool names are deliberately identical to the retired
//! tmcinerney/granola-mcp Python server so existing prompts keep working. The
//! argument shape is flatter, though: FastMCP wrapped a Pydantic model in a
//! required `params` object, which rmcp's `Parameters<T>` does not.

use std::time::Duration;

use anyhow::Result;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::api;
use crate::meetings;

/// Default page size for MCP callers.
///
/// AIDEV-NOTE: deliberately higher than the CLI's DEFAULT_LIST_LIMIT of 20.
/// This is the one place the two front ends intentionally differ: a human
/// scanning a terminal table wants a short list, whereas an agent filtering
/// programmatically over compact summaries wants a useful window. `limit` is a
/// parameter on both sides, so either caller can override, and clamping lives
/// in the shared core. Keep any divergence to defaults — never to behaviour.
const DEFAULT_LIMIT: u32 = 50;

fn default_limit() -> u32 {
    DEFAULT_LIMIT
}

fn default_include_shared() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResponseFormat {
    /// Structured data, for programmatic use.
    #[default]
    Json,
    /// Human-readable summary.
    Markdown,
}

/// Absorbs MCP's reserved `_`-prefixed keys so they do not read as typos.
///
/// AIDEV-NOTE: replaces `#[serde(deny_unknown_fields)]`, which serde cannot
/// combine with a flattened catch-all. Rejection still happens during
/// deserialization — this type errors on any leftover key *not* starting with
/// `_` — so the guard cannot be forgotten at a call site.
///
/// Why underscore keys are exempt: MCP reserves `_meta` on requests, and a
/// leading underscore is a reserved-prefix convention, so such a key can never
/// collide with a real argument. A client that puts `_meta` inside `arguments`
/// (the spec puts it a level up) would otherwise get a rejection it cannot act
/// on — the same unactionable-failure class this validation exists to remove.
/// Values are discarded; nothing reserved is interpreted.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ReservedArgs;

impl<'de> Deserialize<'de> for ReservedArgs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Sink;

        impl<'de> serde::de::Visitor<'de> for Sink {
            type Value = ReservedArgs;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("no unrecognised arguments")
            }

            fn visit_map<A>(self, mut map: A) -> Result<ReservedArgs, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut unknown: Vec<String> = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    map.next_value::<serde::de::IgnoredAny>()?;
                    if !key.starts_with('_') {
                        unknown.push(key);
                    }
                }
                if !unknown.is_empty() {
                    unknown.sort();
                    let list = unknown
                        .iter()
                        .map(|k| format!("`{k}`"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(serde::de::Error::custom(format!(
                        "unknown field{}: {list}. Arguments are passed flat, not wrapped \
                         in a `params` object; see the tool's inputSchema.",
                        if unknown.len() == 1 { "" } else { "s" }
                    )));
                }
                Ok(ReservedArgs)
            }
        }

        deserializer.deserialize_map(Sink)
    }
}

// Contributes no properties of its own, so a flattened field leaves the parent
// schema's declared arguments untouched.
impl JsonSchema for ReservedArgs {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ReservedArgs".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "object" })
    }
}

// AIDEV-NOTE: rejecting unrecognised arguments is load-bearing for an
// agent-facing API, and restores parity with the retired Python server's
// `extra="forbid"`. Serde ignores unknown keys by default, so a typo'd or
// wrongly-nested argument object would deserialise to all-defaults — an
// unfiltered list of 50 full documents — instead of erroring. That reads to the
// caller as a working tool returning the wrong thing, far worse than a rejected
// call. Enforced by the flattened ReservedArgs below, which also lets MCP's
// reserved `_`-prefixed keys through.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(extend(
    "additionalProperties" = false,
    "patternProperties" = serde_json::json!({ "^_": {} })
))]
pub struct ListMeetingsArgs {
    /// Meetings on a single day. ISO `YYYY-MM-DD`. Shorthand for setting
    /// `since` to that day and `until` to the next.
    #[serde(default)]
    pub date: Option<String>,
    /// Filter meetings created at or after this point. ISO date `YYYY-MM-DD`,
    /// an RFC3339 timestamp, `today`, `yesterday`, or a relative span like
    /// `7d` / `2h`. Date-only bounds are interpreted as UTC.
    #[serde(default)]
    pub since: Option<String>,
    /// Filter meetings created before this point. Same accepted forms as
    /// `since`. The upper bound is exclusive.
    #[serde(default)]
    pub until: Option<String>,
    /// Alias for `since`, matching the CLI's explicit created-at flag.
    #[serde(default)]
    pub created_since: Option<String>,
    /// Alias for `until`, matching the CLI's explicit created-at flag.
    #[serde(default)]
    pub created_until: Option<String>,
    /// Filter on document `updated_at` rather than `created_at`.
    #[serde(default)]
    pub updated_since: Option<String>,
    /// Filter on document `updated_at` rather than `created_at`. Exclusive.
    #[serde(default)]
    pub updated_until: Option<String>,
    /// Case-insensitive substring match against the meeting title.
    #[serde(default)]
    pub search: Option<String>,
    /// Maximum number of meetings to return (1-200).
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Skip this many matching meetings before returning `limit`. Use with the
    /// `total_matched` in the response to page through a long result set.
    #[serde(default)]
    pub offset: u32,
    /// Output format.
    #[serde(default)]
    pub response_format: ResponseFormat,
    /// Include meetings shared with you as well as ones you own.
    #[serde(default = "default_include_shared")]
    pub include_shared: bool,
    // Never read: its Deserialize impl does the validating, and reserved values
    // are deliberately discarded. Present so serde routes leftover keys here.
    #[allow(dead_code)]
    #[serde(flatten)]
    pub reserved: ReservedArgs,
}

// AIDEV-NOTE: notes default to markdown rather than json because they are prose
// and the labelled headings are the readable form; the other tools default to
// json. Both remain available on either.
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(extend(
    "additionalProperties" = false,
    "patternProperties" = serde_json::json!({ "^_": {} })
))]
pub struct NotesArgs {
    /// Granola meeting ID from `granola_list_meetings`. A unique ID prefix also
    /// resolves.
    pub meeting_id: String,
    /// Output format.
    #[serde(default = "markdown_format")]
    pub response_format: ResponseFormat,
    // Never read: its Deserialize impl does the validating, and reserved values
    // are deliberately discarded. Present so serde routes leftover keys here.
    #[allow(dead_code)]
    #[serde(flatten)]
    pub reserved: ReservedArgs,
}

fn markdown_format() -> ResponseFormat {
    ResponseFormat::Markdown
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(extend(
    "additionalProperties" = false,
    "patternProperties" = serde_json::json!({ "^_": {} })
))]
pub struct MeetingIdFormatArgs {
    /// Granola meeting ID from `granola_list_meetings`. A unique ID prefix also
    /// resolves.
    pub meeting_id: String,
    /// Output format.
    #[serde(default)]
    pub response_format: ResponseFormat,
    // Never read: its Deserialize impl does the validating, and reserved values
    // are deliberately discarded. Present so serde routes leftover keys here.
    #[allow(dead_code)]
    #[serde(flatten)]
    pub reserved: ReservedArgs,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(extend(
    "additionalProperties" = false,
    "patternProperties" = serde_json::json!({ "^_": {} })
))]
pub struct AuthArgs {
    /// Output format.
    #[serde(default)]
    pub response_format: ResponseFormat,
    // Never read: its Deserialize impl does the validating, and reserved values
    // are deliberately discarded. Present so serde routes leftover keys here.
    #[allow(dead_code)]
    #[serde(flatten)]
    pub reserved: ReservedArgs,
}

/// How long `granola_auth_login` waits before giving up on the import.
///
/// AIDEV-NOTE: sits between `exchange_refresh_token`'s own 15s timeout and the
/// 60s default tool-call timeout in common MCP clients, so a slow network alone
/// cannot trip it and tripping it cannot look like a client-side hang.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct GranolaServer;

/// Run a blocking Granola API call off the async reactor.
///
/// AIDEV-NOTE: `api::Client` is synchronous (ureq), so calling it directly from
/// an async tool handler would block the runtime thread driving the stdio
/// transport. `spawn_blocking` keeps the transport responsive.
/// AIDEV-NOTE: the error is returned intact rather than pre-formatted so `reply`
/// can downcast it. An auth failure needs a recovery sentence appended, and
/// stringifying here would throw away the type that decides whether to add one.
async fn blocking<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => Err(anyhow::anyhow!("granola worker thread failed: {e}")),
    }
}

/// AIDEV-NOTE: tool failures are reported as `Ok(CallToolResult::error(..))`,
/// not `Err(ErrorData)`. MCP clients render protocol errors opaquely ("tool
/// result missing"), so an `Err` would hide the message — and the message is the
/// actionable part.
///
/// AIDEV-NOTE: re-auth IS attempted, both implicitly (see `api::Recovery`) and on
/// demand via `granola_auth_login`. This note previously said the opposite, on the
/// belief that credential import needs a terminal. It does not: import is local
/// file I/O plus, on a migrated install, one refresh-token POST. That mistaken
/// belief reached users as "run `granola auth login` in a terminal, it will open a
/// browser flow" — advice for a flow that does not exist.
fn reply(result: Result<String>) -> Result<CallToolResult, ErrorData> {
    Ok(match result {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(e) => {
            let mut message = format!("{e:#}");
            if e.downcast_ref::<api::Error>()
                .is_some_and(api::Error::is_auth_failure)
            {
                message.push_str("\n\n");
                message.push_str(AUTH_RECOVERY_HINT);
            }
            CallToolResult::error(vec![ContentBlock::text(message)])
        }
    })
}

/// Appended to auth failures on the data tools.
///
/// AIDEV-NOTE: deliberately not baked into `api::Error`'s Display, which the CLI
/// shares — the CLI must say "run `granola auth login`" and this must name a
/// tool. See the AIDEV-NOTE on `api::RecoveryHint`.
const AUTH_RECOVERY_HINT: &str =
    "Call `granola_auth_login` to re-import credentials from the Granola desktop app. \
     That is a local import, not a browser or device-code flow, so it works in this \
     session without a terminal. Call `granola_auth_status` first for the specific cause.";

fn meeting_markdown(m: &Value) -> String {
    let title = m
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .unwrap_or("(Untitled)");
    let id = m.get("id").and_then(Value::as_str).unwrap_or("");
    let created = m.get("created_at").and_then(Value::as_str).unwrap_or("");
    let start = m
        .pointer("/google_calendar_event/start/dateTime")
        .and_then(Value::as_str)
        .unwrap_or(created);

    let mut lines = vec![
        format!("### {title}"),
        format!("- **ID**: `{id}`"),
        format!("- **Start**: {start}"),
    ];
    if let Some(end) = m
        .pointer("/google_calendar_event/end/dateTime")
        .and_then(Value::as_str)
    {
        lines.push(format!("- **End**: {end}"));
    }
    let platform = m
        .pointer("/people/conferencing/type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    lines.push(format!("- **Platform**: {platform}"));

    let attendees: Vec<&str> = m
        .pointer("/people/attendees")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(attendee_label).collect())
        .unwrap_or_default();
    if !attendees.is_empty() {
        lines.push(format!("- **Attendees**: {}", attendees.join(", ")));
    }
    lines.join("\n")
}

/// Attendee display name, falling back to the email when Granola has no
/// resolved person record.
fn attendee_label(attendee: &Value) -> Option<&str> {
    attendee
        .pointer("/details/person/name/fullName")
        .and_then(Value::as_str)
        .or_else(|| attendee.get("name").and_then(Value::as_str))
        .or_else(|| attendee.get("email").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
}

/// Copy a transcript segment, adding a flattened speaker name and a bot flag.
fn annotate_segment(segment: &Value) -> Value {
    let mut out = segment.clone();
    let speaker = meetings::segment_speaker_name(segment);
    if let Some(obj) = out.as_object_mut() {
        obj.insert(
            "speaker".into(),
            speaker
                .map(|s| Value::String(s.into()))
                .unwrap_or(Value::Null),
        );
        obj.insert(
            "speaker_is_likely_bot".into(),
            Value::Bool(speaker.is_some_and(meetings::looks_like_notetaker_bot)),
        );
    }
    out
}

fn transcript_markdown(transcript: &Value) -> String {
    let mut lines = vec!["# Transcript".to_string(), String::new()];
    for segment in transcript.as_array().into_iter().flatten() {
        let text = segment
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            continue;
        }
        let source = segment
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let ts = segment
            .get("start_timestamp")
            .and_then(Value::as_str)
            .unwrap_or("");
        // AIDEV-NOTE: `source` is an audio channel, not a person — `system` can
        // carry several remote participants. Only a name Granola itself put on
        // the segment is shown as a speaker; never infer one from attendees.
        let speaker = match meetings::segment_speaker_name(segment) {
            Some(name) if meetings::looks_like_notetaker_bot(name) => {
                format!("**{name}** _(notetaker bot)_")
            }
            Some(name) => format!("**{name}**"),
            None => format!("**{source} audio**"),
        };
        lines.push(format!("{speaker} _(source: {source}; {ts})_: {text}"));
        lines.push(String::new());
    }
    lines.join("\n")
}

#[tool_router]
impl GranolaServer {
    #[tool(
        name = "granola_list_meetings",
        description = "List Granola meetings, newest first. Filter by date range or title \
                       substring. Returns meeting IDs to pass to the other granola_* tools."
    )]
    async fn list_meetings(
        &self,
        Parameters(args): Parameters<ListMeetingsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        reply(
            blocking(move || {
                let query = meetings::MeetingQuerySpec {
                    date: args.date.as_deref(),
                    since: args.since.as_deref(),
                    until: args.until.as_deref(),
                    created_since: args.created_since.as_deref(),
                    created_until: args.created_until.as_deref(),
                    updated_since: args.updated_since.as_deref(),
                    updated_until: args.updated_until.as_deref(),
                    search: args.search.as_deref(),
                    limit: args.limit,
                    offset: args.offset,
                    include_shared: args.include_shared,
                }
                .resolve()?;

                let page = api::with_token_refresh(|c| {
                    meetings::list_meetings(c, &query)
                        .map_err(|e| api::Error::Transport(e.to_string()))
                })?;

                Ok(match args.response_format {
                    ResponseFormat::Json => {
                        let summaries: Vec<Value> = page
                            .meetings
                            .iter()
                            .map(meetings::meeting_summary)
                            .collect();
                        serde_json::to_string_pretty(&serde_json::json!({
                            "total_matched": page.total_matched,
                            "offset": page.offset,
                            "count": summaries.len(),
                            "meetings": summaries,
                        }))?
                    }
                    ResponseFormat::Markdown => {
                        let shown = page.meetings.len();
                        let mut out = if page.total_matched > shown {
                            format!(
                                "# Meetings ({}-{} of {})\n",
                                page.offset as usize + 1,
                                page.offset as usize + shown,
                                page.total_matched
                            )
                        } else {
                            format!("# Meetings ({shown} found)\n")
                        };
                        for m in &page.meetings {
                            out.push('\n');
                            out.push_str(&meeting_markdown(m));
                            out.push('\n');
                        }
                        out
                    }
                })
            })
            .await,
        )
    }

    #[tool(
        name = "granola_get_notes",
        description = "Get the notes for one Granola meeting: both the notes the user typed \
                       during the call and Granola's AI-enhanced summary, labelled separately. \
                       They are different documents and either may be absent."
    )]
    async fn get_notes(
        &self,
        Parameters(args): Parameters<NotesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        reply(
            blocking(move || {
                let doc = api::with_token_refresh(|c| {
                    let id = meetings::resolve_meeting_id(c, &args.meeting_id)
                        .map_err(|e| api::Error::Transport(e.to_string()))?;
                    meetings::fetch_full_document(c, &id)
                })?;
                let title = doc.get("title").and_then(Value::as_str);
                let notes = meetings::meeting_notes(&doc);
                Ok(match args.response_format {
                    ResponseFormat::Json => serde_json::to_string_pretty(&notes.to_json())?,
                    ResponseFormat::Markdown => notes.render_markdown(title),
                })
            })
            .await,
        )
    }

    #[tool(
        name = "granola_get_transcript",
        description = "Get the full transcript for one Granola meeting. Speaker names appear \
                       only where Granola itself attributed a segment; `source` is an audio \
                       channel, not a person."
    )]
    async fn get_transcript(
        &self,
        Parameters(args): Parameters<MeetingIdFormatArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        reply(
            blocking(move || {
                let transcript = api::with_token_refresh(|c| {
                    let id = meetings::resolve_meeting_id(c, &args.meeting_id)
                        .map_err(|e| api::Error::Transport(e.to_string()))?;
                    c.get_document_transcript(&id)
                })?;
                Ok(match args.response_format {
                    // AIDEV-NOTE: raw segments are preserved and `speaker` /
                    // `speaker_is_likely_bot` added alongside, so a caller does
                    // not have to know to dig into detectedSpeaker, and nothing
                    // upstream is hidden.
                    ResponseFormat::Json => {
                        let annotated: Vec<Value> = transcript
                            .as_array()
                            .map(|segs| segs.iter().map(annotate_segment).collect())
                            .unwrap_or_default();
                        serde_json::to_string_pretty(&annotated)?
                    }
                    ResponseFormat::Markdown => transcript_markdown(&transcript),
                })
            })
            .await,
        )
    }

    #[tool(
        name = "granola_get_meeting_context",
        description = "Get a compact, conservative context object for one Granola meeting: \
                       title, calendar window, attendee names, and per-channel transcript \
                       attribution. Omits note content, emails, and URLs."
    )]
    async fn get_meeting_context(
        &self,
        Parameters(args): Parameters<MeetingIdFormatArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        reply(
            blocking(move || {
                let id = api::with_token_refresh(|c| {
                    meetings::resolve_meeting_id(c, &args.meeting_id)
                        .map_err(|e| api::Error::Transport(e.to_string()))
                })?;
                let doc = api::with_token_refresh(|c| meetings::fetch_full_document(c, &id))?;
                let transcript = api::with_token_refresh(|c| c.get_document_transcript(&id))?;
                let context = meetings::meeting_context_value(doc, transcript)?;
                Ok(match args.response_format {
                    ResponseFormat::Json => serde_json::to_string_pretty(&context)?,
                    ResponseFormat::Markdown => context_markdown(&context),
                })
            })
            .await,
        )
    }

    #[tool(
        name = "granola_auth_status",
        description = "Check whether Granola credentials are present and accepted by the API. \
                       Reports the specific cause of an auth failure and whether this server \
                       can repair it itself. Call this when another granola_* tool reports an \
                       authentication problem.",
        annotations(
            title = "Check Granola authentication",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn auth_status(
        &self,
        Parameters(args): Parameters<AuthArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        reply(blocking(move || render_auth_report(&api::auth_report(), args.response_format)).await)
    }

    // AIDEV-NOTE: `read_only_hint` is omitted-as-false on both auth tools, and the
    // four data tools stay unannotated. read_only_hint = true is what lets a client
    // auto-approve a call unattended, so it must not be claimed by anything that can
    // write credentials — and since the data tools refresh and may re-import (see
    // `api::Recovery`), that now includes them. `granola_auth_login` is not
    // idempotent either: each import can rotate the credential chain.
    #[tool(
        name = "granola_auth_login",
        description = "Re-import Granola credentials from the Granola desktop app and validate \
                       them. This is a local import — it reads the desktop app's own credential \
                       files and may exchange a refresh token — not a browser or device-code \
                       flow, so it needs no terminal and no user interaction. Call it when \
                       granola_auth_status reports stale or missing credentials.",
        annotations(
            title = "Re-import Granola credentials",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn auth_login(
        &self,
        Parameters(args): Parameters<AuthArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // AIDEV-NOTE: `spawn_blocking` cannot be cancelled, so on timeout the
        // import keeps running until the Keychain dialog is answered. That is
        // safe: it holds the refresh lock for its duration, so a later explicit
        // retry queues behind it rather than racing it, and if the dialog is
        // eventually approved the credentials it saves are simply already there.
        let report = match tokio::time::timeout(
            LOGIN_TIMEOUT,
            tokio::task::spawn_blocking(api::authenticate),
        )
        .await
        {
            Ok(Ok(report)) => report,
            Ok(Err(e)) => {
                return reply(Err(anyhow::anyhow!("granola worker thread failed: {e}")));
            }
            Err(_elapsed) => api::import_timed_out(),
        };
        reply(render_auth_report(&report, args.response_format))
    }
}

/// Render an `AuthReport` for an MCP caller, with the recovery step phrased as
/// something the agent can act on.
fn render_auth_report(report: &api::AuthReport, format: ResponseFormat) -> Result<String> {
    let next_step = mcp_recovery_text(report.recovery);
    Ok(match format {
        ResponseFormat::Json => {
            let mut value = serde_json::to_value(report)?;
            if let (Some(obj), Some(next)) = (value.as_object_mut(), next_step) {
                obj.insert("next_step".into(), Value::String(next.into()));
            }
            serde_json::to_string_pretty(&value)?
        }
        ResponseFormat::Markdown => {
            let mut out = format!(
                "# Granola authentication\n\n- **Status**: {}\n- **Code**: `{}`\n\n{}\n",
                if report.ok { "ok" } else { "not working" },
                report.code,
                report.message
            );
            if let Some(next) = next_step {
                out.push_str(&format!("\n**Next step**: {next}\n"));
            }
            out
        }
    })
}

/// The MCP server's phrasing of a `RecoveryHint` — names tools, not shell
/// commands, except where a human at the machine genuinely is required.
fn mcp_recovery_text(hint: api::RecoveryHint) -> Option<&'static str> {
    match hint {
        api::RecoveryHint::None => None,
        // AIDEV-NOTE: deliberately says nothing about terminals. Mentioning one
        // even to rule it out invites an agent to relay it as a step, which is
        // the failure this whole change exists to prevent — and a test asserts
        // the word is absent from any hint the server can act on itself.
        api::RecoveryHint::Reimport => Some(
            "Call `granola_auth_login`. It re-imports from the Granola desktop app in this \
             session: a local file import, not a browser or device-code flow.",
        ),
        api::RecoveryHint::SignInToDesktop => Some(
            "The Granola desktop app has no credentials to import. Ask the user to open Granola \
             and sign in, then call `granola_auth_login`.",
        ),
        // The one case that genuinely cannot be automated: macOS will hold a GUI
        // dialog for Granola's own Keychain item until a person answers it.
        api::RecoveryHint::ApproveKeychain => Some(
            "macOS needs one-time approval to release Granola's encryption key, which only the \
             user can give. Ask them to run `granola auth login` once in a terminal and approve \
             the Keychain prompt; after that this server can re-import on its own.",
        ),
        api::RecoveryHint::FixKeychainAccess => Some(
            "This server cannot read the OS keychain, so it cannot recover on its own. Ask the \
             user to run `granola auth status` in a terminal to see the underlying error.",
        ),
        api::RecoveryHint::Retry => {
            Some("Granola's API could not be reached. Retry the original tool call shortly.")
        }
        api::RecoveryHint::DeadEnd => Some(
            "No local credential source remains to import from, so neither this server nor the \
             CLI can recover. The Granola desktop app has to write fresh credentials first — ask \
             the user to sign out and back in to Granola desktop.",
        ),
    }
}

fn context_markdown(context: &Value) -> String {
    let title = context
        .pointer("/document/title")
        .and_then(Value::as_str)
        .unwrap_or("(untitled)");
    let mut out = format!("# {title}\n");
    if let Some(id) = context.pointer("/document/id").and_then(Value::as_str) {
        out.push_str(&format!("\n- **ID**: `{id}`\n"));
    }
    if let Some(start) = context
        .pointer("/calendar/start/date_time")
        .and_then(Value::as_str)
    {
        out.push_str(&format!("- **Start**: {start}\n"));
    }
    if let Some(names) = context
        .pointer("/people/attendee_names")
        .and_then(Value::as_array)
    {
        let names: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
        if !names.is_empty() {
            out.push_str(&format!("- **Attendees**: {}\n", names.join(", ")));
        }
    }
    if let Some(count) = context
        .pointer("/transcript/segment_count")
        .and_then(Value::as_u64)
    {
        out.push_str(&format!("- **Transcript segments**: {count}\n"));
    }

    out.push_str("\n## Transcript channels\n\n");
    for channel in context
        .pointer("/attribution/channels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let source = channel
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let count = channel
            .get("segment_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let names: Vec<&str> = channel
            .get("detected_speaker_names")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if names.is_empty() {
            out.push_str(&format!("- {source}: {count} segments\n"));
        } else {
            out.push_str(&format!(
                "- {source}: {count} segments; detected speakers: {}\n",
                names.join(", ")
            ));
        }
    }
    out
}

#[tool_handler(
    name = "granola",
    instructions = "Access Granola meeting notes, AI-enhanced summaries, and transcripts. \
                    Call granola_list_meetings first to find a meeting and its ID, then \
                    granola_get_notes for the enhanced notes, granola_get_transcript for the \
                    full transcript, or granola_get_meeting_context for a compact summary with \
                    conservative speaker attribution. Transcript `source` names an audio \
                    channel rather than a person, so do not treat it as a speaker identity. \
                    Authentication repairs itself: a rejected credential is re-imported from the \
                    Granola desktop app automatically. If a tool still reports an auth failure, \
                    call granola_auth_status for the cause and granola_auth_login to re-import. \
                    Do not tell the user to run `granola auth login` in a terminal unless a \
                    tool's own next_step says so — credential import is local file I/O, not a \
                    browser flow, and this server can do it itself."
)]
impl ServerHandler for GranolaServer {}

/// Serve MCP over stdio until the client disconnects.
pub fn run() -> Result<()> {
    // AIDEV-NOTE: the runtime is built here rather than via #[tokio::main] so
    // the CLI subcommands stay entirely synchronous and pay no runtime cost.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let service = GranolaServer.serve(rmcp::transport::stdio()).await?;
            service.waiting().await?;
            Ok(())
        })
}

#[cfg(test)]
mod tests {
    use super::{
        mcp_recovery_text, render_auth_report, AuthArgs, ListMeetingsArgs, NotesArgs,
        ResponseFormat,
    };
    use crate::api;

    fn report(code: &'static str, recovery: api::RecoveryHint) -> api::AuthReport {
        api::AuthReport {
            ok: false,
            code,
            message: "Stored credentials were rejected by Granola.".into(),
            credentials_present: true,
            validated: false,
            source: None,
            desktop: crate::auth::DesktopState {
                plaintext_files: vec!["stored-accounts.json".into()],
                encrypted_files: Vec::new(),
                storage_dek_present: false,
                plaintext_refresh_token_present: true,
                needs_cross_app_keychain: false,
            },
            recovery,
        }
    }

    /// The auth tools take the same validated-argument shape as the data tools.
    #[test]
    fn auth_args_accept_the_documented_shape_and_reject_typos() {
        let args: AuthArgs = serde_json::from_str(r#"{"response_format":"markdown"}"#)
            .expect("the documented flat shape must deserialise");
        assert!(matches!(args.response_format, ResponseFormat::Markdown));

        // Everything optional, and json is the default.
        let defaults: AuthArgs = serde_json::from_str("{}").expect("all fields optional");
        assert!(matches!(defaults.response_format, ResponseFormat::Json));

        // Same guards as every other tool: reserved keys through, typos out.
        assert!(serde_json::from_str::<AuthArgs>(r#"{"_meta":{}}"#).is_ok());
        assert!(serde_json::from_str::<AuthArgs>(r#"{"responseFormat":"json"}"#).is_err());
        assert!(serde_json::from_str::<AuthArgs>(r#"{"params":{}}"#).is_err());
    }

    /// A state the server can repair must not be reported to the agent as
    /// something the user has to do in a terminal. That mistake is the reason
    /// these tools exist, so it is asserted rather than left to review.
    ///
    /// The check is for the bare word, not the instruction form: an agent
    /// relaying "no terminal needed" to a user is nearly as unhelpful as telling
    /// them to open one, so a self-repairable hint should not raise the subject.
    #[test]
    fn a_self_repairable_state_never_tells_the_agent_to_use_a_terminal() {
        let text = mcp_recovery_text(api::RecoveryHint::Reimport).expect("a hint");
        assert!(text.contains("granola_auth_login"), "{text}");
        for banned in ["terminal", "browser flow", "sign in", "log in"] {
            assert!(!text.to_lowercase().contains(banned), "{banned:?}: {text}");
        }
    }

    /// The converse: the two states that genuinely need a person at the machine
    /// must say so, rather than sending the agent round a loop it cannot exit.
    #[test]
    fn states_needing_a_human_say_so() {
        for hint in [
            api::RecoveryHint::ApproveKeychain,
            api::RecoveryHint::FixKeychainAccess,
        ] {
            let text = mcp_recovery_text(hint).expect("a hint");
            assert!(text.contains("terminal"), "{hint:?} should defer: {text}");
        }
    }

    #[test]
    fn a_working_state_suggests_nothing() {
        assert!(mcp_recovery_text(api::RecoveryHint::None).is_none());
    }

    #[test]
    fn rendered_reports_carry_the_code_and_the_next_step() {
        let report = report(api::codes::STALE_CREDENTIALS, api::RecoveryHint::Reimport);

        let json = render_auth_report(&report, ResponseFormat::Json).expect("render json");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(parsed["code"], "stale_credentials");
        assert_eq!(parsed["recovery"], "reimport");
        assert!(parsed["next_step"]
            .as_str()
            .expect("next_step")
            .contains("granola_auth_login"));

        let markdown = render_auth_report(&report, ResponseFormat::Markdown).expect("render md");
        assert!(markdown.contains("stale_credentials"), "{markdown}");
        assert!(markdown.contains("Next step"), "{markdown}");
    }

    #[test]
    fn a_healthy_report_renders_without_a_next_step() {
        let mut healthy = report(api::codes::OK, api::RecoveryHint::None);
        healthy.ok = true;
        let json = render_auth_report(&healthy, ResponseFormat::Json).expect("render json");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert!(parsed.get("next_step").is_none(), "{json}");
    }

    /// MCP reserves `_meta`, and a leading underscore cannot collide with a real
    /// argument — so reserved keys pass through and are ignored, while anything
    /// that could be a mistyped parameter is still rejected.
    #[test]
    fn reserved_underscore_keys_are_accepted_and_ignored() {
        let args: ListMeetingsArgs =
            serde_json::from_str(r#"{"limit":2,"_meta":{"progressToken":1}}"#)
                .expect("_meta must not be treated as a typo");
        assert_eq!(args.limit, 2);

        // Any underscore-prefixed key, not just _meta — the prefix is the rule.
        assert!(serde_json::from_str::<ListMeetingsArgs>(r#"{"_futureThing":"x"}"#).is_ok());
        // And on the other argument types too.
        assert!(serde_json::from_str::<NotesArgs>(r#"{"meeting_id":"a","_meta":{}}"#).is_ok());
    }

    /// The exemption is a *prefix* rule, so a trailing or interior underscore is
    /// still a typo and must not slip through.
    #[test]
    fn only_a_leading_underscore_is_exempt() {
        for bad in [r#"{"limit_":2}"#, r#"{"my_limit":2}"#, r#"{"limitt":2}"#] {
            assert!(
                serde_json::from_str::<ListMeetingsArgs>(bad).is_err(),
                "should reject: {bad}"
            );
        }
    }

    /// A reserved key must not disable validation for the rest of the object.
    #[test]
    fn reserved_keys_do_not_weaken_the_rest_of_the_check() {
        let err = serde_json::from_str::<ListMeetingsArgs>(r#"{"_meta":{},"params":{"limit":2}}"#)
            .expect_err("`params` is still unknown even alongside _meta");
        assert!(err.to_string().contains("params"), "unexpected: {err}");
    }

    /// Regression: unknown arguments must be rejected, not silently ignored.
    ///
    /// Without `deny_unknown_fields`, a wrongly-nested `{"params": {...}}`
    /// deserialised to all-defaults and the tool returned an unfiltered list of
    /// 50 full documents as a *success*, which reads as a working tool giving
    /// the wrong answer.
    #[test]
    fn list_args_reject_wrongly_nested_params() {
        let err = serde_json::from_str::<ListMeetingsArgs>(r#"{"params":{"limit":2}}"#)
            .expect_err("a nested params object must not deserialise to defaults");
        assert!(
            err.to_string().contains("unknown field"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn list_args_reject_misspelled_field() {
        assert!(serde_json::from_str::<ListMeetingsArgs>(r#"{"limitt":2}"#).is_err());
        assert!(
            serde_json::from_str::<ListMeetingsArgs>(r#"{"limit":2,"nonsense":true}"#).is_err()
        );
    }

    #[test]
    fn id_args_reject_unknown_fields() {
        assert!(serde_json::from_str::<NotesArgs>(r#"{"meeting_id":"abc"}"#).is_ok());
        assert!(serde_json::from_str::<NotesArgs>(r#"{"meetingId":"abc"}"#).is_err());
    }

    /// Notes are prose, so this one tool defaults to markdown while the others
    /// default to json. Assert that deliberately, so it is not "fixed" later.
    #[test]
    fn notes_default_to_markdown() {
        let args: NotesArgs = serde_json::from_str(r#"{"meeting_id":"abc"}"#).unwrap();
        assert!(matches!(args.response_format, ResponseFormat::Markdown));
    }

    #[test]
    fn list_args_accept_documented_shape_and_defaults() {
        let args: ListMeetingsArgs =
            serde_json::from_str(r#"{"since":"7d","limit":2,"response_format":"markdown"}"#)
                .expect("the documented flat shape must deserialise");
        assert_eq!(args.limit, 2);
        assert!(matches!(args.response_format, ResponseFormat::Markdown));

        // Everything is optional; an empty object is a valid "recent meetings" call.
        let defaults: ListMeetingsArgs = serde_json::from_str("{}").expect("all fields optional");
        assert_eq!(defaults.limit, super::DEFAULT_LIMIT);
        assert!(matches!(defaults.response_format, ResponseFormat::Json));
        assert!(defaults.since.is_none());
    }
}
