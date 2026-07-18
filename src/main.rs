//! granola-cli — Rust port of the upstream JS CLI with the credential-storage
//! fix from beaulebens/granola-cli#6 baked in.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};
use serde_json::Value;

mod api;
mod auth;
mod output;
mod prosemirror;

use output::Format;

const DEFAULT_LIST_LIMIT: u32 = 20;

#[derive(Parser)]
#[command(
    name = "granola",
    version,
    about = "Unofficial CLI for Granola meeting notes",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage authentication
    #[command(subcommand)]
    Auth(AuthCmd),
    /// Work with meetings
    #[command(subcommand)]
    Meeting(MeetingCmd),
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Import credentials from the Granola desktop app
    Login(OutputOpts),
    /// Validate current credentials against the Granola API
    Status(OutputOpts),
    /// Delete credentials from the OS keychain
    Logout(OutputOpts),
}

#[derive(Subcommand)]
enum MeetingCmd {
    /// List recent meetings
    List(ListArgs),
    /// Show meeting metadata
    View(IdArgs),
    /// Print meeting notes as markdown
    Notes(IdArgs),
    /// Print meeting transcript
    Transcript(IdArgs),
    /// Export a meeting (notes + optional transcript) to a file
    Export(ExportArgs),
}

#[derive(Args, Clone)]
struct OutputOpts {
    /// Output format: json, yaml, table, markdown, text
    #[arg(short = 'o', long, default_value = "table")]
    output: Format,
}

#[derive(Args, Clone)]
struct ListArgs {
    /// Maximum number of meetings to return
    #[arg(short = 'l', long, default_value_t = DEFAULT_LIST_LIMIT)]
    limit: u32,
    /// Lower bound — `today`, `7d`, `2h`, or ISO date
    #[arg(long)]
    since: Option<String>,
    /// Upper bound — same accepted forms as --since
    #[arg(long)]
    until: Option<String>,
    /// Lower creation-time bound — same accepted forms as --since
    #[arg(long)]
    created_since: Option<String>,
    /// Exclusive upper creation-time bound — same accepted forms as --since
    #[arg(long)]
    created_until: Option<String>,
    /// Lower document-update-time bound — same accepted forms as --since
    #[arg(long)]
    updated_since: Option<String>,
    /// Exclusive upper document-update-time bound — same accepted forms as --since
    #[arg(long)]
    updated_until: Option<String>,
    /// Substring match on meeting title (case-insensitive)
    #[arg(short = 's', long)]
    search: Option<String>,
    /// Skip merging in shared (non-owned) documents
    #[arg(long)]
    no_shared: bool,
    #[command(flatten)]
    out: OutputOpts,
}

#[derive(Args, Clone)]
struct IdArgs {
    /// Meeting (document) ID or unique prefix from `meeting list`
    id: String,
    #[command(flatten)]
    out: OutputOpts,
}

