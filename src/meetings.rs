//! Meeting data access and shaping, independent of any output surface.
//!
//! AIDEV-NOTE: nothing in this module may write to stdout. It is shared by the
//! CLI (which prints) and the MCP server (whose stdout *is* the JSON-RPC
//! stream, so a stray `println!` corrupts framing and kills the session with no
//! useful error). Keep formatting that returns `String` here if you like, but
//! leave the printing to `main.rs` / `output.rs`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::api;

/// Owned + shared documents, deduped, filtered by date range and search,
/// sorted by `updated_at` desc.
fn fetch_meetings_merged(client: &api::Client, include_shared: bool) -> Result<Vec<Value>> {
    // Owned documents: page through /v2/get-documents until we run out.
    let mut by_id: HashMap<String, Value> = HashMap::new();
    let page_size: u32 = 100;
    let mut offset: u32 = 0;
    loop {
        let resp = client.get_documents(page_size, offset, false)?;
        let docs = resp
            .get("docs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let n = docs.len();
        for mut d in docs {
            if let Some(id) = d.get("id").and_then(Value::as_str).map(str::to_owned) {
                d.as_object_mut()
                    .unwrap()
                    .insert("_origin".into(), Value::String("owned".into()));
                by_id.insert(id, d);
            }
        }
        if (n as u32) < page_size {
            break;
        }
        offset += page_size;
        // safety: bound at 1000 docs for now
        if offset > 1000 {
            break;
        }
    }

    if include_shared {
        // Document lists (folders) — enumerate to find shared doc IDs.
        let lists = client
            .get_document_lists()
            .map(|v| v.as_array().cloned().unwrap_or_default())
            .unwrap_or_default();

        let mut shared_ids: HashSet<String> = HashSet::new();
        for list in &lists {
            // v2 returns full documents array; v1 returns document_ids
            if let Some(arr) = list.get("documents").and_then(Value::as_array) {
                for d in arr {
                    if let Some(id) = d.get("id").and_then(Value::as_str) {
                        if !by_id.contains_key(id) {
                            shared_ids.insert(id.to_string());
                        }
                    }
                }
            }
            if let Some(arr) = list.get("document_ids").and_then(Value::as_array) {
                for id in arr.iter().filter_map(Value::as_str) {
                    if !by_id.contains_key(id) {
                        shared_ids.insert(id.to_string());
                    }
                }
            }
        }

        if !shared_ids.is_empty() {
            let ids: Vec<String> = shared_ids.into_iter().collect();
            // Batch in chunks of 100 (spec limit)
            for chunk in ids.chunks(100) {
                let resp = client.get_documents_batch(chunk, false)?;
                let docs = resp
                    .get("documents")
                    .or_else(|| resp.get("docs"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for mut d in docs {
                    if let Some(id) = d.get("id").and_then(Value::as_str).map(str::to_owned) {
                        d.as_object_mut()
                            .unwrap()
                            .insert("_origin".into(), Value::String("shared".into()));
                        by_id.insert(id, d);
                    }
                }
            }
        }
    }

    let mut all: Vec<Value> = by_id.into_values().collect();
    all.sort_by(|a, b| {
        let av = a.get("updated_at").and_then(Value::as_str).unwrap_or("");
        let bv = b.get("updated_at").and_then(Value::as_str).unwrap_or("");
        bv.cmp(av)
    });
    Ok(all)
}

fn in_date_range(m: &Value, since: Option<DateTime<Utc>>, until: Option<DateTime<Utc>>) -> bool {
    in_timestamp_range(m, "created_at", since, until)
}

fn in_timestamp_range(
    m: &Value,
    field: &str,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> bool {
    let timestamp = m
        .get(field)
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));
    let Some(timestamp) = timestamp else {
        return since.is_none() && until.is_none();
    };
    if let Some(s) = since {
        if timestamp < s {
            return false;
        }
    }
    if let Some(u) = until {
        if timestamp >= u {
            return false;
        }
    }
    true
}

fn looks_like_full_meeting_id(id: &str) -> bool {
    id.len() == 36 && id.chars().filter(|c| *c == '-').count() == 4
}

pub(crate) fn resolve_meeting_id_from_documents(
    raw_id: &str,
    meetings: &[Value],
) -> Result<String> {
    let trimmed = raw_id.trim();
    if trimmed.is_empty() {
        anyhow::bail!("meeting ID cannot be empty");
    }
    if looks_like_full_meeting_id(trimmed) {
        return Ok(trimmed.to_string());
    }

    let matches: Vec<String> = meetings
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str))
        .filter(|id| id.starts_with(trimmed))
        .map(str::to_string)
        .collect();

    match matches.as_slice() {
        [only] => Ok(only.clone()),
        [] => anyhow::bail!(
            "meeting ID `{trimmed}` did not match any recent meeting. Use the full UUID from \
             `granola meeting list --output json` or a unique prefix from `granola meeting list`."
        ),
        _ => anyhow::bail!(
            "meeting ID `{trimmed}` matched multiple recent meetings. Use the full UUID from \
             `granola meeting list --output json`."
        ),
    }
}

