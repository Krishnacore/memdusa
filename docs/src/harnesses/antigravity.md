# Antigravity CLI

Google Antigravity CLI (`agy`) is the successor to Gemini CLI for consumer accounts. memdusa installs its hooks under a single `memdusa` group in Antigravity's hooks file, plus an MCP server and a narrow auto-approval hook.

## Install

```sh
memdusa install antigravity
```

| Scope | Hooks | MCP config |
|---|---|---|
| `user` (default) | `~/.gemini/config/hooks.json` | `~/.gemini/config/mcp_config.json` |
| `project` | `.agents/hooks.json` | `.agents/mcp_config.json` |

The installer owns the whole `memdusa` group in the hooks file and leaves your other hooks alone. The group contains the session, prompt, tool and stop handlers, plus a `PreToolUse` handler matched to `call_mcp_tool|mcp_memdusa_.*` that auto-approves calls to memdusa's own MCP server. It does not approve calls to any other server.

## Trust and first run

Project hooks in `.agents/` run in `agy -p` without a trust prompt. In headless mode Antigravity soft-denies MCP calls it would have to ask about, so the auto-approval hook is what lets the agent call memdusa's `checkpoint` tool. You do not need `--dangerously-skip-permissions` for memdusa.

Antigravity 1.5.14 and later also soft-deny the agent's own file writes and commands in `agy -p` unless an
allow-rule covers them. That is Antigravity's permission system, not memdusa's: for unattended runs, pass
session-only rules with `--settings`. Antigravity matches the *resolved* path, so on macOS a folder under
`/var/folders/…` must be given as `/private/var/folders/…`:

```sh
agy --settings '{"permissions":{"allow":["write_file(/path/to/repo)","command(*)"]}}' -p "your task"
```

`MEMDUSA_RUN` and `MEMDUSA_TASK` from the launching shell do reach the MCP server, so no `env` entry is needed in `mcp_config.json`.

## What each hook does

| Hook | What memdusa does |
|---|---|
| `SessionStart` | Injects the resume brief. |
| `PreInvocation` | Journals the user's input on the first invocation of a turn and injects relevant memories. Later invocations of the same turn are ignored. |
| `PostToolUse` (`run_command`, `write_file`, `write_to_file`, `replace_file_content`, `multi_replace_file_content`) | Journals shell commands and changed files. |
| `Stop` | Asks for a richer checkpoint through `decision: continue`, reads the agent's last words from the session transcript, and writes one fallback checkpoint when the agent goes fully idle after editing files. |
| `PreToolUse` (`call_mcp_tool`, `mcp_memdusa_*`) | Auto-approves only memdusa's own MCP server in headless mode. |

Antigravity's `Stop` payload has no assistant text. The adapter reads the transcript path from `transcriptPath` and uses the last `PLANNER_RESPONSE` step with content as the agent's last words, so tool results are not mistaken for a final message.

## Known limitations

- Antigravity has no session-end or compaction hook. No session-end event or compaction fallback checkpoint is recorded.
- The fallback checkpoint is written at most once per session, when the agent goes fully idle after edits. The richer `Stop` reminder still asks for a checkpoint when memdusa's MCP tools are available.
- Antigravity renamed its write tool between releases; the adapter accepts both `write_file` and `write_to_file` argument shapes.
- Gemini CLI remains available for enterprise and paid-API users. Consumer accounts moved to Antigravity in June 2026.

## Verify it works

```sh
memdusa status
memdusa resume
cat ~/.gemini/config/hooks.json              # the memdusa group
cat ~/.gemini/config/mcp_config.json         # the memdusa server
```

Start an `agy` session (interactive or `agy -p`) and check that the brief appears. In headless mode, make a small edit and confirm that a checkpoint can be saved without `--dangerously-skip-permissions`.

## Uninstall

```sh
memdusa uninstall antigravity
```

Add `--scope project` for a project install. Uninstall removes the entire `memdusa` group from the hooks file, including the auto-approval handler, and removes the MCP entry. Other hooks and settings are left as they were.

See also: [Harnesses overview](index.md), [Gemini CLI](gemini.md), [Hook events](../reference/hooks.md).
