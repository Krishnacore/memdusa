# memdusa

memdusa gives AI coding agents a memory that outlives the session. Connect Claude Code, Codex, Cursor,
OpenCode, pi, Antigravity, Gemini CLI or Hermes to it once. After that, when a session ends, crashes, runs
out of context or hits a usage limit, the next session picks up where the last one stopped, in the same
tool or a different one.

The name is short for **mem**ory and **du**rability for AI **a**gents.

## What it does

- **Remembers** facts, preferences, decisions and todos per project (or globally), and recalls the
  relevant ones into each prompt.
- **Keeps a journal** of what happened: prompts, file edits, shell commands, commits, the agent's last
  words.
- **Checkpoints** the work: goal, what's done, what's next, blockers and key files. If the agent forgets,
  memdusa writes a fallback checkpoint before context is compacted or the session ends.
- **Hands off**: at the start of every session the agent gets a short *resume brief* built from all of
  the above.
- **Coordinates several agents**: a coordinator can delegate tasks to workers in different harnesses.
  If a worker dies, its task goes stale and a replacement takes it over without redoing finished work.
- **Scales to a team**: run `memdusa serve` and every machine and agent shares one memory, with users,
  per-project access control, OAuth sign-in and sync.

Most of this happens through each harness's own hooks, so the agent doesn't have to remember to use
memdusa. The agent can also use it directly through MCP tools.

```text
 Claude Code ─┐                        ┌────────────── memdusa ──────────────┐
 Codex ───────┤── hooks ──────────────▶│ memory       facts, prefs, decisions │
 Cursor ──────┤                        │ checkpoints  goal / done / next      │
 OpenCode ────┤── MCP (stdio / HTTP) ─▶│ journal      activity log            │
 your agent ──┘── REST /v1 ───────────▶│ runs         durable steps + leases  │
                                       │ resume       hand-off brief          │
                                       └──── SQLite (local) | Postgres ──────┘
```

## Where to start

- **Try it on your machine:** [Installation](getting-started/installation.md), then the
  [Quick start](getting-started/quick-start.md).
- **Connect your tool:** [Harnesses](harnesses/index.md) has one page per supported harness.
- **Understand the model:** [Core concepts](getting-started/concepts.md) explains memories,
  checkpoints, the journal, durable runs and the brief.
- **Run it for a team:** start with the [Team server overview](team/index.md).
- **Look something up:** the [CLI](reference/cli.md), [configuration](reference/configuration.md),
  [MCP](reference/mcp.md) and [REST API](reference/rest-api.md) references.

## License

memdusa is free and open source under the Apache License 2.0. See
[LICENSE](https://github.com/Krishnacore/memdusa/blob/main/LICENSE) and
[NOTICE](https://github.com/Krishnacore/memdusa/blob/main/NOTICE).
