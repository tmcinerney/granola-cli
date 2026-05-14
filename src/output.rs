//! Output formatting + relative date parsing.
//!
//! Format dispatch for `--output {json,yaml,table,markdown}` and a
//! relative-date parser that accepts `today`, `yesterday`, `7d`, `2h`,
//! `2025-08-18`, or full ISO timestamps. The CLI does this so skills
//! never have to compute dates from natural language.

use std::fmt::Display;
use std::str::FromStr;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};
use comfy_table::{Cell, ContentArrangement, Table};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Yaml,
    Table,
    Markdown,
    Text,
}

impl FromStr for Format {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "json" => Ok(Format::Json),
            "yaml" => Ok(Format::Yaml),
            "table" => Ok(Format::Table),
            "markdown" | "md" => Ok(Format::Markdown),
            "text" | "txt" => Ok(Format::Text),
            other => Err(format!("unknown format: {other}")),
        }
    }
}

impl Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Format::Json => "json",
            Format::Yaml => "yaml",
            Format::Table => "table",
            Format::Markdown => "markdown",
            Format::Text => "text",
        })
    }
}

/// Emit a value in the requested format.
pub fn emit<T: Serialize>(value: &T, format: Format) {
    let json_value = serde_json::to_value(value).unwrap_or(Value::Null);
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(&json_value).unwrap()),
        Format::Yaml => print!("{}", serde_yaml::to_string(&json_value).unwrap_or_default()),
        Format::Text | Format::Markdown => {
            // Default fallback for non-tabular data: pretty JSON.
            println!("{}", serde_json::to_string_pretty(&json_value).unwrap());
        }
        Format::Table => {
            // Table formatting is per-command (different schemas); commands
            // that want a table call `meeting_table` etc. directly.
            println!("{}", serde_json::to_string_pretty(&json_value).unwrap());
        }
    }
}

/// JSON error shape on stdout, so agentic skills consuming `--output json`
/// always parse pure JSON regardless of failure mode.
pub fn emit_json_error(code: &str, message: &str) {
    let v = serde_json::json!({
        "error": { "code": code, "message": message }
    });
    println!("{}", serde_json::to_string_pretty(&v).unwrap());
}

// ---- Meeting list table -----------------------------------------------------

pub fn meeting_table(meetings: &[Value]) -> String {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            Cell::new("ID"),
            Cell::new("Date"),
            Cell::new("Title"),
            Cell::new("Origin"),
        ]);
    for m in meetings {
        let id = m
            .get("id")
            .and_then(Value::as_str)
            .map(|s| s.chars().take(8).collect::<String>())
            .unwrap_or_default();
        let date = m
            .get("created_at")
            .and_then(Value::as_str)
            .map(|s| s.split('T').next().unwrap_or(s).to_string())
            .unwrap_or_default();
        let title = m
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("(untitled)")
            .to_string();
        let origin = m
            .get("_origin")
            .and_then(Value::as_str)
            .unwrap_or("owned")
            .to_string();
        table.add_row(vec![id, date, title, origin]);
    }
    table.to_string()
}

// ---- Date parsing -----------------------------------------------------------

/// Parse a CLI `--since` / `--until` value.
///
/// Accepts:
/// - `today` → midnight UTC today
/// - `yesterday` → midnight UTC yesterday
/// - relative durations: `7d`, `2h`, `30m` (interpreted as "now minus N")
/// - ISO dates: `2025-08-18`
/// - full ISO 8601: `2025-08-18T14:04:59Z`
pub fn parse_date_spec(s: &str) -> Result<DateTime<Utc>, String> {
    let trimmed = s.trim().to_ascii_lowercase();
    let now = Utc::now();

    match trimmed.as_str() {
        "today" => Ok(now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc()),
        "yesterday" => Ok((now - ChronoDuration::days(1))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()),
        _ => {
            // Relative duration: `7d`, `2h`, `30m`, `45s`
            if let Some(rel) = parse_relative(&trimmed) {
                return Ok(now - rel);
            }
            // ISO date-only
            if let Ok(d) = NaiveDate::parse_from_str(&trimmed, "%Y-%m-%d") {
                return Ok(d.and_hms_opt(0, 0, 0).unwrap().and_utc());
            }
            // Full RFC3339 / ISO8601
            if let Ok(dt) = DateTime::parse_from_rfc3339(&trimmed) {
                return Ok(dt.with_timezone(&Utc));
            }
            Err(format!(
                "could not parse date spec `{s}` — try `today`, `7d`, `2h`, or ISO date `YYYY-MM-DD`"
            ))
        }
    }
}

fn parse_relative(s: &str) -> Option<ChronoDuration> {
    let (digits, unit) = s.split_at(s.find(|c: char| c.is_alphabetic())?);
    let n: i64 = digits.parse().ok()?;
    match unit {
        "s" => Some(ChronoDuration::seconds(n)),
        "m" => Some(ChronoDuration::minutes(n)),
        "h" => Some(ChronoDuration::hours(n)),
        "d" => Some(ChronoDuration::days(n)),
        "w" => Some(ChronoDuration::weeks(n)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_today() {
        let d = parse_date_spec("today").unwrap();
        let today = Utc::now().date_naive();
        assert_eq!(d.date_naive(), today);
    }

    #[test]
    fn parses_relative_days() {
        let d = parse_date_spec("7d").unwrap();
        let expected = Utc::now() - ChronoDuration::days(7);
        // within 1s tolerance
        assert!((d - expected).num_seconds().abs() < 2);
    }

    #[test]
    fn parses_iso_date() {
        let d = parse_date_spec("2025-08-18").unwrap();
        assert_eq!(
            d.date_naive(),
            NaiveDate::from_ymd_opt(2025, 8, 18).unwrap()
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_date_spec("not-a-date").is_err());
    }
}
