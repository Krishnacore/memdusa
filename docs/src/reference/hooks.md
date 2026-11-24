# Hook events

Hooks are the small programs a harness runs at points in a session. memdusa uses them to inject the resume brief, journal activity and write fallback checkpoints, without the agent having to remember. This page describes the events, the canonical payload they share and what each event records.

## Events and CLI names

`memdusa hook <event>` reads the harness's JSON on stdin. Each event has a CLI name and a time budget:

| Event | CLI name | Budget | Queueable |
|---|---|---|---|
| `SessionStart` | `session-start` | 10 s | no |
| `UserPromptSubmit` | `user-prompt-submit` | 5 s | yes |
| `PostToolUse` | `post-tool-use` | 5 s | yes |
| `Stop` | `stop` | 5 s | no |
| `PreCompact` | `pre-compact` | 10 s | yes |
| `PostCompact` | `post-compact` | 10 s | yes |
| `SessionEnd` | `session-end` | 10 s | yes |
| `Git` | `git` | 3 s | yes |
| `McpApproval` | `mcp-approval` | 5 s | no |

A hook gives up well inside its budget and exits 0: any error is logged to stderr and never breaks the session. `SessionStart` uses a 4-second budget for the brief itself (longer than the brief's 3-second semantic-search cap), so a slow embedding API produces a lexical brief rather than no brief.

The event set differs by harness. For example, Antigravity has no session-end or compaction hooks, and OpenCode's plugin calls `memdusa hook` directly with canonical payloads. See the [harness overview](../harnesses/index.md).

## The canonical payload

Every harness has its own hook dialect. On your machine, an adapter translates the native payload into one canonical, Claude Code–shaped format and renders the output back in that harness's dialect. The hook logic is identical in local mode and on a team server.

The canonical input is a JSON object. memdusa reads these fields and ignores the rest:

| Field | Used by | Meaning |
|---|---|---|
| `session_id` | all | Session id; groups events and identifies the worker's lease owner. |
| `cwd` | events that read git state | Working directory. |
| `source` | `SessionStart` | `startup`, `resume`, `clear` or `compact`. |
| `prompt` | `UserPromptSubmit`, `SessionEnd` | The user's request. |
| `tool_name`, `tool_input` | `PostToolUse`, `McpApproval` | Tool that ran and its arguments. |
| `stop_hook_active` | `Stop` | True when the agent is already continuing because of a Stop hook. |
| `last_assistant_message` | `Stop`, `SessionEnd` | The agent's final message of the turn. Adapters normalize other harnesses' versions of this. |
| `fully_idle` | `Stop` | Antigravity's `fullyIdle`: the agent is completely finished. |
| `compact_summary` | `PostCompact` | Summary the harness generated. |
| `trigger` | `PreCompact`, `PostCompact` | `manual` or `auto`. |
| `reason` | `SessionEnd` | Why the session ended. |
| `git_event`, `content`, `data` | `Git` | `commit`, `checkout` or `merge`; a one-line summary; structured facts (sha, branch, files). |

In remote mode, a hook sends `POST /v1/hooks/{event}` with the project key detected on the client (the server cannot see its git repo), the hook config (`agent`, `journal_prompts`, `stop_guard_mins`, `recall`), the raw `input`, client code state when needed, the original event time and the delegated `task`, `parent` and `run`. A `session_coord` flag tells the server to recover the task from the session when the environment is shared, as with Hermes' gateway. See [Multi-agent work and delegation](../guides/multi-agent.md).

## What each event records

- **`SessionStart`** journals `session_start` with its source (including after `/clear` and compaction). For a worker it also journals a `delegated_session` event with task, parent and harness, and claims its run step with a 15-minute lease. It injects the resume brief; after `/clear` or compaction the same brief is injected again. The claim notice, the worker's stored assignment and any stale-lease warning appear at the top of that brief.
- **`UserPromptSubmit`** journals the prompt as a short line with the full text (up to 4,000 characters) in structured data. It then searches the project's memories plus visible globals and injects up to 5 matching ones in a `<memdusa-recall>` block. `--no-prompts` / `MEMDUSA_NO_PROMPTS` stops the journaling; `MEMDUSA_RECALL=off` stops the injection. See [Memory hygiene](../guides/memory-hygiene.md).
- **`PostToolUse`** journals file changes and shell commands. Claude Code's matcher is `Edit|MultiEdit|Write|NotebookEdit|Bash`; other harnesses map their own edit and command tools to the same canonical events. This is also a sign of life that renews a worker's lease.
- **`Stop`** records the agent's last words (`last_assistant_message`, up to 1,500 characters, skipped when identical to the session's previous one) as an `agent_message` event. If files changed and the last checkpoint is older than 15 minutes, it asks the agent once to save a checkpoint (`MEMDUSA_STOP_GUARD_MINS`, `0` turns the reminder off). The reminder fires at most once per session per window. With Antigravity's `fully_idle`, a fully idle stop also writes a fallback checkpoint.
- **`PreCompact`** writes an automatic fallback checkpoint if the agent did not save one (last request, changed files, the previous plan carried forward, the agent's last message) and journals a `compaction` event.
- **`PostCompact`** records the summary the harness generated (`compact_summary`, up to 4,000 characters).
- **`SessionEnd`** journals `session_end` and writes the same fallback checkpoint if files changed. It may also pick up a prompt or last message the harness did not report earlier. It does **not** finish a delegated task: the worker's lease simply expires, which is what stopping mid-task should look like. Completion is explicit with `task_complete`.
- **`Git`** records commits, branch switches and merges whoever made them, a human or any agent. `memdusa install git` installs these hooks in `git rev-parse --git-path hooks`, so `core.hooksPath` (for example Husky) is respected. Existing hooks are kept. They run in the background and never block or fail a git command.
- **`McpApproval`** auto-approves calls to memdusa's own MCP server in Antigravity's headless mode. Every other tool falls back to the harness's normal policy.

Every worker event except `SessionStart`, `SessionEnd`, `Git` and `McpApproval` renews the step lease. That includes prompts, tool use and stop.

The brief shows the latest agent message and compaction summary under "Last words from the previous session", but only when they are newer than the latest checkpoint. See [Core concepts](../getting-started/concepts.md).

## Offline queue and spool

When a remote server is unreachable, no event is lost:

- Write-only events (`UserPromptSubmit`, `PostToolUse`, `PreCompact`, `PostCompact`, `SessionEnd` and `Git`) are appended to `$MEMDUSA_HOME/spool/queue.jsonl` (default `~/.memdusa/spool/`). `SessionStart` and `Stop` produce output only useful in the moment, so they are not queued.
- The next hook that reaches the server replays the backlog first, in one `POST /v1/hooks/batch` request, sorted by when each hook fired and with the original timestamps.
- `SessionStart` serves the last cached brief, marked as possibly out of date.
- `memdusa status` reports whether the server is reachable and how many events are queued.
- Rejected requests (revoked key, bad input) are not queued. Delivery is at-least-once, so a replayed event may be recorded twice if the connection drops after the server accepted it.
- The queue stops accepting events at 20 MB. Concurrent hook processes append to the same file safely; only one process flushes at a time, and pending files are deleted only after the server confirms.

Before v0.1.9, `memdusa sync` flushed this queue by hand. It now runs project sync; the queue still flushes on its own with the next hook that reaches the server. See [Sync and sync mode](../team/sync.md).

See also: [CLI reference](cli.md), [Configuration](configuration.md), [Importing past sessions](../guides/importing-sessions.md).
