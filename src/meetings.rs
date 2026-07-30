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
pub(crate) fn fetch_meetings_merged(
    client: &api::Client,
    include_shared: bool,
) -> Result<Vec<Value>> {
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

pub(crate) fn in_date_range(
    m: &Value,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> bool {
    in_timestamp_range(m, "created_at", since, until)
}

pub(crate) fn in_timestamp_range(
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

/// Extract the ProseMirror notes document, falling back to the top-level
/// `notes` field when no panel has been viewed.
pub(crate) fn notes_document(document: &Value) -> Value {
    document
        .pointer("/last_viewed_panel/content")
        .or_else(|| document.get("notes"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Notes rendered as markdown, falling back to Granola's flat `notes_markdown`
/// field when the ProseMirror document is absent or renders empty.
pub(crate) fn notes_markdown(document: &Value) -> String {
    let rendered = crate::prosemirror::to_markdown(&notes_document(document));
    if rendered.is_empty() {
        document
            .get("notes_markdown")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        rendered
    }
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
    match detected_speaker_name(seg) {
        Some(speaker) => format!("[{ts}] ({source}; speaker: {speaker}) {text}"),
        None => format!("[{ts}] ({source}) {text}"),
    }
}

/// Return only speaker identity supplied by Granola's transcript payload.
/// In particular, this must not infer a remote name from calendar attendees:
/// a `system` audio channel can contain multiple remote participants.
fn detected_speaker_name(seg: &Value) -> Option<&str> {
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
        if let Some(name) = detected_speaker_name(segment) {
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
    serde_json::json!({
        "channels": channels,
        "speaker_attribution": "Only names present in raw transcript segments are listed; no calendar-based inference is applied.",
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
    let prosemirror = notes_document(&document);
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
            "available": !prosemirror.is_null(),
            "format": if prosemirror.is_null() { Value::Null } else { Value::String("prosemirror".into()) },
        },
        "transcript": {
            "segment_count": segments.len(),
        },
        "attribution": attribution,
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        attribution_summary, format_transcript_segment, in_date_range, meeting_context_value,
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
        let meetings = vec![json!({ "id": "bdb68fba-fdf4-4b97-b7e2-b63deca0f234" })];
        let resolved =
            resolve_meeting_id_from_documents("bdb68fba-fdf4-4b97-b7e2-b63deca0f234", &meetings)
                .expect("full id should be preserved");
        assert_eq!(resolved, "bdb68fba-fdf4-4b97-b7e2-b63deca0f234");
    }

    #[test]
    fn resolves_unique_short_prefix() {
        let meetings = vec![
            json!({ "id": "bdb68fba-fdf4-4b97-b7e2-b63deca0f234" }),
            json!({ "id": "fa148cc7-b834-4dfd-9a58-8f93fb069022" }),
        ];
        let resolved = resolve_meeting_id_from_documents("bdb68fba", &meetings)
            .expect("short prefix should resolve");
        assert_eq!(resolved, "bdb68fba-fdf4-4b97-b7e2-b63deca0f234");
    }

    #[test]
    fn errors_on_ambiguous_prefix() {
        let meetings = vec![
            json!({ "id": "bdb68fba-fdf4-4b97-b7e2-b63deca0f234" }),
            json!({ "id": "bdb68fba-1111-4b97-b7e2-b63deca0f235" }),
        ];
        let err = resolve_meeting_id_from_documents("bdb68fba", &meetings)
            .expect_err("ambiguous prefix should fail");
        assert!(
            err.to_string().contains("matched multiple recent meetings"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn errors_on_missing_prefix() {
        let meetings = vec![json!({ "id": "bdb68fba-fdf4-4b97-b7e2-b63deca0f234" })];
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
            "detectedSpeaker": { "participantName": "Gary Grossman" }
        });

        assert_eq!(
            attribution_summary(&json!([segment])),
            json!({
                "channels": [{
                    "source": "system",
                    "segment_count": 1,
                    "detected_speaker_names": ["Gary Grossman"]
                }],
                "speaker_attribution": "Only names present in raw transcript segments are listed; no calendar-based inference is applied."
            })
        );
    }

    #[test]
    fn attribution_summary_falls_back_to_legacy_speaker_field() {
        let segment = json!({
            "source": "microphone",
            "start_timestamp": "2026-07-22T16:31:21.054Z",
            "text": "Hello.",
            "detected_speaker_name": "Travers"
        });

        assert_eq!(
            attribution_summary(&json!([segment])),
            json!({
                "channels": [{
                    "source": "microphone",
                    "segment_count": 1,
                    "detected_speaker_names": ["Travers"]
                }],
                "speaker_attribution": "Only names present in raw transcript segments are listed; no calendar-based inference is applied."
            })
        );
    }

    #[test]
    fn transcript_attribution_output_keeps_raw_source() {
        let segment = json!({
            "source": "system",
            "start_timestamp": "2026-07-22T16:31:21.054Z",
            "text": "Hello.",
            "detectedSpeaker": { "participantName": "Gary Grossman" }
        });

        assert_eq!(
            format_transcript_segment(&segment),
            "[2026-07-22T16:31:21.054Z] (system; speaker: Gary Grossman) Hello."
        );
    }

    #[test]
    fn attribution_summary_does_not_infer_names_for_unnamed_channels() {
        let transcript = json!([
            { "source": "microphone", "text": "Hey Gary." },
            { "source": "system", "text": "Hi." },
            {
                "source": "system",
                "text": "Thanks.",
                "detectedSpeaker": { "participantName": "Gary Grossman" }
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
                        "detected_speaker_names": ["Gary Grossman"]
                    }
                ],
                "speaker_attribution": "Only names present in raw transcript segments are listed; no calendar-based inference is applied."
            })
        );
    }

    #[test]
    fn context_is_compact_and_omits_raw_sensitive_fields() {
        let document = json!({
            "id": "meeting-123",
            "title": "Gary / Travers",
            "last_viewed_panel": {
                "content": {
                    "type": "doc",
                    "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "Decision" }] }]
                }
            },
            "unrecognized_document_field": { "kept": true },
            "people": {
                "creator": { "email": "person@example.com", "name": "Travers McInerney" },
                "attendees": [{ "details": { "person": { "name": { "fullName": "Gary" } } } }]
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
            Some(&json!("Travers McInerney"))
        );
        assert_eq!(
            context.pointer("/people/attendee_names"),
            Some(&json!(["Gary"]))
        );
        assert!(context.pointer("/document/people").is_none());
        assert!(context.pointer("/document/url").is_none());
        assert!(context
            .pointer("/document/unrecognized_document_field")
            .is_none());
        assert!(context.pointer("/transcript/0").is_none());
    }

    #[test]
    fn context_rejects_non_array_transcript_payloads() {
        let err = meeting_context_value(json!({ "id": "meeting-123" }), json!({ "segments": [] }))
            .expect_err("context needs a raw segment array");
        assert!(err.to_string().contains("not a segment array"));
    }
}