// AIDEV-NOTE: The table view intentionally shortens IDs to 8 characters for
// readability. Content commands must resolve that prefix back to the full UUID
// before calling Granola's document endpoints, or the API returns HTTP 400.
pub(crate) fn resolve_meeting_id(client: &api::Client, raw_id: &str) -> Result<String> {
    let meetings = fetch_meetings_merged(client, true)?;
    resolve_meeting_id_from_documents(raw_id, &meetings)
}

/// Fetch the full document via `/v1/get-documents-batch` with
/// `include_last_viewed_panel: true`. This is the most reliable single-doc
/// fetch path — `get-document-metadata` returns a sparse view on many
/// accounts and doesn't include notes content.
pub(crate) fn fetch_full_document(client: &api::Client, id: &str) -> Result<Value, api::Error> {
    let resp = client.get_documents_batch(&[id.to_string()], true)?;
    let docs = resp
        .get("documents")
        .or_else(|| resp.get("docs"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(docs.into_iter().next().unwrap_or(Value::Null))
}

/// Compact per-meeting summary for list responses.
///
/// AIDEV-NOTE: a raw Granola document carries ~47 fields. The largest is
/// `ydoc_state` (a CRDT blob, meaningless to a caller), then `people` (which
/// includes attendee emails) and `notes`/`notes_markdown`. Returning those from
/// a *list* call cost ~6.5k characters per meeting — so a default limit of 50
/// produced ~600k of mostly-noise, and leaked note content and emails for
/// meetings the caller never asked to read. Detail belongs in the per-meeting
/// tools instead. Keep this projection additive: widen it only with fields that
/// help a caller *choose* a meeting.
pub(crate) fn meeting_summary(m: &Value) -> Value {
    let attendees: Vec<&str> = m
        .pointer("/people/attendees")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(person_display_name).collect())
        .unwrap_or_default();

    serde_json::json!({
        "id": m.get("id"),
        "title": m.get("title"),
        "created_at": m.get("created_at"),
        "updated_at": m.get("updated_at"),
        "start": m.pointer("/google_calendar_event/start/dateTime"),
        "end": m.pointer("/google_calendar_event/end/dateTime"),
        "platform": m.pointer("/people/conferencing/type"),
        "attendee_names": attendees,
        "origin": m.get("_origin"),
        // Cheap here: the list payload carries the flat note fields, so this
        // answers "which meetings did I take notes in" without a second fetch.
        "has_own_notes": has_own_notes(m),
    })
}

/// Render optional speaker identity without replacing Granola's raw
/// audio-channel label. This is intentionally opt-in because the raw channel
/// remains the clearest default when no diarization is present.
pub(crate) fn format_transcript_segment(seg: &Value) -> String {
    let source = seg.get("source").and_then(Value::as_str).unwrap_or("");
    let text = seg.get("text").and_then(Value::as_str).unwrap_or("");
    let ts = seg
        .get("start_timestamp")
        .and_then(Value::as_str)
        .unwrap_or("");
    match segment_speaker_name(seg) {
        Some(speaker) => format!("[{ts}] ({source}; speaker: {speaker}) {text}"),
        None => format!("[{ts}] ({source}) {text}"),
    }
}

/// Return only speaker identity supplied by Granola's transcript payload.
/// In particular, this must not infer a remote name from calendar attendees:
/// a `system` audio channel can contain multiple remote participants.
pub(crate) fn segment_speaker_name(seg: &Value) -> Option<&str> {
    seg.pointer("/detectedSpeaker/participantName")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .or_else(|| {
            seg.get("detected_speaker_name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
        })
}

pub(crate) fn attribution_summary(transcript: &Value) -> Value {
    let mut channels: BTreeMap<String, (u64, BTreeSet<String>)> = BTreeMap::new();
    for segment in transcript.as_array().into_iter().flatten() {
        let source = segment
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let entry = channels.entry(source).or_default();
        entry.0 += 1;
        if let Some(name) = segment_speaker_name(segment) {
            entry.1.insert(name.to_string());
        }
    }

    let channels: Vec<Value> = channels
        .into_iter()
        .map(|(source, (segment_count, detected_speaker_names))| {
            serde_json::json!({
                "source": source,
                "segment_count": segment_count,
                "detected_speaker_names": detected_speaker_names.into_iter().collect::<Vec<_>>(),
            })
        })
        .collect();
    // AIDEV-NOTE: bots are flagged, never removed from detected_speaker_names —
    // the detection is a heuristic and a false positive must not erase a real
    // participant. Callers subtract this set if they want humans only.
    let likely_bots: Vec<Value> = channels
        .iter()
        .filter_map(|c| c.get("detected_speaker_names").and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .filter(|n| looks_like_notetaker_bot(n))
        .map(|n| Value::String(n.to_string()))
        .collect();

    serde_json::json!({
        "channels": channels,
        "likely_notetaker_bots": likely_bots,
        "speaker_attribution": "Only names present in raw transcript segments are listed; no calendar-based inference is applied. Names matching known notetaker vendors are additionally listed in likely_notetaker_bots.",
    })
}

/// A compact, stable and conservative attribution summary. Complete raw data
/// remains available through `meeting view --output json` and `meeting
/// transcript --output json`; context deliberately omits emails, URLs, note
/// content, and arbitrary API fields.
fn person_display_name(person: &Value) -> Option<&str> {
    person
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| person.pointer("/name/fullName").and_then(Value::as_str))
        .or_else(|| {
            person
                .pointer("/details/person/name/fullName")
                .and_then(Value::as_str)
        })
        .filter(|name| !name.is_empty())
}

pub(crate) fn meeting_context_value(document: Value, transcript: Value) -> Result<Value> {
    let segments = transcript.as_array().ok_or_else(|| {
        anyhow::anyhow!("Granola returned a transcript payload that is not a segment array")
    })?;
    let notes = meeting_notes(&document);
    let attribution = attribution_summary(&transcript);
    let attendees = document
        .pointer("/people/attendees")
        .and_then(Value::as_array)
        .map(|attendees| {
            attendees
                .iter()
                .filter_map(person_display_name)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(serde_json::json!({
        "schema_version": 1,
        "provenance": {
            "document": "Granola meeting document API response",
            "notes": "Editable note document stored with the meeting; it has no transcript-segment or speaker provenance.",
            "transcript": "Granola raw transcript API response",
            "speaker_attribution": "Only names supplied in raw transcript segments are summarized; calendar participants are never inferred as speakers."
        },
        "document": {
            "id": document.get("id"),
            "title": document.get("title"),
            "type": document.get("type"),
            "created_at": document.get("created_at"),
            "updated_at": document.get("updated_at"),
            "creation_source": document.get("creation_source"),
        },
        "people": {
            "creator_name": document.pointer("/people/creator").and_then(person_display_name),
            "attendee_names": attendees,
        },
        "conferencing": {
            "type": document.pointer("/people/conferencing/type"),
        },
        "calendar": {
            "start": {
                "date_time": document.pointer("/google_calendar_event/start/dateTime"),
                "date": document.pointer("/google_calendar_event/start/date"),
                "time_zone": document.pointer("/google_calendar_event/start/timeZone"),
            },
            "end": {
                "date_time": document.pointer("/google_calendar_event/end/dateTime"),
                "date": document.pointer("/google_calendar_event/end/date"),
                "time_zone": document.pointer("/google_calendar_event/end/timeZone"),
            },
        },
        "notes": {
            "available": !notes.is_empty(),
            "my_notes": notes.mine.is_some(),
            "ai_notes": notes.ai.is_some(),
        },
        "transcript": {
            "segment_count": segments.len(),
        },
        "attribution": attribution,
    }))
}

// ---- Shared query core ------------------------------------------------------
//
// AIDEV-NOTE: the CLI and the MCP server must behave identically for the same
// query, so the *operation* lives here and each front end is only an adapter
// that translates its own input syntax and formats the output. Before this,
// main.rs and mcp.rs each parsed bounds, applied filters and chose defaults
// separately, and they had already drifted seven ways (--no-shared vs
// owned_only, MCP-only date/offset, different default limits). If you add a
// parameter, add it to MEETING_LIST_PARAMETERS and to both front ends — the
// drift test in main.rs fails otherwise.

/// Canonical parameter set for a meeting-list query.
///
/// Both front ends are asserted against this: clap argument ids on one side,
/// the generated JSON schema properties on the other. Output formatting is
/// deliberately excluded — the CLI offers yaml/table/text that make no sense
/// over MCP, and MCP's `response_format` is not a filter.
// Referenced by the drift test rather than at runtime, and load-bearing as
// documentation of the contract between the two front ends.
#[allow(dead_code)]
pub(crate) const MEETING_LIST_PARAMETERS: &[&str] = &[
    "created_since",
    "created_until",
    "date",
    "include_shared",
    "limit",
    "offset",
    "search",
    "since",
    "until",
    "updated_since",
    "updated_until",
];

/// Raw, unparsed query as each front end receives it.
///
/// Kept separate from `MeetingQuery` so that date-spec parsing, the `date`
/// single-day shorthand and limit clamping happen in exactly one place.
#[derive(Debug, Default)]
pub(crate) struct MeetingQuerySpec<'a> {
    pub date: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub created_since: Option<&'a str>,
    pub created_until: Option<&'a str>,
    pub updated_since: Option<&'a str>,
    pub updated_until: Option<&'a str>,
    pub search: Option<&'a str>,
    pub limit: u32,
    pub offset: u32,
    pub include_shared: bool,
}

/// A resolved query: bounds parsed, search lowercased, limit clamped.
#[derive(Debug)]
pub(crate) struct MeetingQuery {
    pub created_since: Option<DateTime<Utc>>,
    pub created_until: Option<DateTime<Utc>>,
    pub updated_since: Option<DateTime<Utc>>,
    pub updated_until: Option<DateTime<Utc>>,
    pub search: Option<String>,
    pub limit: u32,
    pub offset: u32,
    pub include_shared: bool,
}

/// Largest page either front end will return.
pub(crate) const MAX_LIMIT: u32 = 200;

fn parse_bound(spec: Option<&str>, field: &str) -> Result<Option<DateTime<Utc>>> {
    match spec {
        None => Ok(None),
        Some(s) => crate::output::parse_date_spec(s)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("invalid `{field}`: {e}")),
    }
}

impl MeetingQuerySpec<'_> {
    pub(crate) fn resolve(self) -> Result<MeetingQuery> {
        // `date` is shorthand for a single day. Explicit bounds win over it so
        // that passing both is not silently contradictory.
        let (date_since, date_until) = match self.date {
            Some(day) => {
                let start = parse_bound(Some(day), "date")?.unwrap();
                (Some(start), Some(start + chrono::Duration::days(1)))
            }
            None => (None, None),
        };

        Ok(MeetingQuery {
            created_since: parse_bound(self.created_since.or(self.since), "since")?.or(date_since),
            created_until: parse_bound(self.created_until.or(self.until), "until")?.or(date_until),
            updated_since: parse_bound(self.updated_since, "updated_since")?,
            updated_until: parse_bound(self.updated_until, "updated_until")?,
            search: self.search.map(str::to_lowercase),
            limit: self.limit.clamp(1, MAX_LIMIT),
            offset: self.offset,
            include_shared: self.include_shared,
        })
    }
}

/// One page of matching meetings, with the total so a caller can page.
pub(crate) struct MeetingPage {
    pub total_matched: usize,
    pub offset: u32,
    /// Full documents for this page, newest first.
    pub meetings: Vec<Value>,
}

/// Run a meeting-list query. The single implementation behind both front ends.
pub(crate) fn list_meetings(client: &api::Client, query: &MeetingQuery) -> Result<MeetingPage> {
    let all = fetch_meetings_merged(client, query.include_shared)?;

    // AIDEV-NOTE: matched is materialised before paging so the total can be
    // reported. Without it a caller cannot tell "that is everything" from
    // "there is more", which is the only thing that makes `offset` usable.
    let matched: Vec<Value> = all
        .into_iter()
        .filter(|m| in_date_range(m, query.created_since, query.created_until))
        .filter(|m| in_timestamp_range(m, "updated_at", query.updated_since, query.updated_until))
        .filter(|m| match &query.search {
            Some(q) => m
                .get("title")
                .and_then(Value::as_str)
                .map(|t| t.to_lowercase().contains(q))
                .unwrap_or(false),
            None => true,
        })
        .collect();

    let total_matched = matched.len();
    let meetings = matched
        .into_iter()
        .skip(query.offset as usize)
        .take(query.limit as usize)
        .collect();

    Ok(MeetingPage {
        total_matched,
        offset: query.offset,
        meetings,
    })
}

// ---- Notes ------------------------------------------------------------------

/// The two independent kinds of notes Granola stores for a meeting.
///
/// AIDEV-NOTE: these are genuinely different documents, not fallbacks for one
/// another. `mine` is what you typed during the call (`notes` /
/// `notes_markdown` / `notes_plain`); `ai` is Granola's generated summary
/// (`last_viewed_panel.content`). Roughly a quarter of meetings have both.
/// Earlier code preferred the panel and fell back to yours, which silently
/// discarded your own notes whenever a summary existed — never collapse these
/// back into a single field.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct MeetingNotes {
    pub ai: Option<String>,
    pub mine: Option<String>,
}

