# Hermes Agent

Nous Research's Hermes Agent uses shell hooks in a YAML config file. memdusa merges marked blocks into that file and starts an MCP server that Hermes' gateway keeps running between sessions.

## Install

```sh
memdusa install hermes
```

Hermes has only a user config, so `--scope user` (the default) is the only valid scope. The file is `$HERMES_HOME/config.yaml`, or `~/.hermes/config.yaml` when `HERMES_HOME` is not set.

memdusa inserts its entries between `# >>> memdusa` and `# <<< memdusa` markers, under your existing `hooks:` and `mcp_servers:` keys or as new sections. The rest of the file, including comments, is left byte for byte as it was. An empty value such as `hooks: {}` is expanded into a block, with the original line kept as a comment for uninstall.

### macOS binary staging

Hermes launches hooks from a background daemon. On macOS, dyld can stall until the hook timeout when that daemon executes a binary from a TCC-protected folder (`~/Desktop`, `~/Documents`, `~/Downloads` or iCloud Drive). If your memdusa binary is there, the installer copies it to `~/.memdusa/bin/memdusa` (or `<MEMDUSA_HOME>/bin/memdusa`) and points the hooks at that copy. A symlink does not help, because dyld resolves it to the protected path. A bare command name passed with `--bin` is left as-is.

The installer also warns if the database you pass with `--database-url` is a SQLite file in one of those folders. The default `~/.memdusa/memdusa.db` is fine.

## Trust and first run

Hermes runs only shell hooks that are allowlisted. Run Hermes once with `--accept-hooks`, or approve the hooks at its prompt. Headless runs never ask, so allowlist them before automating. `hermes hooks doctor` shows hook status.

After installing or reinstalling, restart the gateway so it starts memdusa's MCP server with the new settings:

```sh
hermes gateway restart
```

## What each hook does

| Hook | What memdusa does |
|---|---|
| `pre_llm_call` | On the first turn, injects the resume brief as `context`. On every turn, journals the user's message and injects relevant memories. |
| `post_tool_call` (`write_file`, `patch`, `terminal`) | Journals changed files and shell commands. |
| `pre_verify` | Records the agent's last message and asks for a checkpoint with `action: continue` when files changed and the last checkpoint is old. It fires only after edits. |
| `on_session_finalize` | Records the session end and writes a fallback checkpoint if files changed without one. |

Hermes has no compaction hook, so the fallback checkpoint is written at session finalization.

Hermes runs hooks from its own install directory, so the payload's working directory is not the project. memdusa reads the session's real workspace from `$HERMES_HOME/state.db` (read-only) to pick the project key, falling back to the payload `cwd` and then `MEMDUSA_PROJECT`.

## Known limitations

- `--scope project` and `--scope local` are rejected: Hermes only has a user config.
- Hooks must be allowlisted, and the gateway must be restarted after an install.
- The gateway runs every session's hooks in one long-lived process, so a delegated Hermes worker cannot pick up `MEMDUSA_RUN` / `MEMDUSA_TASK` from its shell. Put the `MEMDUSA_RUN=<id> MEMDUSA_TASK=<task>` line from `memdusa delegate` in the worker's prompt instead. This applies to headless runs such as `hermes -z` too. See [Multi-agent work and delegation](../guides/multi-agent.md).
- A macOS binary or database inside a protected folder needs the staging or relocation described above, or hooks time out.

## Verify it works

```sh
hermes hooks doctor                          # hook allowlist status
memdusa status
memdusa resume
grep -c 'memdusa' ~/.hermes/config.yaml      # managed blocks present
```

Start a Hermes session after restarting the gateway. The brief should appear on the first turn. Make a small edit and check recent activity in `memdusa resume`.

## Uninstall

```sh
memdusa uninstall hermes
```

Uninstall removes exactly the `# >>> memdusa` / `# <<< memdusa` blocks from `config.yaml`, including the MCP entry, and restores an expanded empty value to its original line. Everything else in the file stays untouched. Restart the gateway after uninstalling.

See also: [Harnesses overview](index.md), [Hook events](../reference/hooks.md), [Configuration reference](../reference/configuration.md).
