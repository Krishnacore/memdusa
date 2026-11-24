# Cursor

Cursor gets memdusa hooks, an MCP server entry and one narrow CLI permission. This page covers the Cursor IDE and the `cursor-agent` CLI, including print mode.

## Install

```sh
memdusa install cursor
```

| Scope | Hooks and MCP | CLI permission file |
|---|---|---|
| `user` (default) | `~/.cursor/hooks.json`, `~/.cursor/mcp.json` | `~/.cursor/cli-config.json` |
| `project` | `.cursor/hooks.json`, `.cursor/mcp.json` | `.cursor/cli.json` |

The MCP entry uses Cursor's `stdio` type and runs `memdusa mcp --agent cursor`. The installer also adds `Mcp(memdusa:*)` to `permissions.allow` in the CLI config. That one entry lets print mode call memdusa's own MCP tools without an interactive approver. It does not widen shell, file, web or any other MCP server permission, and your other allow/deny entries are preserved.

## Trust and first run

Untrusted folders do not start in print mode. Trust the folder interactively, or pass `--trust` to `cursor-agent`. The `--approve-mcps` flag auto-approves the MCP **server**; the `Mcp(memdusa:*)` permission covers memdusa's **tools**. Shell commands still follow Cursor's own policy, so automation that runs them needs `--force` or an allowlist.

## What each hook does

| Hook | What memdusa does |
|---|---|
| `sessionStart` | Injects the brief as `additional_context`. It is fire-and-forget, with a short budget so a stalled hook cannot linger into the session. |
| `beforeSubmitPrompt` | Journals the prompt. Cursor cannot inject context from this hook, so no memories are returned here. |
| `afterFileEdit`, `afterShellExecution` | Journal changed files and shell commands. |
| `stop` | Asks for a checkpoint through `followup_message`. Records the agent's last words from the session transcript. |
| `preCompact` | Writes a fallback checkpoint if files changed without one. |
| `sessionEnd` | Writes a fallback checkpoint. In headless mode this is also where memdusa reads the user's prompt and the agent's last words from the transcript. |

Cursor's hook handlers sit directly in each event list in `hooks.json`, rather than in nested groups like Claude Code's. The installer keeps your own handlers in those lists.

## Known limitations

- `cursor-agent -p` fires only `sessionStart`, the shell/edit hooks and `sessionEnd`. The prompt and last words are recovered from the transcript at session end.
- Cursor runs Claude Code hooks by default ("Third-Party Imports"). If you installed both, turn that setting off or uninstall one set, or events are recorded twice. `memdusa install cursor` warns you when it detects Claude Code hooks.
- Cursor ignores every hook in `hooks.json` when a command contains `://`. The installer rewrites `sqlite://…` into the equivalent `sqlite:<path>` form for you. A Postgres URL cannot be written there: the installer warns you to use `memdusa login <server>` (stored in `~/.memdusa`) or a SQLite database instead.
- Relevant memories are not injected on `beforeSubmitPrompt`; the brief at session start is the main context that Cursor receives.

## Verify it works

```sh
memdusa status
memdusa resume
cat ~/.cursor/mcp.json                       # memdusa entry
cat ~/.cursor/cli-config.json                # Mcp(memdusa:*) permission
```

Then start a Cursor session in a trusted folder. Check that the brief appears and that a small edit reaches the journal (`memdusa recall` shows stored memories; recent activity appears in `memdusa resume`). In print mode, add `--trust` and `--approve-mcps` on the launch line.

## Uninstall

```sh
memdusa uninstall cursor
```

Use `--scope project` if you installed that way. Uninstall removes memdusa's hooks, its MCP entry and exactly the `Mcp(memdusa:*)` permission it added. Other settings stay as they were.

See also: [Harnesses overview](index.md), [Hook events](../reference/hooks.md), [Configuration reference](../reference/configuration.md).
