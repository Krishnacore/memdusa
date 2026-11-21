# memdusa

**Mem**ory & **Du**rability for AI **A**gents: universal memory and durable execution for agents and harnesses.

Connect Claude Code, Cursor, Codex, Gemini CLI or your own agent to one `memdusa` service. Then switch tools, lose a
session or crash mid-task, and continue where you left off.

```
 Claude Code ─┐                        ┌────────────── memdusa ──────────────┐
 Cursor ──────┤── MCP (stdio / HTTP) ─▶│ memory       facts, prefs, decisions │
 Codex ───────┤                        │ checkpoints  goal / done / next      │
 Gemini CLI ──┤── REST /v1 ───────────▶│ journal      activity log            │
 your agent ──┘                        │ runs         durable steps + leases  │
                                       │ resume       hand-off brief          │
                                       └──── SQLite (local) | Postgres ──────┘
```

## Concepts

| | What it is | Why |
|---|---|---|
| **Memory** | Typed entries (`fact`, `preference`, `decision`, `todo`, `note`), scoped to a project or global, deduplicated | Knowledge outlives sessions and tools |
| **Checkpoint** | Goal, done, next, blockers, key files (latest one per task counts) | The core of the hand-off |
| **Journal** | Append-only activity events | "What happened recently?" |
| **Durable run** | Multi-step work whose steps are journaled. Completed steps **replay** their recorded output instead of re-executing. One agent holds a **lease**; if it disappears, the run goes **stale** and another agent can claim it | Survives crashes and tool switches without redoing side effects |
| **Resume brief** | Checkpoints + unfinished runs + memories + recent activity, rendered as Markdown | First call of every session |

**Project key:** by default this is the git remote normalized to `github.com/org/repo`, so it's the same on every
machine and in every harness. Without a remote it falls back to the git root path, then the cwd. Override it with
`--project` or `MEMDUSA_PROJECT`.

## Install

- **Prebuilt binaries** (Linux x86_64/arm64, macOS Intel/Apple Silicon, Windows): download from the GitHub release
  for each `v*` tag, unpack, and put `memdusa` on your `PATH`.
- **Server image**: `docker run -p 7077:7077 -v memdusa:/data ghcr.io/<owner>/memdusa` (SQLite in the `/data`
  volume; set `MEMDUSA_DATABASE_URL=postgres://…` for Postgres, `MEMDUSA_ALLOWED_HOSTS` for your domain).
  Create the first user with `docker exec <container> memdusa user add <name> --admin`.
- **From source**: `cargo install --path crates/memdusa`

## Quick start

```sh

memdusa project                 # show the detected project key
memdusa remember "Prefers small PRs" --kind preference --global
memdusa resume                  # print the hand-off brief
```

Data goes to `~/.memdusa/memdusa.db` by default. Set `MEMDUSA_DATABASE_URL` to `sqlite://...` or `postgres://...` to
change it.

## Claude Code: one command

```sh
memdusa install claude-code            # user scope: every project on this machine
# --scope project   -> .claude/settings.json (shared with the team)
# --scope local     -> .claude/settings.local.json (just you)
# --dry-run         -> show the resulting settings, write nothing
memdusa uninstall claude-code
```

This registers the MCP server (`claude mcp add`) and merges these hooks into your settings. It's idempotent, keeps
your other hooks, and backs the file up to `settings.json.memdusa-backup` first:

| Hook | What memdusa does |
|---|---|
| `SessionStart` | Injects the resume brief, on startup, `--resume`, `/clear`, after compaction and on fork, so Claude continues where *any* agent left off |
| `UserPromptSubmit` | Journals what you asked for (turn off with `MEMDUSA_NO_PROMPTS=1`) |
| `PostToolUse` (`Edit\|MultiEdit\|Write\|NotebookEdit\|Bash`) | Journals changed files and shell commands |
| `Stop` | If files changed and the last checkpoint is >15 min old, asks Claude once to save a `checkpoint` before finishing (`MEMDUSA_STOP_GUARD_MINS`, 0 = off) |
| `PreCompact`, `SessionEnd` | Fallback: if files changed but no checkpoint was saved, writes an automatic one (last request, changed files, previous plan carried forward) |

Hooks never break a session. Any error is logged to stderr, the hook exits 0, and it gives up well inside Claude
Code's timeout. A hook takes about 15 ms. In remote mode they call the server instead (see below).