fn non_empty(s: String) -> Option<String> {
    let trimmed = s.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Extract both note kinds, each as markdown.
pub(crate) fn meeting_notes(document: &Value) -> MeetingNotes {
    let ai = document
        .pointer("/last_viewed_panel/content")
        .map(crate::prosemirror::to_markdown)
        .and_then(non_empty);

    // Yours: the ProseMirror doc if present, else the flat fields Granola also
    // ships (which are the only form the list endpoint returns).
    let mine = document
        .get("notes")
        .map(crate::prosemirror::to_markdown)
        .and_then(non_empty)
        .or_else(|| {
            document
                .get("notes_markdown")
                .and_then(Value::as_str)
                .map(str::to_string)
                .and_then(non_empty)
        })
        .or_else(|| {
            document
                .get("notes_plain")
                .and_then(Value::as_str)
                .map(str::to_string)
                .and_then(non_empty)
        });

    MeetingNotes { ai, mine }
}

impl MeetingNotes {
    pub(crate) fn is_empty(&self) -> bool {
        self.ai.is_none() && self.mine.is_none()
    }

    /// Structured form, shared by `meeting notes --output json` and the MCP
    /// tool so both surfaces return the same shape. Absent kinds are null
    /// rather than empty strings, so "no notes" is distinguishable from "".
    pub(crate) fn to_json(&self) -> Value {
        serde_json::json!({
            "my_notes": self.mine,
            "ai_notes": self.ai,
        })
    }

    /// Render both kinds under headings, so provenance is never ambiguous.
    /// Yours come first: they are what you chose to write down.
    pub(crate) fn render_markdown(&self, title: Option<&str>) -> String {
        let mut out = String::new();
        if let Some(t) = title {
            out.push_str(&format!("# {t}\n"));
        }
        // Demote by 2 so a content `#` lands at `###`, below the `##` labels.
        if let Some(mine) = &self.mine {
            out.push_str(&format!("\n## My notes\n\n{}\n", demote_headings(mine, 2)));
        }
        if let Some(ai) = &self.ai {
            out.push_str(&format!(
                "\n## AI-enhanced notes\n\n{}\n",
                demote_headings(ai, 2)
            ));
        }
        if self.is_empty() {
            out.push_str("\nThis meeting has no notes.\n");
        }
        out
    }
}

/// Push ATX headings down `by` levels, so embedded content nests *under* the
/// section heading that introduces it.
///
/// AIDEV-NOTE: Granola's AI panel contains its own `# ` headings. Emitted
/// verbatim under a `## AI-enhanced notes` label they outrank that label, so the
/// panel's sections read as top-level sections of the meeting. Fenced code
/// blocks are skipped because a `#` at the start of a line inside one is a
/// comment, not a heading. Caps at H6, the deepest markdown level.
fn demote_headings(markdown: &str, by: usize) -> String {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push(line.to_string());
            continue;
        }
        if !in_fence && trimmed.starts_with('#') {
            let hashes = trimmed.chars().take_while(|c| *c == '#').count();
            // Only a real ATX heading: hashes must be followed by a space.
            if hashes <= 6 && trimmed.chars().nth(hashes) == Some(' ') {
                let extra = "#".repeat((hashes + by).min(6) - hashes);
                out.push(format!("{extra}{line}"));
                continue;
            }
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

/// Whether the caller wrote their own notes for this meeting.
///
/// Derived from the list payload's flat fields as well as the ProseMirror doc,
/// so it works on a list response without a second fetch.
pub(crate) fn has_own_notes(document: &Value) -> bool {
    meeting_notes(document).mine.is_some()
}

// ---- Notetaker bots ---------------------------------------------------------

/// Vendor and role markers that identify an automated meeting-notetaker rather
/// than a person. Matched case-insensitively as substrings.
const NOTETAKER_MARKERS: &[&str] = &[
    "notetaker",
    "note taker",
    "notetaking",
    "fireflies",
    "otter.ai",
    "read.ai",
    "avoma",
    "fathom notetaker",
    "grain.co",
    "chorus.ai",
    "gong.io",
    "meeting recorder",
    "transcription bot",
];

/// Heuristic: does this detected-speaker name look like a notetaker bot?
///
/// AIDEV-NOTE: deliberately a flag rather than a filter, and deliberately
/// conservative — a false positive would erase a real participant. Callers
/// receive the full speaker list plus this annotation and decide for
/// themselves. Calibrated against real transcripts where names like
/// `Panzerino`, `benton` and `Irad "E-rod" Eyal` must not match.
pub(crate) fn looks_like_notetaker_bot(name: &str) -> bool {
    let lower = name.to_lowercase();
    NOTETAKER_MARKERS.iter().any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::{
        attribution_summary, format_transcript_segment, has_own_notes, in_date_range,
        looks_like_notetaker_bot, meeting_context_value, meeting_notes, meeting_summary,
        resolve_meeting_id_from_documents,
    };
    use chrono::{DateTime, Utc};
    use serde_json::json;

    fn timestamp(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn date_range_uses_created_at_not_updated_at() {
        let meeting = json!({
            "created_at": "2026-07-15T17:00:00Z",
            "updated_at": "2026-08-01T17:00:00Z"
        });

        assert!(in_date_range(
            &meeting,
            Some(timestamp("2026-07-15T00:00:00Z")),
            Some(timestamp("2026-07-16T00:00:00Z")),
        ));
    }

    #[test]
    fn date_range_excludes_its_upper_bound() {
        let meeting = json!({ "created_at": "2026-07-16T00:00:00Z" });

        assert!(!in_date_range(
            &meeting,
            Some(timestamp("2026-07-15T00:00:00Z")),
            Some(timestamp("2026-07-16T00:00:00Z")),
        ));
    }

    #[test]
    fn keeps_full_meeting_uuid() {
        let meetings = vec![json!({ "id": "aaaaaaaa-1111-4111-8111-111111111111" })];
        let resolved =
            resolve_meeting_id_from_documents("aaaaaaaa-1111-4111-8111-111111111111", &meetings)
                .expect("full id should be preserved");
        assert_eq!(resolved, "aaaaaaaa-1111-4111-8111-111111111111");
    }

    #[test]
    fn resolves_unique_short_prefix() {
        let meetings = vec![
            json!({ "id": "aaaaaaaa-1111-4111-8111-111111111111" }),
            json!({ "id": "bbbbbbbb-2222-4222-8222-222222222222" }),
        ];
        let resolved = resolve_meeting_id_from_documents("aaaaaaaa", &meetings)
            .expect("short prefix should resolve");
        assert_eq!(resolved, "aaaaaaaa-1111-4111-8111-111111111111");
    }

    #[test]
    fn errors_on_ambiguous_prefix() {
        let meetings = vec![
            json!({ "id": "aaaaaaaa-1111-4111-8111-111111111111" }),
            json!({ "id": "aaaaaaaa-3333-4333-8333-333333333333" }),
        ];
        let err = resolve_meeting_id_from_documents("aaaaaaaa", &meetings)
            .expect_err("ambiguous prefix should fail");
        assert!(
            err.to_string().contains("matched multiple recent meetings"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn errors_on_missing_prefix() {
        let meetings = vec![json!({ "id": "aaaaaaaa-1111-4111-8111-111111111111" })];
        let err = resolve_meeting_id_from_documents("deadbeef", &meetings)
            .expect_err("missing prefix should fail");
        assert!(
            err.to_string().contains("did not match any recent meeting"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn attribution_summary_uses_nested_detected_speaker() {
        let segment = json!({
            "source": "system",
            "start_timestamp": "2026-07-22T16:31:21.054Z",
            "text": "Thanks for that.",
            "detectedSpeaker": { "participantName": "Rae Nakamura" }
        });

        assert_eq!(
            attribution_summary(&json!([segment])),
            json!({
                "channels": [{
                    "source": "system",
                    "segment_count": 1,
                    "detected_speaker_names": ["Rae Nakamura"]
                }],
                "likely_notetaker_bots": [],
                "speaker_attribution": "Only names present in raw transcript segments are listed; no calendar-based inference is applied. Names matching known notetaker vendors are additionally listed in likely_notetaker_bots."
            })
        );
    }

    #[test]
    fn attribution_summary_falls_back_to_legacy_speaker_field() {
        let segment = json!({
            "source": "microphone",
            "start_timestamp": "2026-07-22T16:31:21.054Z",
            "text": "Hello.",
            "detected_speaker_name": "Sam"
        });

        assert_eq!(
            attribution_summary(&json!([segment])),
            json!({
                "channels": [{
                    "source": "microphone",
                    "segment_count": 1,
                    "detected_speaker_names": ["Sam"]
                }],
                "likely_notetaker_bots": [],
                "speaker_attribution": "Only names present in raw transcript segments are listed; no calendar-based inference is applied. Names matching known notetaker vendors are additionally listed in likely_notetaker_bots."
            })
        );
    }

    #[test]
    fn transcript_attribution_output_keeps_raw_source() {
        let segment = json!({
            "source": "system",
            "start_timestamp": "2026-07-22T16:31:21.054Z",
            "text": "Hello.",
            "detectedSpeaker": { "participantName": "Rae Nakamura" }
        });

        assert_eq!(
            format_transcript_segment(&segment),
            "[2026-07-22T16:31:21.054Z] (system; speaker: Rae Nakamura) Hello."
        );
    }

    #[test]
    fn attribution_summary_does_not_infer_names_for_unnamed_channels() {
        let transcript = json!([
            { "source": "microphone", "text": "Hey Rae." },
            { "source": "system", "text": "Hi." },
            {
                "source": "system",
                "text": "Thanks.",
                "detectedSpeaker": { "participantName": "Rae Nakamura" }
            }
        ]);

        assert_eq!(
            attribution_summary(&transcript),
            json!({
                "channels": [
                    {
                        "source": "microphone",
                        "segment_count": 1,
                        "detected_speaker_names": []
                    },
                    {
                        "source": "system",
                        "segment_count": 2,
                        "detected_speaker_names": ["Rae Nakamura"]
                    }
                ],
                "likely_notetaker_bots": [],
                "speaker_attribution": "Only names present in raw transcript segments are listed; no calendar-based inference is applied. Names matching known notetaker vendors are additionally listed in likely_notetaker_bots."
            })
        );
    }

    #[test]
    fn context_is_compact_and_omits_raw_sensitive_fields() {
        let document = json!({
            "id": "meeting-123",
            "title": "Rae / Sam",
            "last_viewed_panel": {
                "content": {
                    "type": "doc",
                    "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "Decision" }] }]
                }
            },
            "unrecognized_document_field": { "kept": true },
            "people": {
                "creator": { "email": "person@example.com", "name": "Sam Okafor" },
                "attendees": [{ "details": { "person": { "name": { "fullName": "Rae" } } } }]
            },
            "google_calendar_event": {
                "start": { "dateTime": "2026-07-22T17:00:00Z", "timeZone": "America/Los_Angeles" },
                "end": { "dateTime": "2026-07-22T17:30:00Z", "timeZone": "America/Los_Angeles" }
            },
            "url": "https://calendar.example.com/private"
        });
        let transcript = json!([
            {
                "id": "segment-123",
                "source": "system",
                "text": "Hello.",
                "unrecognized_segment_field": { "kept": true }
            }
        ]);

        let context = meeting_context_value(document.clone(), transcript.clone()).unwrap();
        assert_eq!(context.pointer("/document/id"), Some(&json!("meeting-123")));
        assert_eq!(context.pointer("/notes/available"), Some(&json!(true)));
        assert_eq!(
            context.pointer("/transcript/segment_count"),
            Some(&json!(1))
        );
        assert_eq!(
            context.pointer("/people/creator_name"),
            Some(&json!("Sam Okafor"))
        );
        assert_eq!(
            context.pointer("/people/attendee_names"),
            Some(&json!(["Rae"]))
        );
        assert!(context.pointer("/document/people").is_none());
        assert!(context.pointer("/document/url").is_none());
        assert!(context
            .pointer("/document/unrecognized_document_field")
            .is_none());
        assert!(context.pointer("/transcript/0").is_none());
    }

    #[test]
    fn meeting_summary_omits_bulk_and_sensitive_fields() {
        let doc = json!({
            "id": "aaaaaaaa-1111-4111-8111-111111111111",
            "title": "Rae / Sam",
            "created_at": "2026-07-22T17:00:00Z",
            "updated_at": "2026-07-22T18:00:00Z",
            "_origin": "owned",
            // Bulk and sensitive fields that must not reach a list response.
            "ydoc_state": "AAAAAAAAAAAAAAAA",
            "notes": { "type": "doc", "content": [] },
            "notes_markdown": "secret decisions",
            "people": {
                "conferencing": { "type": "zoom" },
                "attendees": [
                    { "email": "rae@example.com",
                      "details": { "person": { "name": { "fullName": "Rae" } } } }
                ]
            },
            "google_calendar_event": {
                "start": { "dateTime": "2026-07-22T17:00:00Z" },
                "end": { "dateTime": "2026-07-22T17:30:00Z" }
            }
        });

        let summary = meeting_summary(&doc);

        assert_eq!(summary["title"], json!("Rae / Sam"));
        assert_eq!(summary["platform"], json!("zoom"));
        assert_eq!(summary["start"], json!("2026-07-22T17:00:00Z"));
        assert_eq!(summary["end"], json!("2026-07-22T17:30:00Z"));
        assert_eq!(summary["attendee_names"], json!(["Rae"]));
        assert_eq!(summary["origin"], json!("owned"));

        // Bulk noise and note content stay out.
        for absent in ["ydoc_state", "notes", "notes_markdown", "people"] {
            assert!(
                summary.get(absent).is_none(),
                "`{absent}` must not appear in a list summary"
            );
        }
        // Attendee emails must not leak, in any field.
        assert!(
            !serde_json::to_string(&summary)
                .unwrap()
                .contains("@example.com"),
            "attendee emails must not appear in a list summary"
        );
    }

    #[test]
    fn meeting_summary_tolerates_a_sparse_document() {
        let summary = meeting_summary(&json!({ "id": "x" }));
        assert_eq!(summary["id"], json!("x"));
        assert_eq!(summary["attendee_names"], json!([]));
        assert!(summary["platform"].is_null());
        assert!(summary["start"].is_null());
    }

    /// Regression: an AI panel must not hide notes the user typed themselves.
    ///
    /// The old notes_markdown() preferred last_viewed_panel and fell back to
    /// `notes`, so on the ~25% of meetings with both, your own notes were
    /// silently dropped.
    #[test]
    fn notes_returns_both_kinds_when_both_exist() {
        let doc = json!({
            "last_viewed_panel": { "content": {
                "type": "doc",
                "content": [{ "type": "paragraph",
                              "content": [{ "type": "text", "text": "AI summary" }] }]
            }},
            "notes": { "type": "doc",
                "content": [{ "type": "paragraph",
                              "content": [{ "type": "text", "text": "Note to self" }] }] }
        });
        let notes = meeting_notes(&doc);
        assert_eq!(notes.ai.as_deref(), Some("AI summary"));
        assert_eq!(notes.mine.as_deref(), Some("Note to self"));

        // Both labelled, with the user's own first.
        let md = notes.render_markdown(Some("Title"));
        assert!(md.contains("# Title"));
        assert!(md.contains("## My notes"));
        assert!(md.contains("## AI-enhanced notes"));
        assert!(
            md.find("## My notes") < md.find("## AI-enhanced notes"),
            "your own notes should come first:\n{md}"
        );
    }

    #[test]
    fn notes_fall_back_through_the_flat_fields() {
        // The list endpoint ships notes_markdown / notes_plain but no panel.
        let only_flat = meeting_notes(&json!({ "notes_markdown": "- typed this" }));
        assert_eq!(only_flat.mine.as_deref(), Some("- typed this"));
        assert!(only_flat.ai.is_none());
        assert!(has_own_notes(&json!({ "notes_plain": "typed" })));

        // Empty or whitespace-only must not count as notes.
        assert!(!has_own_notes(&json!({ "notes_markdown": "   " })));
        assert!(meeting_notes(&json!({})).is_empty());
    }

    #[test]
    fn embedded_headings_nest_under_their_section_label() {
        let doc = json!({
            "notes_markdown": "# Note to Self\n- do the thing",
            "last_viewed_panel": { "content": { "type": "doc", "content": [] } }
        });
        let md = meeting_notes(&doc).render_markdown(Some("Standup"));
        assert!(md.contains("# Standup"));
        assert!(md.contains("## My notes"));
        // The note's own H1 is demoted below the section label, not above it.
        assert!(md.contains("### Note to Self"), "got:\n{md}");
        assert!(!md.contains("\n# Note to Self"));
    }

    #[test]
    fn demoting_headings_skips_fenced_code_and_caps_at_h6() {
        let input = "# One\n```\n# not a heading\n```\n###### Six\nplain";
        let out = super::demote_headings(input, 2);
        assert!(out.contains("### One"));
        assert!(
            out.contains("\n# not a heading\n"),
            "fence content changed:\n{out}"
        );
        assert!(out.contains("###### Six"), "must cap at h6:\n{out}");
        assert!(out.contains("plain"));
    }

    #[test]
    fn flags_notetaker_bots_without_touching_real_names() {
        for bot in [
            "Fireflies.ai Notetaker VK",
            "Otter.ai",
            "Read.ai meeting recorder",
            "Some Note Taker",
        ] {
            assert!(looks_like_notetaker_bot(bot), "should flag: {bot}");
        }
        // Real participant names from live transcripts must never match.
        for human in [
            "Rae Nakamura",
            "Sam Okafor",
            "Panzerino",
            "benton",
            "Irad \"E-rod\" Eyal",
            "Rob Zhang",
        ] {
            assert!(!looks_like_notetaker_bot(human), "should not flag: {human}");
        }
    }

    #[test]
    fn attribution_flags_bots_but_keeps_them_in_the_speaker_list() {
        let transcript = json!([
            { "source": "system", "text": "Hi.",
              "detectedSpeaker": { "participantName": "Rae Nakamura" } },
            { "source": "system", "text": "Recording.",
              "detectedSpeaker": { "participantName": "Fireflies.ai Notetaker VK" } }
        ]);
        let summary = attribution_summary(&transcript);

        // Flagged...
        assert_eq!(
            summary["likely_notetaker_bots"],
            json!(["Fireflies.ai Notetaker VK"])
        );
        // ...but still present, because the detection is only a heuristic.
        let names = summary["channels"][0]["detected_speaker_names"].clone();
        assert_eq!(names, json!(["Fireflies.ai Notetaker VK", "Rae Nakamura"]));
    }

    #[test]
    fn context_rejects_non_array_transcript_payloads() {
        let err = meeting_context_value(json!({ "id": "meeting-123" }), json!({ "segments": [] }))
            .expect_err("context needs a raw segment array");
        assert!(err.to_string().contains("not a segment array"));
    }
}
