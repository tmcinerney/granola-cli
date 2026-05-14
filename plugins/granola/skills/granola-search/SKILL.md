---
description: Search the user's Granola meetings by title. Use when the user asks "find meetings about <topic>", "search for <keyword>", "do I have any meetings on <subject>", or otherwise asks to locate meetings by content keywords.
---

# granola-search

Search Granola meetings by title keyword.

## Prerequisite check

Run `command -v granola` first. If missing:

> Stop. Tell the user: "The `granola` CLI isn't installed. Install with `brew install tmcinerney/tap/granola-cli`, then run `granola auth login` once."

## How to search

The CLI's `--search` flag does case-insensitive substring matching against meeting **titles**. Content search isn't supported yet.

```sh
granola meeting list --search "<keyword>" --since <spec> --limit <N> --output json
```

Defaults:

- `--since 60d` for searches (broader than `granola-recent`'s default — users searching usually want history)
- `--limit 30`

If the user mentions a specific time range ("in the last week", "from Q1"), tighten `--since` accordingly.

## Handling no-results

If the title search returns 0 results, **tell the user the search is title-only** and ask if they want to broaden the date range. Don't claim there are no meetings on the topic — there may be matches in the body that we can't see.

Example:

> No meetings with "<keyword>" in the title in the last 60 days. (Note: I can only search titles, not meeting content.) Want me to widen the date range, or try a different keyword?

## Presenting results

Format as a markdown table: **Date**, **Title**, **Origin** (owned/shared), **ID (short, first 8 chars)**.

If the user wants to dig into one, suggest: *"Want me to pull up the notes for one of these?"* — which will route to the `granola-notes` skill.

## Error handling

JSON errors → relay verbatim. `unauthenticated` and `stale_credentials` → ask the user to run `granola auth login`.
