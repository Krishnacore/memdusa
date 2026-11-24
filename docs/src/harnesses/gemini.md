# Gemini CLI

Gemini CLI keeps both its hooks and its MCP servers in `settings.json`. memdusa merges into that one file, so an install writes one set of changes.

## Install

```sh
memdusa install gemini
```

| Scope | Settings file |
|---|---|
| `user` (default) | `~/.gemini/settings.json` |
| `project` | `.gemini/settings.json` |

The same file receives the memdusa hooks and an `mcpServers.memdusa` entry that runs `memdusa mcp --agent gemini`. Your other settings and hooks are preserved. Gemini hook timeouts are written in milliseconds; the installer converts them for you.

## Trust and first run

Gemini CLI has no separate memdusa allowlist step in the installer. Start a new session after installing to receive the brief.

**Note:** Google stopped serving individual accounts in Gemini CLI in June 2026. For a consumer account, use [Antigravity CLI](antigravity.md) (`memdusa install antigravity`) instead. Gemini CLI remains for enterprise and paid-API users, and `memdusa install gemini` prints a note to that effect.

## What each hook does

| Hook | What memdusa does |
|---|---|
| `SessionStart` | Injects the resume brief. |
| `BeforeAgent` | Journals the prompt and injects relevant memories, using Gemini's event name in the output. |
| `AfterTool` (`write_file\|replace\|run_shell_command`) | Journals changed files and shell commands. |
| `AfterAgent` | Journals the agent's last message from `prompt_response`, then asks for a checkpoint with `decision: deny` when files changed and the last checkpoint is old. |
| `PreCompress` | Writes a fallback checkpoint if files changed without one. |
| `SessionEnd` | Writes a fallback checkpoint if files changed without one. |

Gemini has no `PostCompact` hook, so it does not record the harness-generated compaction summary that Claude Code captures.

## Known limitations

- Consumer accounts cannot use Gemini CLI. Use Antigravity for those accounts.
- No `PostCompact` compaction summary is recorded.
- The `AfterAgent` checkpoint reminder uses Gemini's `deny` decision, with the reminder text in the reason.

## Verify it works

```sh
memdusa status
memdusa resume
cat ~/.gemini/settings.json                  # hooks and mcpServers.memdusa
```

Start a new Gemini CLI session and check that the brief appears. After an edit, check recent activity in `memdusa resume`.

## Uninstall

```sh
memdusa uninstall gemini
```

Add `--scope project` to match a project install. Uninstall removes memdusa's hooks and MCP entry from `settings.json` and leaves the rest of the file unchanged.

See also: [Harnesses overview](index.md), [Antigravity CLI](antigravity.md), [Hook events](../reference/hooks.md).
