# Harnesses

A harness is the agent tool you work in: Claude Code, Codex, Cursor and the others below. One `memdusa install` command per harness registers memdusa as an MCP server and installs that tool's hooks. After that, memory and checkpoints follow you when you switch tools.

## Support table

| Harness | How memdusa plugs in | Brief at start | Prompts | Edits / commands | Checkpoint reminder | Fallback checkpoint |
|---|---|---|---|---|---|---|
| [Claude Code](claude-code.md) | Hooks in `settings.json` | Yes, also after compaction | Yes | Yes | `Stop` | `PreCompact`, `PostCompact` (summary), `SessionEnd` |
| [Codex CLI](codex.md) | `~/.codex/hooks.json` + `config.toml` | Yes, also after compaction | Yes | Yes (patch → files) | `Stop` | `PreCompact`, `SessionEnd` |
| [Cursor](cursor.md) | `~/.cursor/hooks.json` + `mcp.json` | Yes | Yes | Yes | `stop` (`followup_message`) | `preCompact`, `sessionEnd` |
| [Antigravity CLI](antigravity.md) | `~/.gemini/config/hooks.json` + `mcp_config.json` | Yes | Yes, first `PreInvocation` | Yes | `Stop` (`continue`) | `Stop` when fully idle, once per session (last words from transcript) |
| [Gemini CLI](gemini.md) | `~/.gemini/settings.json` | Yes | Yes (`BeforeAgent`) | Yes | `AfterAgent` (`deny`) | `PreCompress`, `SessionEnd` |
| [OpenCode](opencode.md) | TypeScript plugin in `.opencode/plugins/` (1.x: `memdusa.ts`; 2.x: `memdusa/index.ts`) | Yes, first prompt | Yes | Yes | idle (`session.idle` / `experimental.idle`) | Compaction hook |
| [pi](pi.md) | TypeScript extension in `~/.pi/agent/extensions/` | Yes, first run | Yes | Yes | `agent_before_settle` | `session_before_compact`, `session_shutdown` |
| [Hermes Agent](hermes.md) | Shell hooks in `~/.hermes/config.yaml` | Yes, first turn | Yes | Yes | `pre_verify` (after edits only) | `on_session_finalize` |

[Git hooks](git.md) record commits, branch switches and merges. They are separate from harness hooks and work for any agent or human in the repository.

Each harness's payload is translated to memdusa's canonical format on your machine, and memdusa's output is rendered back in that tool's dialect. Remote mode, the offline queue, redaction and code-state capture work the same everywhere.

## Install one harness

```sh
memdusa install claude-code
```

Replace `claude-code` with `codex`, `cursor`, `antigravity`, `gemini`, `opencode`, `pi` or `hermes`. Installation is idempotent: re-running it updates memdusa's entries and keeps your other settings and hooks. JSON settings files are backed up to `<filename>.memdusa-backup` (for example `settings.json.memdusa-backup`) before a change.

The generated hook commands contain the absolute path of your memdusa binary by default. If you commit a project-scoped config for teammates, pass `--bin memdusa` so each person runs the copy on their own `PATH`.

## Common install options

| Option | What it does |
|---|---|
| `--scope <user\|project\|local>` | Selects the config file to edit. `user` (the default) applies to every project on this machine; `project` writes into the repository and is shared with the team; `local` is Claude Code only (`.claude/settings.local.json`). See each harness page for exact paths. |
| `--database-url <URL>` | Writes that URL into every generated hook command, MCP entry and plugin, so they use it instead of the default `~/.memdusa/memdusa.db`. Without it, generated config is unchanged. Accepts `sqlite://…` and `postgres://…`. |
| `--dry-run` | Prints the resulting settings without writing anything. Use it to inspect an install before you run it. |
| `--no-mcp` | Installs hooks only; does not register the MCP server. |
| `--project <PROJECT>` | Overrides the detected project key. |

`--database-url` is useful where a harness runs hooks or MCP servers in a background service that does not inherit your shell environment, and for a worker agent that needs a specific database. The full option list is in the [CLI reference](../reference/cli.md).

## Trust and first run

Most harnesses refuse to run a project's hooks or MCP servers until you approve them once:

| Harness | One-time step |
|---|---|
| Codex | Project hooks run only after trust: approve in `/hooks` (automation: `--dangerously-bypass-hook-trust`). A project `.codex/config.toml` also needs the project trusted (trust prompt or `codex --trust`). |
| Antigravity CLI (`agy`) | Project hooks in `.agents/` run in `agy -p` without a trust prompt. Headless mode soft-denies MCP calls it would have to ask about, so `memdusa install antigravity` adds a PreToolUse hook that auto-approves **only** calls to memdusa's own server. `MEMDUSA_RUN` / `MEMDUSA_TASK` from the launching shell reach the MCP server. |
| Cursor CLI (`cursor-agent`) | Untrusted folders won't start in print mode: trust the folder interactively or pass `--trust`. `--approve-mcps` auto-approves the MCP **server**. `memdusa install cursor` also adds `Mcp(memdusa:*)` to Cursor's CLI permissions so print mode can call memdusa's own MCP tools without an interactive approver. Shell commands still follow Cursor's own policy: use `--force` (or an allowlist) for automation that runs them. |
| pi | Project extensions (`.pi/`) load only for trusted projects: answer the trust prompt, or pass `--approve` / `-a` for one run. |
| Hermes | Shell hooks must be allowlisted: run Hermes once with `--accept-hooks` (or approve at its prompt). `hermes hooks doctor` shows their status. After installing, restart the gateway with `hermes gateway restart`. |
| OpenCode | Restart OpenCode to load the plugin. The installer detects OpenCode 1.x or 2.x and writes the matching plugin; re-run it after switching versions. |
| Gemini CLI | Google stopped serving individual accounts in Gemini CLI in June 2026. Use Antigravity (`agy`, `memdusa install antigravity`) for consumer accounts. |

## Rules that apply everywhere

- The checkpoint reminder fires at most once per session per 15-minute window in every harness, so it cannot trap an agent that has no way to checkpoint. Set `MEMDUSA_STOP_GUARD_MINS=0` to turn it off.
- A hook never breaks a session. Errors are logged to stderr, and the hook exits 0 within the harness's timeout.
- Cursor also runs Claude Code hooks by default ("Third-Party Imports"). If both are installed, turn that off so events are not recorded twice; `memdusa install cursor` warns you.

## Use memdusa without hooks

```sh
memdusa mcp --agent claude-code
```

Any other MCP client can connect this way. Replace `claude-code` with your agent name for attribution. Without hooks, the agent can still use MCP tools but nothing is captured automatically. See [MCP tools, resources and prompts](../reference/mcp.md).

## Uninstall

```sh
memdusa uninstall claude-code
```

Use the same harness and scope as the install. Uninstall removes memdusa's hooks and MCP registration and leaves your other settings untouched. For Hermes it removes only its marked YAML blocks, and for Git only its own line in each hook script.

See also: [Hook events reference](../reference/hooks.md), [Multi-agent work and delegation](../guides/multi-agent.md), [CLI reference](../reference/cli.md).
