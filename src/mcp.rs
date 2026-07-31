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
    use super::{ListMeetingsArgs, NotesArgs, ResponseFormat};

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
