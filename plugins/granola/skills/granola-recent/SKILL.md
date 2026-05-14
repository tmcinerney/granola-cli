---
description: List the user's recent Granola meetings (today, yesterday, last N days). Use when the user asks "what meetings did I have", "show recent meetings", "today's meetings", "meetings this week", or similar lookups by recency.
---

# granola-recent

List recent Granola meetings via the `granola` CLI.

## Prerequisite check

Before doing anything, run `command -v granola` via Bash. If it returns nothing:

> Stop. Tell the user: "The `granola` CLI isn't installed. Install with `brew install tmcinerney/tap/granola-cli` (or `cargo install granola-cli`), then run `granola auth login` once."

## How to invoke the CLI

Always pass `--output json` so you get structured data. Always pass `--since` and `--limit` rather than computing dates yourself — the CLI parses `today`, `yesterday`, `7d`, `2h`, `30m`, and ISO dates.

```sh
granola meeting list --since <spec> --limit <N> --output json
```

Default to `--limit 20` unless the user implies they want more or fewer.

Map common phrasings to `--since`:

| User says | `--since` |
|---|---|
| "today's meetings", "today" | `today` |
| "yesterday" | `yesterday` |
| "this week", "the last week" | `7d` |
| "the last two weeks" | `14d` |
| "in the last hour" | `1h` |
| "recent meetings" (no qualifier) | `7d` |

The CLI returns an array of meeting objects. Each has `id`, `title`, `created_at`, `updated_at`, `_origin` (`"owned"` or `"shared"`), and Google Calendar metadata in `google_calendar_event`.

## Presenting results

Format as a markdown table with columns: **Date**, **Title**, **ID (short)**. Use the first 8 characters of `id` as the short ID — the user can copy it into other commands. If `_origin` is `"shared"`, append " *(shared)*" to the title.

Sort by `updated_at` descending (the CLI already does this, but be explicit).

If the list is empty, say "No meetings found for that range" — don't fabricate.

## Error handling

If the CLI's stdout JSON contains `{"error": ...}` (or the bash command fails), report the error message to the user verbatim. Common errors:

- `unauthenticated` → "You're not logged in. Run `granola auth login`."
- `stale_credentials` → "Granola rejected your credentials. Try `granola auth login` again."