#[derive(Args, Clone)]
struct ExportArgs {
    /// Meeting (document) ID or unique prefix from `meeting list`
    id: String,
    /// Output file path (default: stdout)
    #[arg(short = 'f', long)]
    output_file: Option<PathBuf>,
    /// Include the transcript section in the export
    #[arg(long)]
    include_transcript: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Auth(c) => run_auth(c),
        Command::Meeting(c) => run_meeting(c),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Last-resort: ensure stderr always carries the human-readable
            // error. JSON-output paths print their own JSON error to stdout
            // before propagating, so this stderr line is debug context only.
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

// ---- auth ------------------------------------------------------------------

fn run_auth(cmd: &AuthCmd) -> Result<()> {
    match cmd {
        AuthCmd::Login(o) => auth_login(o),
        AuthCmd::Status(o) => auth_status(o),
        AuthCmd::Logout(o) => auth_logout(o),
    }
}

fn auth_login(opts: &OutputOpts) -> Result<()> {
    match auth::load_credentials_from_file() {
        Ok(c) => auth::save_credentials(&c)?,
        #[cfg(target_os = "macos")]
        Err(auth::Error::DesktopKeyMigrated) => match auth::bootstrap_migrated_credentials() {
            Ok(_) => {}
            Err(auth::Error::RefreshRejected { .. }) => {
                return emit_error(
                    opts,
                    "bootstrap_refresh_rejected",
                    "Granola rejected the leftover desktop refresh token. This install can no \
                     longer bootstrap CLI credentials from local desktop state.",
                )
            }
            Err(auth::Error::NoDesktopCredentials { .. }) => {
                return emit_error(
                    opts,
                    "no_bootstrap_credentials",
                    "Granola moved its encryption key into app-only storage and no leftover \
                     plaintext refresh token is available for one-time CLI bootstrap.",
                )
            }
            Err(e) => return Err(e.into()),
        },
        Err(auth::Error::NoDesktopCredentials { tried }) => {
            let msg = format!(
                "could not find Granola credentials on disk. Looked in: {}. \
                 Is the Granola desktop app installed and signed in?",
                tried
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return emit_error(opts, "no_desktop_credentials", &msg);
        }
        Err(e) => return Err(e.into()),
    }
    // Validate by hitting /v1/get-workspaces (the cheapest authenticated call
    // per the upstream API spec). This catches the silent-success bug the
    // upstream CLI has — where login appears to succeed but the imported
    // token is already stale.
    let validated = api::with_token_refresh(|c| c.get_workspaces());
    match validated {
        Ok(_) => emit_message(opts, "ok", "Credentials imported and validated"),
        Err(e) if is_stale_credentials_error(&e) => emit_error(
            opts,
            "stale_credentials",
            "Imported credentials were rejected by Granola. This usually means \
             Granola desktop's session is stale. Try re-importing with \
             `granola auth login` after confirming Granola desktop is signed in.",
        ),
        Err(e) => Err(e.into()),
    }
}

fn auth_status(opts: &OutputOpts) -> Result<()> {
    if auth::get_credentials()?.is_none() {
        return emit_error(
            opts,
            "unauthenticated",
            "Not logged in. Run `granola auth login`.",
        );
    }
    match api::with_token_refresh(|c| c.get_workspaces()) {
        Ok(_) => emit_message(opts, "ok", "Authenticated and validated"),
        Err(e) if is_stale_credentials_error(&e) => emit_error(
            opts,
            "stale_credentials",
            "Credentials in keychain were rejected. Run `granola auth login` to re-import.",
        ),
        Err(e) => Err(e.into()),
    }
}

fn auth_logout(opts: &OutputOpts) -> Result<()> {
    auth::delete_credentials()?;
    emit_message(opts, "ok", "Logged out")
}

fn is_stale_credentials_error(err: &api::Error) -> bool {
    matches!(err, api::Error::Http { status: 401, .. })
        || matches!(err, api::Error::Auth(auth::Error::RefreshRejected { .. }))
}

fn emit_message(opts: &OutputOpts, code: &str, message: &str) -> Result<()> {
    match opts.output {
        Format::Json => println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({ "ok": true, "code": code, "message": message })
            )
            .unwrap()
        ),
        _ => println!("{message}"),
    }
    Ok(())
}

fn emit_error(opts: &OutputOpts, code: &str, message: &str) -> Result<()> {
    if opts.output == Format::Json {
        output::emit_json_error(code, message);
    }
    anyhow::bail!("{message}");
}

// ---- meeting ---------------------------------------------------------------

fn run_meeting(cmd: &MeetingCmd) -> Result<()> {
    match cmd {
        MeetingCmd::List(a) => meeting_list(a),
        MeetingCmd::View(a) => meeting_view(a),
        MeetingCmd::Notes(a) => meeting_notes(a),
        MeetingCmd::Transcript(a) => meeting_transcript(a),
        MeetingCmd::Export(a) => meeting_export(a),
    }
}

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

