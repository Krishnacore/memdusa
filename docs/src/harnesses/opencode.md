# OpenCode

OpenCode uses a TypeScript plugin rather than JSON hooks. The plugin calls `memdusa hook` with memdusa's canonical payloads, so redaction, the offline queue and remote mode work the same as in other harnesses.

## Install

```sh
memdusa install opencode
```

The installer runs `opencode --version` and picks the plugin for the installed line. OpenCode 1.x (stable) and 2.x have different plugin APIs and MCP setup. If `opencode` is not on your `PATH`, the installer assumes 1.x. Set `MEMDUSA_OPENCODE_API=1` or `MEMDUSA_OPENCODE_API=2` to override detection. Re-run the install after switching OpenCode versions; the installer removes a plugin left over from the other line.

| Scope | Plugin, 1.x | Plugin, 2.x | MCP |
|---|---|---|---|
| `user` (default) | `~/.config/opencode/plugins/memdusa.ts` | `~/.config/opencode/plugins/memdusa/index.ts` | 1.x: `~/.config/opencode/opencode.json(c)`; 2.x: global `opencode` config |
| `project` | `.opencode/plugins/memdusa.ts` | `.opencode/plugins/memdusa/index.ts` | 1.x: project `opencode.json(c)`; 2.x: project `opencode` config |

For 1.x, the installer adds an `mcp.memdusa` entry to `opencode.json` or `opencode.jsonc`, whichever exists. If that file contains comments, the installer leaves it alone and prints the entry for you to paste in. For 2.x, it runs `opencode mcp add memdusa -- <path-to-memdusa> mcp --agent opencode` (with `--global` for user scope) after checking `opencode mcp list`.

## Trust and first run

Restart OpenCode to load the plugin. There is no separate hook allowlist step.

OpenCode 2.x runs plugins and MCP servers in a shared background service that does not inherit the environment of the shell you started `opencode` from. Configure memdusa at install time instead: pass `--database-url` to `memdusa install opencode`, or sign in with `memdusa login` (saved in `~/.memdusa` and read by every hook). Do not rely on shell environment variables for this database or token.

A delegated worker must launch with `opencode run --standalone`: that starts a private service which does inherit the environment, so its MCP tools can see `MEMDUSA_RUN` and `MEMDUSA_TASK`. See [Multi-agent work and delegation](../guides/multi-agent.md).

## What each hook does

| Plugin event | What memdusa does |
|---|---|
| First prompt of a session (new, resumed or forked) | Loads the resume brief as session context. |
| Every prompt | Journals the prompt and injects relevant memories. |
| Tool execution after | Journals file edits and shell commands. |
| Compaction | Writes a fallback checkpoint if files changed without one; keeps the brief available after compaction. |
| Idle (`session.idle` in 1.x, `experimental.idle` in 2.x) | Journals the agent's last message and asks for a checkpoint if files changed, at most once per window. |

OpenCode has no session-end hook. No `session-end` event or session-end fallback checkpoint is recorded there.

## Known limitations

- No `session-end` hook: last words are captured when the agent goes idle, not when the session closes.
- The 1.x and 2.x plugin APIs differ. The installer detects and replaces the right one, but a version switch needs a re-install and a restart.
- In 2.x, shell environment variables are not visible to the background service. Use `--database-url` at install time or `memdusa login`, and `--standalone` for delegated workers.
- Plugin failures are written to `$MEMDUSA_HOME/logs/plugin.log` (default `~/.memdusa/logs/plugin.log`). A hook that cannot run is logged there and never fails the agent's turn.

## Verify it works

```sh
memdusa status
memdusa resume
opencode mcp list                            # 2.x: memdusa should be listed
cat ~/.memdusa/logs/plugin.log               # empty or no recent errors
```

Restart OpenCode and start a session. The brief should appear as session context; after an edit, check the journal through `memdusa resume`. For 1.x, check for the `mcp.memdusa` entry in `opencode.json(c)` instead of using `opencode mcp list`.

## Uninstall

```sh
memdusa uninstall opencode
```

Use the same scope as the install. Uninstall removes the plugin file (and its directory for 2.x). OpenCode has no `mcp remove` command, so for 2.x the installer prints an instruction to delete `memdusa` from `mcp.servers` in your `opencode.json(c)` by hand. Restart OpenCode afterwards.

See also: [Harnesses overview](index.md), [Hook events](../reference/hooks.md), [Multi-agent work and delegation](../guides/multi-agent.md).
