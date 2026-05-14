---
description: Export a Granola meeting (notes, optionally transcript) to a local markdown file. Use when the user says "save this meeting to <path>", "export my standup notes", "write the design review to a file", or otherwise asks to persist meeting content to disk.
---

# granola-export

Save a meeting's content as a markdown file on disk.

## Prerequisite check

Run `command -v granola` first. If missing:

> Stop. Tell the user: "The `granola` CLI isn't installed. Install with `brew install tmcinerney/tap/granola-cli`, then run `granola auth login` once."

## Workflow

1. **Find the meeting.** If the user gave a specific meeting ID, skip to step 2. Otherwise, run `granola meeting list --since 14d --search "<query>" --output json` and resolve as in `granola-notes`:

   - 1 result → use it
   - 0 results → ask for a wider date range or different terms
   - >1 results → show the user the candidates as a table and ask which one

2. **Determine the output path.** If the user named a path, use it. Otherwise default to `./<slugified-title>.md` in the current working directory. Slugify by lowercasing, replacing non-alphanumeric runs with `-`, and trimming dashes from the ends.

3. **Determine whether to include the transcript.** Include it (`--include-transcript`) only if the user explicitly asked for "transcript", "full transcript", "everything", or similar. Default is notes-only.

4. **Run the export.**

   ```sh
   granola meeting export <id> --output-file <path>
   # or with transcript:
   granola meeting export <id> --output-file <path> --include-transcript
   ```

   The CLI writes the file and prints `wrote <path>` to stderr. Confirm to the user: "Saved to `<path>` (notes only)" or "Saved to `<path>` (notes + transcript)".

## Confirmation before overwriting

If the path already exists, **ask before overwriting**. Use the Bash command `test -e <path> && echo EXISTS` to check. If `EXISTS`, ask: "`<path>` already exists. Overwrite?"

## Error handling

Relay JSON errors from the CLI verbatim. If the path's parent directory doesn't exist, mention that specifically rather than just the underlying I/O error.
