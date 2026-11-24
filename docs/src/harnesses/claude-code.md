# Claude Code

Claude Code is the harness with the fullest memdusa integration: seven hooks in its settings file plus an MCP server. Use this page if you work in Claude Code interactively or run it headless with `claude -p`.

## Install

```sh
memdusa install claude-code
```

The default `user` scope applies to every project on this machine. Two other scopes are available:

| Scope | Hooks file | Who it affects |
|---|---|---|
| `user` (default) | `~/.claude/settings.json` | Every project on this machine |
| `project` | `.claude/settings.json` | This repository, shared with the team |
| `local` | `.claude/settings.local.json` | This repository, just you |

Register the MCP server for the same scope. The installer calls `claude mcp add memdusa --scope <scope> -- <path-to-memdusa> mcp --agent claude-code`. If the `claude` CLI is not installed, it prints that command so you can run it later.

Installation merges memdusa's hooks into your existing settings, keeps other hooks intact, removes older memdusa entries instead of duplicating them, and backs the file up to `<settings-file>.memdusa-backup` first. Use `--dry-run` to print the resulting settings without writing.

## Trust and first run

Claude Code needs no separate hook allowlisting step; the hooks live in its own settings file. Start a new session, or resume an existing one, after installing. The session receives the brief automatically.

## What each hook does

| Hook | What memdusa does |
|---|---|
| `SessionStart` | Injects the resume brief on startup, `--resume`, `/clear`, after compaction and on fork, so Claude continues where any agent left off. |
| `UserPromptSubmit` | Journals what you asked for and injects up to five relevant memories. |
| `PostToolUse` | Journals file changes from `Edit`, `MultiEdit`, `Write` and `NotebookEdit`, and shell commands from `Bash`. |
| `Stop` | If files changed and the last checkpoint is older than 15 minutes, asks Claude once to save a checkpoint before finishing. Also records the agent's last words. |
| `PreCompact`, `SessionEnd` | Fallback: if files changed but no checkpoint was saved, writes an automatic one with the last request, changed files, the previous plan carried forward and the agent's last message. |
| `PostCompact` | Records the compaction summary Claude Code generated, up to 4000 characters. |

Hooks give up well inside Claude Code's timeout (a typical call takes about 15 ms). Any error is logged to stderr and the hook exits 0. In remote mode the hooks call the team server instead of the local database. See [Hook events](../reference/hooks.md) for the event format.

The checkpoint reminder and last-words rules are shared across harnesses; see [Core concepts](../getting-started/concepts.md#what-the-brief-contains).

## Control what gets captured

| Variable | Effect |
|---|---|
| `MEMDUSA_NO_PROMPTS=1` | Do not journal user prompts. |
| `MEMDUSA_RECALL=off` | Do not inject memories into prompts. |
| `MEMDUSA_STOP_GUARD_MINS=0` | Turn off the checkpoint reminder. The default is 15 minutes. |

The full list is in the [Configuration reference](../reference/configuration.md).

## Known limitations

- The reminder fires at most once per session per 15-minute window. If memdusa's MCP tools are unavailable, the agent is told to stop rather than loop.
- A global memory is private to its author on a team server; project data is shared with users who have access to that project.
- The generated hook commands use the absolute path of the installed binary. Use `--bin memdusa` when you commit a project-scoped settings file for teammates.

## Verify it works

```sh
claude mcp get memdusa                       # MCP server registered
memdusa status                               # local or remote mode, reachability
memdusa resume                               # the brief the session receives
```

Then start a new Claude Code session. Confirm that the brief appears, make a small edit, and check that the journal records it with `memdusa recall` or in the resume brief's recent activity.

## Uninstall

```sh
memdusa uninstall claude-code
```

Add `--scope project` or `--scope local` if you installed with that scope. Uninstall removes memdusa's hooks and runs `claude mcp remove memdusa --scope <scope>`; your other settings and hooks stay as they were.

See also: [Harnesses overview](index.md), [Multi-agent work and delegation](../guides/multi-agent.md), [MCP tools](../reference/mcp.md).
