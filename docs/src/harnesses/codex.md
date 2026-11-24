# Codex CLI

OpenAI's Codex CLI gets memdusa hooks in its own `hooks.json` and an MCP server in its `config.toml`. This page covers interactive Codex and headless `codex exec`.

## Install

```sh
memdusa install codex
```

| Scope | Hooks file | MCP config |
|---|---|---|
| `user` (default) | `~/.codex/hooks.json` | `~/.codex/config.toml` |
| `project` | `.codex/hooks.json` | `.codex/config.toml` |

The installer adds a `[mcp_servers.memdusa]` table with the memdusa command and arguments, `default_tools_approval_mode = "approve"` and the four coordination variables Codex does not pass through on its own: `MEMDUSA_RUN`, `MEMDUSA_TASK`, `MEMDUSA_PARENT` and `MEMDUSA_STEP_LEASE_SECS`. Other keys and sub-tables in that config are left as written. `--dry-run` prints the result without writing.

## Trust and first run

Codex runs new or changed hooks only after you trust them. Open Codex and approve them in `/hooks`. For automation, pass `--dangerously-bypass-hook-trust`. A project `.codex/config.toml` also requires the project to be trusted: accept the trust prompt or run `codex --trust`.

## What each hook does

| Hook | What memdusa does |
|---|---|
| `SessionStart` | Injects the resume brief. It re-fires with source `compact` after compaction, so the brief is re-injected. |
| `UserPromptSubmit` | Journals the prompt and injects relevant memories. |
| `PostToolUse` (`Bash\|apply_patch`) | Journals shell commands and turns `apply_patch` output into a list of changed files. |
| `Stop` | Asks for a checkpoint once when files changed and the last checkpoint is older than 15 minutes; records the agent's last words. |
| `PreCompact` | Writes a fallback checkpoint if files changed without one. |
| `SessionEnd` | Writes a fallback checkpoint; Codex allows this hook at most three seconds. |

Codex has no `PostCompact` hook, so it does not record the harness-generated compaction summary that Claude Code captures. See [Hook events](../reference/hooks.md) for the canonical event format.

## Headless and delegated workers

`default_tools_approval_mode = "approve"` lets headless tool calls reach memdusa's own MCP tools without an interactive approver. Those tools only write to memdusa's store, so approving them grants no access to arbitrary files or commands.

`codex exec` reads stdin when it is not a terminal. Close it in scripts or it waits:

```sh
MEMDUSA_RUN=0198abcdef MEMDUSA_TASK=invoices \
  codex exec "Continue your delegated task." < /dev/null
```

Codex does not pass the parent environment to stdio MCP servers, so the installer names the coordination variables explicitly in `config.toml`. See [Multi-agent work and delegation](../guides/multi-agent.md).

## Known limitations

- Hooks do not run until you trust them, even after a successful install.
- No `PostCompact` summary is recorded; a `PreCompact` fallback checkpoint covers the loss of context instead.
- Only the four variables listed above are forwarded to the MCP server from Codex's environment.

## Verify it works

```sh
memdusa status
memdusa resume
grep -A6 '\[mcp_servers.memdusa\]' ~/.codex/config.toml   # installed table
```

Then start a new Codex session, trust the hooks in `/hooks`, and check that the brief appears. A small edit followed by `memdusa recall` confirms that file changes reach the journal.

## Uninstall

```sh
memdusa uninstall codex
```

Add `--scope project` to match a project install. Uninstall removes memdusa's hooks from `hooks.json` and the `[mcp_servers.memdusa]` table from `config.toml`, keeping other entries.

See also: [Harnesses overview](index.md), [CLI reference](../reference/cli.md), [Durable run protocol](../reference/durable-runs.md).
