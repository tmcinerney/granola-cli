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

use anyhow::Result;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::api;
use crate::meetings;
use crate::output;

/// Default page size, matching the retired Python server rather than the CLI's
/// smaller interactive default.
const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;

fn default_limit() -> u32 {
    DEFAULT_LIMIT
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

// AIDEV-NOTE: deny_unknown_fields is load-bearing for an agent-facing API, and
// restores parity with the retired Python server's `extra="forbid"`. Without it
// serde silently ignores unrecognised keys, so a typo'd or wrongly-nested
// argument object deserialises to all-defaults — an unfiltered list of 50 full
// documents — instead of erroring. That reads to the caller as a working tool
// returning the wrong thing, which is far worse than a rejected call.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// Output format.
    #[serde(default)]
    pub response_format: ResponseFormat,
    /// Exclude meetings shared with you, returning only ones you own.
    #[serde(default)]
    pub owned_only: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MeetingIdArgs {
    /// Granola meeting ID from `granola_list_meetings`. A unique ID prefix also
    /// resolves.
    pub meeting_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MeetingIdFormatArgs {
    /// Granola meeting ID from `granola_list_meetings`. A unique ID prefix also
    /// resolves.
    pub meeting_id: String,
    /// Output format.
    #[serde(default)]
    pub response_format: ResponseFormat,
}

#[derive(Clone)]
pub struct GranolaServer;

/// Run a blocking Granola API call off the async reactor.
///
/// AIDEV-NOTE: `api::Client` is synchronous (ureq), so calling it directly from
/// an async tool handler would block the runtime thread driving the stdio
/// transport. `spawn_blocking` keeps the transport responsive.
async fn blocking<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(e) => Err(format!("granola worker thread failed: {e}")),
    }
}

/// AIDEV-NOTE: tool failures are reported as `Ok(CallToolResult::error(..))`,
/// not `Err(ErrorData)`. MCP clients render protocol errors opaquely ("tool
/// result missing"), so an `Err` would hide the message — including
/// `api::Error::Unauthenticated`, whose text tells the user to run
/// `granola auth login`. Interactive re-auth is deliberately never attempted:
/// the server is spawned by a GUI client with no terminal to prompt on.
fn reply(result: Result<String, String>) -> Result<CallToolResult, ErrorData> {
    Ok(match result {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
    })
}

fn parse_bound(spec: Option<&str>) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    match spec {
        None => Ok(None),
        Some(s) => output::parse_date_spec(s)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("{e}")),
    }
}

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
                let limit = args.limit.clamp(1, MAX_LIMIT);

                // `date` is sugar for a single-day window; explicit bounds win.
                let (date_since, date_until) = match args.date.as_deref() {
                    Some(day) => {
                        let start = output::parse_date_spec(day)
                            .map_err(|e| anyhow::anyhow!("invalid `date`: {e}"))?;
                        (Some(start), Some(start + chrono::Duration::days(1)))
                    }
                    None => (None, None),
                };

                let created_since =
                    parse_bound(args.created_since.as_deref().or(args.since.as_deref()))?
                        .or(date_since);
                let created_until =
                    parse_bound(args.created_until.as_deref().or(args.until.as_deref()))?
                        .or(date_until);
                let updated_since = parse_bound(args.updated_since.as_deref())?;
                let updated_until = parse_bound(args.updated_until.as_deref())?;
                let search = args.search.as_deref().map(str::to_lowercase);

                let meetings = api::with_token_refresh(|c| {
                    meetings::fetch_meetings_merged(c, !args.owned_only)
                        .map_err(|e| api::Error::Transport(e.to_string()))
                })?;

                let filtered: Vec<Value> = meetings
                    .into_iter()
                    .filter(|m| meetings::in_date_range(m, created_since, created_until))
                    .filter(|m| {
                        meetings::in_timestamp_range(m, "updated_at", updated_since, updated_until)
                    })
                    .filter(|m| match &search {
                        Some(q) => m
                            .get("title")
                            .and_then(Value::as_str)
                            .map(|t| t.to_lowercase().contains(q))
                            .unwrap_or(false),
                        None => true,
                    })
                    .take(limit as usize)
                    .collect();

                Ok(match args.response_format {
                    ResponseFormat::Json => serde_json::to_string_pretty(&filtered)?,
                    ResponseFormat::Markdown => {
                        let mut out = format!("# Meetings ({} found)\n", filtered.len());
                        for m in &filtered {
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
        description = "Get the AI-enhanced notes for one Granola meeting, as markdown."
    )]
    async fn get_notes(
        &self,
        Parameters(args): Parameters<MeetingIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        reply(
            blocking(move || {
                let doc = api::with_token_refresh(|c| {
                    let id = meetings::resolve_meeting_id(c, &args.meeting_id)
                        .map_err(|e| api::Error::Transport(e.to_string()))?;
                    meetings::fetch_full_document(c, &id)
                })?;
                let notes = meetings::notes_markdown(&doc);
                Ok(if notes.trim().is_empty() {
                    "This meeting has no notes.".to_string()
                } else {
                    notes
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
                    ResponseFormat::Json => serde_json::to_string_pretty(&transcript)?,
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
                    channel rather than a person, so do not treat it as a speaker identity."
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
    use super::{ListMeetingsArgs, MeetingIdArgs, ResponseFormat};

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
        assert!(serde_json::from_str::<MeetingIdArgs>(r#"{"meeting_id":"abc"}"#).is_ok());
        assert!(serde_json::from_str::<MeetingIdArgs>(r#"{"meetingId":"abc"}"#).is_err());
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
