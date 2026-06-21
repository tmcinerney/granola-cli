# granola-cli

A Rust CLI for [Granola](https://granola.ai/) meeting notes, with a fix for
the credential-storage break introduced in Granola desktop ≥7.162.

This is a Rust port of [`magarcia/granola-cli`](https://github.com/magarcia/granola-cli)
incorporating the credential-discovery fix from
[PR #6](https://github.com/magarcia/granola-cli/pull/6) so authentication
keeps working on current Granola desktop versions.

The repo also serves as a [Claude Code](https://claude.com/claude-code) plugin
marketplace. Install the plugin and you get four skills that wrap the CLI for
agentic use:

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

## Usage

```sh
granola meeting list --since 7d                    # last 7 days (owned + shared)
granola meeting list --since today --output json   # today only, machine-readable
granola meeting list --search "design review"      # title search

granola meeting view <id>                          # metadata
granola meeting notes <id>                         # notes as markdown
granola meeting transcript <id>                    # full transcript
granola meeting export <id> --output-file out.md   # combined export
```

By default, `meeting list` merges your own meetings with meetings shared to
you (a gap in the upstream CLI). Pass `--no-shared` to skip that hop.

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

## Credits

- API spec and original CLI:
  [magarcia/granola-cli](https://github.com/magarcia/granola-cli)
- Credential-storage fix:
  [@beaulebens](https://github.com/beaulebens) in PR #6
