# Core concepts

memdusa stores four kinds of records: memories, checkpoints, the journal and durable runs. Everything a harness writes goes into one of them. At the start of each session, memdusa builds a resume brief from them.

## The building blocks

| Record | What it is | Why it exists |
|---|---|---|
| **Memory** | A typed entry (`fact`, `preference`, `decision`, `todo`, `note`), scoped to a project or global, deduplicated on save. | Knowledge outlives sessions and tools. |
| **Checkpoint** | Goal, done, next steps, blockers and key files. The latest checkpoint per task is the one that counts. | The core of the hand-off. |
| **Journal** | An append-only log of activity events. | Answers "what happened recently?". |
| **Durable run** | Multi-step work whose steps are journaled. A completed step replays its recorded output instead of running again. One agent holds a lease; a run whose lease expires goes stale and another agent can claim it. | Work survives crashes and tool switches without repeating side effects. |
| **Resume brief** | Checkpoints, unfinished runs, memories and recent activity, rendered as Markdown. | The first thing an agent reads in every session. |

You can write records from the CLI (`memdusa remember`, `memdusa resume`) or through MCP tools that the agent calls. Hooks write most of the journal for you. See [Hook events](../reference/hooks.md) and the [Durable run protocol](../reference/durable-runs.md) for the details.

## Project key

```sh
memdusa project
```

The project key groups all records for one repository. By default it is the git remote normalized to `github.com/org/repo`, so every machine and every harness agrees on it. Without a remote, memdusa falls back to the git root path, then the current directory. Override it with `--project github.com/acme/shop` or `MEMDUSA_PROJECT`.

## What the brief contains

```sh
memdusa resume
```

The brief combines the latest checkpoint of each task, unfinished durable runs, relevant memories and recent activity. Hooks inject it at session start, so the agent continues from where any agent left off, even in a different harness.

One section needs a note: "Last words from the previous session" shows the agent's final message and the latest compaction summary. A `Stop` hook records the last message as an `agent_message` event (up to 1500 characters, skipped when it repeats the session's previous message). A `PostCompact` hook records the summary the harness generated (up to 4000 characters). Both appear in the brief only when they are newer than the latest checkpoint. Not every harness supplies both; see the [Harnesses overview](../harnesses/index.md#support-table) for the per-harness events.

## Where writes go

A hook event is translated into memdusa's canonical format on your machine, then written to the local database or sent to a team server in remote mode. Redaction, the offline queue and code-state capture work the same in both cases. With a team server, the project key is detected on the client and sent with each request; the token stays in `~/.memdusa/config.json`, not in a committable config file.

See also: [Quick start](quick-start.md), [Multi-agent work and delegation](../guides/multi-agent.md), [Configuration reference](../reference/configuration.md).
