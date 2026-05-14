---
description: Fetch the markdown notes from a specific Granola meeting. Use when the user asks "show me the notes from <meeting>", "what did we discuss in <meeting>", "pull up the standup from yesterday", or otherwise asks for a meeting's content by title, date, or attendees.
---

# granola-notes

Fetch and display meeting notes as markdown.

## Prerequisite check

Run `command -v granola` first. If missing:

> Stop. Tell the user: "The `granola` CLI isn't installed. Install with `brew install tmcinerney/tap/granola-cli`, then run `granola auth login` once."

## Workflow

1. **Find the meeting.** Run `granola meeting list --since 14d --search "<query>" --output json` (widen `--since` if the user mentions an older meeting). The `--search` flag does a case-insensitive substring match on titles.

   - If 0 results → tell the user "Couldn't find a meeting matching `<query>`. Want to widen the date range or try different wording?"
   - If 1 result → use its `id`. Continue.
   - If >1 results → **do not guess**. List the candidates as a table (Date, Title, short ID) and ask the user which one. After they pick, continue.

2. **Fetch the notes.** Run `granola meeting notes <id> --output markdown`. The CLI renders the ProseMirror notes doc to markdown.

   If the output is empty, fall back to `granola meeting notes <id> --output json` and pull `notes_markdown` from the response — some accounts store markdown directly without a ProseMirror tree.

3. **Display.** Print the title as an `H1`, then the notes verbatim. No editorialization unless the user explicitly asks for a summary.

## When the user is vague

If the user says "the standup" or "my meeting with Sarah" without a date, default `--since` to `14d` and search by the keyword(s). If the user says "yesterday's standup", pass `--since yesterday --until today` and `--search standup`.

## Error handling

JSON errors from the CLI (`{"error": {"code": ..., "message": ...}}`) — relay to the user. Don't retry silently on `unauthenticated` or `stale_credentials`; those need a manual `granola auth login`.
