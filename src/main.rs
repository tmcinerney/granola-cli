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
    /// Meeting (document) ID
    id: String,
    #[command(flatten)]
    out: OutputOpts,
}

#[derive(Args, Clone)]
struct ExportArgs {
    /// Meeting (document) ID
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
    let creds = match auth::load_credentials_from_file() {
        Ok(c) => c,
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
    };
    auth::save_credentials(&creds)?;

    // Validate by hitting /v1/get-workspaces (the cheapest authenticated call
    // per the upstream API spec). This catches the silent-success bug the
    // upstream CLI has — where login appears to succeed but the imported
    // token is already stale.
    let validated = api::with_token_refresh(|c| c.get_workspaces());
    match validated {
        Ok(_) => emit_message(opts, "ok", "Credentials imported and validated"),
        Err(api::Error::Http { status: 401, .. }) => emit_error(
            opts,
            "stale_credentials",
            "Imported credentials were rejected by Granola. This usually means \
             Granola desktop's credentials are stale or encrypted in a format \
             we don't support. Try signing out and back in inside Granola desktop.",
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
        Err(api::Error::Http { status: 401, .. }) => emit_error(
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
    let since = args
        .since
        .as_deref()
        .map(output::parse_date_spec)
        .transpose()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let until = args
        .until
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
        .filter(|m| in_date_range(m, since, until))
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
    let updated = m
        .get("updated_at")
        .or_else(|| m.get("created_at"))
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));
    let Some(updated) = updated else {
        return since.is_none() && until.is_none();
    };
    if let Some(s) = since {
        if updated < s {
            return false;
        }
    }
    if let Some(u) = until {
        if updated > u {
            return false;
        }
    }
    true
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
    let doc = api::with_token_refresh(|c| fetch_full_document(c, &args.id))?;
    output::emit(&doc, args.out.output);
    Ok(())
}

fn meeting_notes(args: &IdArgs) -> Result<()> {
    let doc = api::with_token_refresh(|c| fetch_full_document(c, &args.id))?;
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
    let transcript = api::with_token_refresh(|c| c.get_document_transcript(&args.id))?;
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
    let doc = api::with_token_refresh(|c| fetch_full_document(c, &args.id))?;
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
        let transcript = api::with_token_refresh(|c| c.get_document_transcript(&args.id))?;
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
