# memdusa

**Mem**ory & **du**rability for AI **a**gents. memdusa gives AI coding agents a memory that outlives the
session. Connect Claude Code, Codex, Cursor, OpenCode, pi, Antigravity, Gemini CLI or Hermes once; then
switch tools, lose a session or crash mid-task, and the next session continues where the last one stopped.

**Documentation: [krishnacore.github.io/memdusa](https://krishnacore.github.io/memdusa/)**

```text
 Claude Code ─┐                        ┌────────────── memdusa ──────────────┐
 Codex ───────┤── hooks ──────────────▶│ memory       facts, prefs, decisions │
 Cursor ──────┤                        │ checkpoints  goal / done / next      │
 OpenCode ────┤── MCP (stdio / HTTP) ─▶│ journal      activity log            │
 your agent ──┘── REST /v1 ───────────▶│ runs         durable steps + leases  │
                                       │ resume       hand-off brief          │
                                       └──── SQLite (local) | Postgres ──────┘
```

- **Remembers** facts, preferences, decisions and todos per project, and recalls the relevant ones into
  each prompt.
- **Journals and checkpoints** the work through each harness's own hooks. If the agent forgets to save a
  checkpoint, memdusa writes one before compaction or session end.
- **Hands off**: every new session starts with a short resume brief.
- **Coordinates agents**: a coordinator delegates tasks to workers in different harnesses. A crashed
  worker's task is taken over without redoing finished work.
- **Scales to a team**: one server with users, per-project access control, OAuth sign-in and sync.

## Install

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/Krishnacore/memdusa/main/install.sh | sh
# or Homebrew
brew install krishnacore/tap/memdusa
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/Krishnacore/memdusa/main/install.ps1 | iex
```

Other options (release archives, Docker, `cargo install`) are in
[Installation](https://krishnacore.github.io/memdusa/getting-started/installation.html).

## Quick start

Connect your harness, then start a session as usual:

```sh
memdusa install claude-code     # or: codex, cursor, opencode, pi, antigravity, gemini, hermes
```

From now on each session gets the resume brief, and prompts, edits and checkpoints are recorded. You can
also use memdusa directly:

```sh
memdusa remember "Prefers small PRs" --kind preference --global
memdusa resume                  # print the hand-off brief for this project
```

Next: the [Quick start](https://krishnacore.github.io/memdusa/getting-started/quick-start.html), the
[harness pages](https://krishnacore.github.io/memdusa/harnesses/index.html), and
[running a team server](https://krishnacore.github.io/memdusa/team/index.html).

## Contributing

```sh
cargo test
cargo clippy --all-targets -- -D warnings
mdbook serve docs               # preview the documentation
```

See [Contributing](https://krishnacore.github.io/memdusa/development/contributing.html). To report a
security issue, use GitHub's private vulnerability reporting rather than a public issue.

## License

memdusa is free and open source under the [Apache License 2.0](LICENSE). You may use it, including
commercially, modify it and redistribute it. If you redistribute memdusa or a work based on it, include the
[NOTICE](NOTICE) file and the license, keep the copyright notices, and state your changes. Please credit it
visibly, for example "Based on memdusa".
