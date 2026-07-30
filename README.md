# granola-cli

A Rust CLI and MCP server for [Granola](https://granola.ai/) meeting notes, with
a fix for the credential-storage break introduced in Granola desktop ≥7.162.

This is a Rust port of [`magarcia/granola-cli`](https://github.com/magarcia/granola-cli)
incorporating the credential-discovery fix from
[PR #6](https://github.com/magarcia/granola-cli/pull/6) so authentication
keeps working on current Granola desktop versions.

One binary, three ways to use it:

- a **command-line tool** — `granola meeting list`, `granola meeting notes <id>`, …
- an **[MCP](https://modelcontextprotocol.io) server** — `granola mcp`, exposing
  meetings, notes and transcripts as tools to any MCP client
  (see [MCP server](#mcp-server))
- a **[Claude Code](https://claude.com/claude-code) plugin** — the repo doubles as
  a plugin marketplace with four skills that wrap the CLI for agentic use:
  - `granola-recent` — fetch recent meetings
  - `granola-notes` — look up notes by title or date
  - `granola-export` — save meeting markdown to disk
  - `granola-search` — search across recent meetings

---

## Install

### Homebrew (macOS / Linux)

```sh
brew install tmcinerney/tap/granola-cli
```

### Cargo

```sh
cargo install granola-cli
```

### Pre-built binaries

Download from [Releases](https://github.com/tmcinerney/granola-cli/releases).

---

## First-time setup

You need the Granola desktop app installed and signed in. The CLI imports
your credentials from it once, stores them in your OS keychain, and rotates
them automatically afterwards.

```sh
granola auth login        # imports credentials from the desktop app
granola auth status       # validates against the Granola API
granola meeting list      # smoke test — should show recent meetings
```

On macOS, Granola desktop 7.427+ moves its encryption key into app-only
Keychain storage. Upgraded installs can still run `granola auth login` once:
the CLI exchanges a leftover plaintext refresh token and stores its own rotated
credential chain in the OS keychain. Fresh installs with no leftover plaintext
token cannot use this bootstrap path.

## Usage

```sh
granola meeting list --since 7d                    # last 7 days (owned + shared)
granola meeting list --since today --output json   # today only, machine-readable
granola meeting list --search "design review"      # title search

granola meeting view <id>                          # metadata
granola meeting notes <id>                         # notes as markdown
granola meeting transcript <id>                    # full transcript
granola meeting transcript <id> --show-attribution # include Granola-supplied speaker names
granola meeting context <id> --output json         # compact attribution/context summary
granola meeting export <id> --output-file out.md   # combined export
```

By default, `meeting list` merges your own meetings with meetings shared to
you (a gap in the upstream CLI). Pass `--no-shared` to skip that hop.

### Transcript attribution and context

`granola meeting transcript` keeps Granola's raw audio-channel labels, such
as `microphone` and `system`. They describe capture channels, not necessarily
individual people: a `system` channel can contain several remote speakers.

Use `granola meeting context <id> --output json` for a compact, stable
summary of the document, note availability, transcript channels, and any
speaker names Granola supplied directly in transcript segments. It never
assigns calendar attendees or a document creator as speakers. The summary
also omits emails, meeting URLs, editable note content, and raw transcript
text.

For complete, lossless API responses, use the existing raw commands:

```sh
granola meeting view <id> --output json        # complete meeting document, including editable notes
granola meeting transcript <id> --output json  # complete raw transcript segment array
```

Granola may expose a nested `detectedSpeaker.participantName` field or a
legacy `detected_speaker_name` field on some transcript segments. The CLI
lists either only when Granola actually provides it; unnamed channels remain
unnamed. Add `--show-attribution` to the human-readable transcript output to
show those optional names without replacing the raw channel label.

---

## MCP server

The same binary is also an [MCP](https://modelcontextprotocol.io) server, so AI
clients can query your Granola data directly:

```sh
granola mcp
```

It speaks JSON-RPC over stdin/stdout, so it is meant to be spawned by an MCP
client rather than run by hand. Authentication is shared with the CLI — run
`granola auth login` once in a terminal and the server uses the same keychain
credentials, refreshing them automatically. It never prompts for login itself,
since it has no terminal to prompt on; if credentials are missing it returns an
error telling you to run `granola auth login`.

### Tools

| Tool | Returns |
|---|---|
| `granola_list_meetings` | Meetings newest-first, filterable by date range or title substring |
| `granola_get_notes` | AI-enhanced notes for one meeting, as markdown |
| `granola_get_transcript` | Full transcript for one meeting |
| `granola_get_meeting_context` | Compact context: calendar window, attendees, per-channel attribution |

`granola_list_meetings` accepts `since` / `until` (ISO date, RFC3339, `today`,
`yesterday`, or a relative span like `7d`), `date` for a single day, `search`,
`limit`, and `response_format` (`json` or `markdown`). The rest take a
`meeting_id` — a full ID or any unique prefix.

Arguments are passed flat, and unrecognised ones are rejected rather than
ignored, so a typo fails loudly instead of silently returning an unfiltered
list. The tool schemas advertise this as `additionalProperties: false`.

`granola_list_meetings` returns a compact summary per meeting — id, title,
timestamps, calendar window, platform, attendee names — not the full Granola
document. A raw document carries ~47 fields, mostly bulk (`ydoc_state`) or
detail that belongs in a per-meeting call, so listing 50 of them cost ~600k
characters. Use `granola_get_notes` or `granola_get_meeting_context` for detail
on a specific meeting.

In JSON the response is `{ total_matched, offset, count, meetings }`. Page with
`offset` — MCP has no cursor pagination for tool results, so `total_matched` is
how you tell "that's everything" from "there's more".

### Client setup

Most clients take a command and args. For Claude Desktop
(`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "granola": {
      "command": "/opt/homebrew/bin/granola",
      "args": ["mcp"]
    }
  }
}
```

Use an **absolute path** to the binary. GUI clients spawn servers with a minimal
`PATH` that usually excludes Homebrew's bin directory, so a bare `granola` will
not resolve. `which granola` tells you the path to use.

For Claude Code, `claude mcp add granola -- /opt/homebrew/bin/granola mcp`.

---

## Claude Code plugin

The repo's `.claude-plugin/marketplace.json` makes this a Claude Code plugin
marketplace. To install the plugin:

```
/plugin marketplace add tmcinerney/granola-cli
/plugin install granola@tmcinerney-granola
```

Then invoke any of the bundled skills naturally:

> "What meetings did I have yesterday?"
> "Pull the notes from my design review with Acme"
> "Export today's standup to ~/notes/standup.md"

The plugin requires the `granola` binary on `$PATH` (see Install above).

---

## Why a Rust rewrite?

Granola desktop ≥7.162 stores fresh tokens in `stored-accounts.json`; the
upstream CLI only reads `supabase.json`, which the desktop app no longer
updates. The result is a silent-success authentication bug — `granola auth
login` reports success, but every subsequent API call returns "Authentication
required."

The upstream fix
([PR #6](https://github.com/magarcia/granola-cli/pull/6)) has been open
without maintainer review since 2026-05-07. This rewrite incorporates that
fix natively, plus a few additions:

- **Validates credentials** during `auth status` instead of just checking
  the keychain (no silent-success bug).
- **Merges shared meetings** in `meeting list` (upstream only returns owned
  documents).
- **JSON errors on stdout** with `--output json`, so agentic skills can pipe
  through `jq` safely.
- **Single static binary**, no Node dependency.

---

## Status

Unofficial, reverse-engineered, MIT-licensed. Not affiliated with Granola.
APIs may change without notice; pin to a version that works for you and
test before upgrading.

Maintainers: see [RELEASING.md](RELEASING.md) for the tag/release/Homebrew
tap workflow.

## Related

**[granola-mcp](https://github.com/tmcinerney/granola-mcp)** — *deprecated.* A
Python MCP server that wrapped this CLI as a subprocess. Its four tools now ship
in this binary as `granola mcp` (see [MCP server](#mcp-server)), which removes
the Python runtime, the subprocess hop, and the version-compatibility contract
between the two packages.

Tool names are unchanged. Arguments are flatter: the Python server wrapped them
in a required `params` object, so `{"params": {"limit": 5}}` becomes
`{"limit": 5}`.

---

## Credits

- API spec and original CLI:
  [magarcia/granola-cli](https://github.com/magarcia/granola-cli)
- Credential-storage fix:
  [@beaulebens](https://github.com/beaulebens) in PR #6
