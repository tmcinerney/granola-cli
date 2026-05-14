# Granola plugin for Claude Code

Four skills for working with your Granola meeting notes from Claude Code:

- **`granola-recent`** — list recent meetings (today / yesterday / last N days)
- **`granola-notes`** — pull the markdown notes for a specific meeting
- **`granola-export`** — save a meeting's notes (and optionally transcript) to a file
- **`granola-search`** — find meetings by title

## Prerequisites

This plugin requires the `granola` CLI binary on your `$PATH`. Install one of:

```sh
brew install tmcinerney/tap/granola-cli       # macOS/Linux via Homebrew
cargo install granola-cli                     # via Cargo
```

Or download a pre-built binary from
[Releases](https://github.com/tmcinerney/granola-cli/releases).

Then sign in once:

```sh
granola auth login
granola auth status   # should print "Authenticated and validated"
```

## Usage

The skills auto-trigger on natural language. Try:

> *"What meetings did I have yesterday?"*
> *"Pull the notes from my design review with Acme last week"*
> *"Export today's standup to ~/notes/standup.md"*
> *"Search my recent meetings for anything about pricing"*

## Why this exists

Granola desktop ≥7.162 broke the upstream JS CLI by moving credentials to a
new file format. This plugin uses the [Rust port](https://github.com/tmcinerney/granola-cli)
that incorporates the fix natively. See the main README for details.
