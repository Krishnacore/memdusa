# pi

The pi coding agent loads memdusa as a TypeScript extension. The extension translates pi's events into `memdusa hook` calls, so all hook logic stays the same as in other harnesses.

## Install

```sh
memdusa install pi
```

| Scope | Extension | MCP config |
|---|---|---|
| `user` (default) | `~/.pi/agent/extensions/memdusa.ts` | `~/.pi/agent/mcp.json` |
| `project` | `.pi/extensions/memdusa.ts` | `.pi/mcp.json` |

The MCP entry runs `memdusa mcp --agent pi` and sets `exposure: "direct"`. pi hides MCP tools behind its "code mode" by default; direct exposure makes memdusa's tools available to the model. Re-run `memdusa install pi` after upgrading memdusa to refresh the extension.

## Trust and first run

pi loads project extensions in `.pi/` only after you trust the project. Answer the trust prompt, or pass `--approve` / `-a` for one run. User scope has no project trust step and works everywhere.

## What each hook does

| pi event | What memdusa does |
|---|---|
| `session_start` | Resets the brief flag for a new, resumed or forked session. |
| `before_agent_start` (first user message) | Loads the resume brief and journals the prompt; injects relevant memories. |
| `tool_result` | Journals file edits (`write`, `edit`) and shell commands (`bash`). |
| `agent_end` | Remembers the run's last assistant message. |
| `agent_before_settle` | Journals that last message and asks for a checkpoint if files changed, at most once per session per window. |
| `session_before_compact` | Writes a fallback checkpoint if files changed without one. |
| `session_shutdown` | Records the session end and writes a fallback checkpoint if needed. |

## Known limitations

- A project extension requires project trust; an untrusted project loads nothing.
- MCP tools need the `exposure: "direct"` setting the installer adds. If you edit `mcp.json` by hand, keep it.
- pi runs hooks from a long-lived process. For delegated workers and a fixed database, configure it at install time with `--database-url` or `memdusa login` rather than relying on shell environment variables. See [Multi-agent work and delegation](../guides/multi-agent.md).
- Extension errors are logged to `$MEMDUSA_HOME/logs/plugin.log` (default `~/.memdusa/logs/plugin.log`). They do not fail the agent's turn.

## Verify it works

```sh
memdusa status
memdusa resume
cat ~/.pi/agent/mcp.json                     # memdusa entry with exposure: direct
cat ~/.memdusa/logs/plugin.log               # empty or no recent errors
```

Start a pi session in a trusted project. The brief should appear on the first user message; after an edit, check recent activity in `memdusa resume`.

## Uninstall

```sh
memdusa uninstall pi
```

Use `--scope project` to match a project install. Uninstall removes the extension file and the memdusa MCP entry, leaving your other pi configuration untouched.

See also: [Harnesses overview](index.md), [Hook events](../reference/hooks.md), [Configuration reference](../reference/configuration.md).
