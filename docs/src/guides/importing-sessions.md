# Importing past sessions

Bring work done before memdusa was installed, or work from a tool without hooks, into memdusa. The importer turns a saved transcript into the events the harness hooks would have recorded, with their original timestamps.

## Import a saved transcript

Run `memdusa import-transcript` from the project directory:

```sh
memdusa import-transcript claude-code --latest
memdusa import-transcript codex ~/.codex/sessions/2026/03/02/rollout-example.jsonl
memdusa import-transcript pi session1.jsonl session2.jsonl --dry-run
```

`--latest` picks the most recent session for the current directory's repository. Otherwise pass one or more transcript files.

| Harness | Value | Where sessions are read from with `--latest` |
|---|---|---|
| Claude Code | `claude-code` | `~/.claude/projects/…` |
| Codex CLI | `codex` | `~/.codex/sessions/…` |
| pi | `pi` | `~/.pi/agent/sessions/…` |

OpenCode transcripts are not supported yet. Use the [OpenCode harness setup](../harnesses/opencode.md) for new work.

## What gets recorded

An imported session becomes the events its hooks would have written: prompts, file changes and commands, each with its original timestamp. A session end event is appended last, so the usual fallback checkpoint is written if files changed. Unrecognized transcript lines are skipped.

The import goes through the same pipeline as live hooks: [secret redaction](redaction.md) applies, remote mode sends the events to the server, and the project key is detected from the directory recorded in the transcript. Pass `--project` to override that detection. See [Project keys and detection](../reference/configuration.md).

Compressed Codex transcripts ending in `.zst` are not supported. Decompress them first with `zstd -d`.

## Check, repeat and skip

- `--dry-run` prints what would be imported (a count of prompts, file changes and commands, the time range, and the project) and writes nothing.
- Import is idempotent. A session that already has events is skipped unless you pass `--force`.
- A transcript with no prompts, file changes or commands is skipped with a message.
- Files that cannot be read or parsed are skipped with a message; the remaining files are still imported.

For all flags, see the [CLI reference](../reference/cli.md). To capture work as it happens instead, install hooks for your [harness](../harnesses/index.md).

See also: [Retention and backups](retention.md), [Hook events](../reference/hooks.md).