## Connect other harnesses (local, stdio)

Each harness launches `memdusa mcp`. Pass `--agent` so records and leases show who did what.

**Claude Code (MCP only, no hooks)**
```sh
claude mcp add memdusa --scope user -- memdusa mcp --agent claude-code
```

**Codex** (`~/.codex/config.toml`)
```toml
[mcp_servers.memdusa]
command = "memdusa"
args = ["mcp", "--agent", "codex"]
```

**Cursor** (`.cursor/mcp.json`) / **Gemini CLI** (`~/.gemini/settings.json`)
```json
{ "mcpServers": { "memdusa": { "command": "memdusa", "args": ["mcp", "--agent", "cursor"] } } }
```

> If a harness doesn't start MCP servers in the project directory, add `"env": {"MEMDUSA_PROJECT": "github.com/org/repo"}`
> or let the agent pass `project` explicitly.

The server ships instructions telling the agent to call `resume` first, `checkpoint` before stopping, and so on. For
harnesses that ignore MCP server instructions, put one line in your `CLAUDE.md` / `AGENTS.md`:

> At the start of every session call the memdusa `resume` tool; save a `checkpoint` before you stop.

## Hosted mode (any machine, cloud agents)

**On the server**
```sh
export MEMDUSA_DATABASE_URL=postgres://user:pass@db/memdusa
memdusa user add alice --admin          # prints alice's first API key, once
memdusa serve --addr 0.0.0.0:7077 --allowed-host memdusa.example.com
```
Several replicas can share one Postgres. Migrations are serialized with an advisory lock.

**On each machine** (no database credentials needed)
```sh
memdusa login https://memdusa.example.com --token mdk_…      # checks the key, saves ~/.memdusa/config.json (0600)
memdusa status                                                # local or remote? reachable?
memdusa install claude-code                                   # same command as local mode
```

After `login`, everything on that machine uses the server:

| | Local mode | Remote mode (after `memdusa login`) |
|---|---|---|
| `memdusa hook …` | runs against the local DB | one `POST /v1/hooks/{event}`; the server runs the same hook logic |
| `memdusa mcp` | MCP server on the local DB | stdio→HTTP proxy to `/mcp` (works in every stdio-only harness) |
| `memdusa resume/remember/recall` | local DB | REST API |

The project key is always detected on the client from git, and sent along with each request. The token stays in
`~/.memdusa/config.json`, never in a committable file like `.mcp.json`. `MEMDUSA_URL` / `MEMDUSA_TOKEN` override the
file, `--database-url` forces local mode, and `memdusa logout` switches back to local. If the server is unreachable,
hooks give up and exit quietly (in about 50 ms when the connection is refused).

### Users and API keys

| | |
|---|---|
| **Keys** | `mdk_` + 256 random bits. Only a SHA-256 hash is stored, and each key is shown once. Keys are named (`laptop`, `ci`) and record when they were last used |
| **Auth** | Turns on automatically once the first user exists, even if that user was created by the CLI while the server is running. Before that the server is open (fine for localhost only; it logs a warning) |
| **Attribution** | Set by the server from the key: records and leases belong to `alice/claude-code`. Two users' agents with the same name never share a lease, and a client can't claim to be someone else |
| **Global memories** | Private to their author: alice's "prefers small PRs" isn't in bob's brief. Project memories, checkpoints and runs are shared by everyone on the server |
| **Shared `--token`** | Still accepted, as an admin credential not tied to a user (bootstrap or break-glass) |

```sh
memdusa user add bob [--admin]           # admin: create a user and their first key
memdusa user list | disable bob | enable bob   # disabling kills all of bob's keys at once
memdusa key create --name ci             # your own new key (admins: --user bob)
memdusa key list [--user bob]
memdusa key revoke mdk_1a2b3c4d           # by prefix or id; admins can revoke anyone's
```

Each command works directly against the database on the server, or through the API from any logged-in machine.
`memdusa status` shows who you are.

> Not yet: per-project access control. Every authenticated user can read and write every project on the server.

**Connecting a harness straight to the URL** also works. The optional headers save the agent from passing `project`
on every call:
```sh
claude mcp add --transport http memdusa https://memdusa.example.com/mcp \
  --header "Authorization: Bearer $TOKEN" \
  --header "X-Memdusa-Project: github.com/org/repo" --header "X-Memdusa-Agent: claude-code"
```

