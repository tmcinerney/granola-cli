//! granola-cli — Rust port of the upstream JS CLI with the credential-storage
//! fix from beaulebens/granola-cli#6 baked in.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::Value;

mod api;
mod auth;
mod mcp;
mod meetings;
mod output;
mod prosemirror;

use meetings::{
    fetch_full_document, fetch_meetings_merged, format_transcript_segment, in_date_range,
    in_timestamp_range, meeting_context_value, notes_markdown, resolve_meeting_id,
};
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
    /// Serve meeting data to AI clients over the Model Context Protocol
    ///
    /// AIDEV-NOTE: speaks JSON-RPC on stdin/stdout, so it is meant to be
    /// spawned by an MCP client rather than run interactively. Anything printed
    /// to stdout by this subcommand corrupts the protocol stream.
    Mcp,
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
    Transcript(TranscriptArgs),
    /// Show document, transcript, and conservative attribution context
    Context(IdArgs),
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
struct TranscriptArgs {
    /// Meeting (document) ID or unique prefix from `meeting list`
    id: String,
    /// Show speaker names when Granola supplies them in raw transcript segments
    #[arg(long)]
    show_attribution: bool,
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
        Command::Mcp => mcp::run(),
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
        MeetingCmd::Context(a) => meeting_context(a),
        MeetingCmd::Export(a) => meeting_export(a),
    }
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
    if matches!(args.out.output, Format::Json | Format::Yaml) {
        output::emit(&meetings::notes_document(&doc), args.out.output);
    } else {
        println!("{}", notes_markdown(&doc));
    }
    Ok(())
}

fn meeting_transcript(args: &TranscriptArgs) -> Result<()> {
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
                    if args.show_attribution {
                        println!("{}", format_transcript_segment(seg));
                    } else {
                        println!("[{ts}] ({source}) {text}");
                    }
                }
            }
        }
    }
    Ok(())
}

fn meeting_context(args: &IdArgs) -> Result<()> {
    let resolved_id = api::with_token_refresh(|c| {
        resolve_meeting_id(c, &args.id).map_err(|e| api::Error::Transport(e.to_string()))
    })?;
    let doc = api::with_token_refresh(|c| fetch_full_document(c, &resolved_id))?;
    let transcript = api::with_token_refresh(|c| c.get_document_transcript(&resolved_id))?;
    let context = meeting_context_value(doc, transcript)?;

    match args.out.output {
        Format::Json | Format::Yaml => output::emit(&context, args.out.output),
        _ => print_context_summary(&context),
    }
    Ok(())
}

fn print_context_summary(context: &Value) {
    let title = context
        .pointer("/document/title")
        .and_then(Value::as_str)
        .unwrap_or("(untitled)");
    let id = context
        .pointer("/document/id")
        .and_then(Value::as_str)
        .unwrap_or("");
    println!("Meeting: {title}");
    if !id.is_empty() {
        println!("Document ID: {id}");
    }
    println!("Transcript channels:");
    if let Some(channels) = context
        .pointer("/attribution/channels")
        .and_then(Value::as_array)
    {
        for channel in channels {
            let source = channel
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let count = channel
                .get("segment_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let names = channel
                .get("detected_speaker_names")
                .and_then(Value::as_array)
                .map(|names| {
                    names
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            if names.is_empty() {
                println!("- {source}: {count} segments");
            } else {
                println!("- {source}: {count} segments; detected speakers: {names}");
            }
        }
    }
    println!("Raw data: `granola meeting view <id> --output json` and `granola meeting transcript <id> --output json`.");
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
    let notes = notes_markdown(&doc);

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