fn meeting_list(args: &ListArgs) -> Result<()> {
    let created_since = args
        .created_since
        .as_deref()
        .or(args.since.as_deref())
        .map(output::parse_date_spec)
        .transpose()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let created_until = args
        .created_until
        .as_deref()
        .or(args.until.as_deref())
        .map(output::parse_date_spec)
        .transpose()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let updated_since = args
        .updated_since
        .as_deref()
        .map(output::parse_date_spec)
        .transpose()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let updated_until = args
        .updated_until
        .as_deref()
        .map(output::parse_date_spec)
        .transpose()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let search = args.search.as_deref().map(str::to_lowercase);

    let meetings = api::with_token_refresh(|c| {
        fetch_meetings_merged(c, !args.no_shared).map_err(|e| api::Error::Transport(e.to_string()))
    })?;

    let filtered: Vec<Value> = meetings
        .into_iter()
        .filter(|m| in_date_range(m, created_since, created_until))
        .filter(|m| in_timestamp_range(m, "updated_at", updated_since, updated_until))
        .filter(|m| match &search {
            Some(q) => m
                .get("title")
                .and_then(Value::as_str)
                .map(|t| t.to_lowercase().contains(q))
                .unwrap_or(false),
            None => true,
        })
        .take(args.limit as usize)
        .collect();

    match args.out.output {
        Format::Json | Format::Yaml => output::emit(&filtered, args.out.output),
        Format::Table => println!("{}", output::meeting_table(&filtered)),
        Format::Markdown | Format::Text => {
            for m in &filtered {
                let title = m
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("(untitled)");
                let id = m.get("id").and_then(Value::as_str).unwrap_or("");
                let date = m.get("created_at").and_then(Value::as_str).unwrap_or("");
                println!("- {date} · {title} ({id})");
            }
        }
    }
    Ok(())
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

fn resolve_meeting_id_from_documents(raw_id: &str, meetings: &[Value]) -> Result<String> {
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
fn resolve_meeting_id(client: &api::Client, raw_id: &str) -> Result<String> {
    let meetings = fetch_meetings_merged(client, true)?;
    resolve_meeting_id_from_documents(raw_id, &meetings)
}

/// Fetch the full document via `/v1/get-documents-batch` with
/// `include_last_viewed_panel: true`. This is the most reliable single-doc
/// fetch path — `get-document-metadata` returns a sparse view on many
/// accounts and doesn't include notes content.
fn fetch_full_document(client: &api::Client, id: &str) -> Result<Value, api::Error> {
    let resp = client.get_documents_batch(&[id.to_string()], true)?;
    let docs = resp
        .get("documents")
        .or_else(|| resp.get("docs"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(docs.into_iter().next().unwrap_or(Value::Null))
}

fn meeting_view(args: &IdArgs) -> Result<()> {
    let doc = api::with_token_refresh(|c| {
        let id =
            resolve_meeting_id(c, &args.id).map_err(|e| api::Error::Transport(e.to_string()))?;
        fetch_full_document(c, &id)
    })?;
    output::emit(&doc, args.out.output);
    Ok(())
}

fn meeting_notes(args: &IdArgs) -> Result<()> {
    let doc = api::with_token_refresh(|c| {
        let id =
            resolve_meeting_id(c, &args.id).map_err(|e| api::Error::Transport(e.to_string()))?;
        fetch_full_document(c, &id)
    })?;
    let notes_doc = doc
        .pointer("/last_viewed_panel/content")
        .or_else(|| doc.get("notes"))
        .cloned()
        .unwrap_or(Value::Null);
    if matches!(args.out.output, Format::Json | Format::Yaml) {
        output::emit(&notes_doc, args.out.output);
    } else {
        let md = prosemirror::to_markdown(&notes_doc);
        if md.is_empty() {
            // Fall back to notes_markdown field if present.
            let fallback = doc
                .get("notes_markdown")
                .and_then(Value::as_str)
                .unwrap_or("");
            println!("{fallback}");
        } else {
            println!("{md}");
        }
    }
    Ok(())
}

fn meeting_transcript(args: &IdArgs) -> Result<()> {
    let transcript = api::with_token_refresh(|c| {
        let id =
            resolve_meeting_id(c, &args.id).map_err(|e| api::Error::Transport(e.to_string()))?;
        c.get_document_transcript(&id)
    })?;
    match args.out.output {
        Format::Json | Format::Yaml => output::emit(&transcript, args.out.output),
        _ => {
            if let Some(arr) = transcript.as_array() {
                for seg in arr {
                    let source = seg.get("source").and_then(Value::as_str).unwrap_or("");
                    let text = seg.get("text").and_then(Value::as_str).unwrap_or("");
                    let ts = seg
                        .get("start_timestamp")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    println!("[{ts}] ({source}) {text}");
                }
            }
        }
    }
    Ok(())
}

fn meeting_export(args: &ExportArgs) -> Result<()> {
    let resolved_id = api::with_token_refresh(|c| {
        resolve_meeting_id(c, &args.id).map_err(|e| api::Error::Transport(e.to_string()))
    })?;
    let doc = api::with_token_refresh(|c| fetch_full_document(c, &resolved_id))?;
    let title = doc
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("(untitled)");
    let notes_doc = doc
        .pointer("/last_viewed_panel/content")
        .or_else(|| doc.get("notes"))
        .cloned()
        .unwrap_or(Value::Null);
    let notes_md = prosemirror::to_markdown(&notes_doc);
    let fallback = doc
        .get("notes_markdown")
        .and_then(Value::as_str)
        .unwrap_or("");
    let notes = if notes_md.is_empty() {
        fallback.to_string()
    } else {
        notes_md
    };

    let mut out = format!("# {title}\n\n{notes}\n");

    if args.include_transcript {
        let transcript = api::with_token_refresh(|c| c.get_document_transcript(&resolved_id))?;
        out.push_str("\n## Transcript\n\n");
        if let Some(arr) = transcript.as_array() {
            for seg in arr {
                let source = seg.get("source").and_then(Value::as_str).unwrap_or("");
                let text = seg.get("text").and_then(Value::as_str).unwrap_or("");
                let ts = seg
                    .get("start_timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                out.push_str(&format!("- [{ts}] ({source}) {text}\n"));
            }
        }
    }

    match &args.output_file {
        Some(path) => {
            fs::write(path, &out).with_context(|| format!("writing {}", path.display()))?;
            eprintln!("wrote {}", path.display());
        }
        None => print!("{out}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{in_date_range, resolve_meeting_id_from_documents};
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
}