## MCP tools

`resume`, `memory_save`, `memory_search`, `memory_forget`, `checkpoint`, `log_event`, `run_start`, `run_step`,
`run_step_complete`, `run_step_fail`, `run_finish`, `run_claim`, `run_get`, `run_list`

### Durable run protocol

```
run_start {name, idempotency_key?}            -> run (re-entered if the key exists)
for each step (stable, deterministic keys):
  run_step {run_id, step_key}
    -> {action: "replay", output}             already done: use output, don't redo
    -> {action: "execute", attempt}           do it, then:
         run_step_complete {output} | run_step_fail {error}
run_finish {outcome: completed|failed|cancelled}
```

Every step call renews the run's lease (default 600s). If the owner vanishes, the lease expires, `resume` shows the run
as **STALE**, and any agent can `run_claim` it and continue. Completed steps replay, and the crashed step re-executes
with `attempt: 2`.

## REST API

| Method | Path |
|---|---|
| GET | `/v1/whoami`: who am I (user, admin) |
| GET/POST | `/v1/me/keys`, DELETE `/v1/me/keys/{id-or-prefix}`: your own keys |
| GET/POST | `/v1/admin/users` (POST `{name, admin?, key_name?}` returns the user and their first key) |
| POST | `/v1/admin/users/{name}/disable`, `/enable`, `/keys` `{name}` |
| GET | `/v1/admin/keys?user=`, DELETE `/v1/admin/keys/{id-or-prefix}` |
| POST | `/v1/hooks/{event}` `{project, agent, journal_prompts, stop_guard_mins, input}` |
| GET | `/v1/resume?project=` |
| POST/GET | `/v1/memories` (GET: `?query&project&kind&limit`) |
| DELETE | `/v1/memories/{id}` |
| POST/GET | `/v1/events` (GET: `?project&session&limit`) |
| POST/GET | `/v1/checkpoints` (GET: `?project&limit`) |
| POST/GET | `/v1/runs` (GET: `?project&status&limit`) |
| GET | `/v1/runs/{id}` |
| POST | `/v1/runs/{id}/claim` `{owner, lease_secs?, force?}` |
| POST | `/v1/runs/{id}/steps/{key}/begin` `{owner, input?}` |
| POST | `/v1/runs/{id}/steps/{key}/complete` `{output?}` |
| POST | `/v1/runs/{id}/steps/{key}/fail` `{error}` |
| POST | `/v1/runs/{id}/finish` `{outcome, output?, error?}` |

## Layout

```
crates/memdusa-core   domain model, portable SQL store (sqlx::Any: SQLite + Postgres), resume brief
crates/memdusa        binary: CLI, MCP server (rmcp, stdio + streamable HTTP), REST API (axum),
                      Claude Code hooks + installer
```

## Development

CI (`.github/workflows/ci.yml`) runs fmt, clippy and tests on Linux, macOS and Windows, the store tests against
Postgres 15 and 17, and a Docker build. Pushing a `v*` tag runs `release.yml`: binaries for five targets with
SHA-256 checksums attached to a GitHub release, and a multi-arch image pushed to GHCR.

```sh
cargo test                                                      # SQLite
MEMDUSA_TEST_POSTGRES_URL=postgres://localhost/memdusa_test cargo test   # + Postgres
```

Schema changes: append a new entry to `MIGRATIONS` in `crates/memdusa-core/src/store.rs` and never edit released entries.

## Roadmap

- [x] Claude Code hooks: auto-resume, journaling, checkpoint guard, fallback checkpoints
- [x] Remote mode: `memdusa login`, hooks over REST, stdio→HTTP MCP proxy
- [ ] Capture `PostCompact` summaries and Stop's `last_assistant_message` into the journal
- [ ] Hook adapters for other harnesses (Codex, Gemini CLI, Cursor) and git hooks
- [ ] Semantic memory search (embeddings behind a trait; local model or API), plus SQLite FTS5 / Postgres `tsvector`
- [ ] Memory consolidation: summarize old events, merge near-duplicate memories
- [x] Per-user API keys, server-side attribution, private global memories
- [ ] Per-project access control (who can read/write which projects), audit log
- [ ] OAuth for remote MCP (so harnesses can connect without pasting a key)
- [ ] Local ⇄ hosted sync
- [ ] Transcript import from Claude Code / Codex session files
