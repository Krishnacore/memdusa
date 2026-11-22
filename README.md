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

- **Homebrew** (macOS and Linux, arm64 and x86_64): `brew install krishnacore/tap/memdusa`
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
| `UserPromptSubmit` | Journals what you asked for (turn off with `MEMDUSA_NO_PROMPTS=1`), and injects up to 5 memories relevant to the prompt (`MEMDUSA_RECALL=off` disables) |
| `PostToolUse` (`Edit\|MultiEdit\|Write\|NotebookEdit\|Bash`) | Journals changed files and shell commands |
| `Stop` | If files changed and the last checkpoint is >15 min old, asks Claude once to save a `checkpoint` before finishing (`MEMDUSA_STOP_GUARD_MINS`, 0 = off) |
| `PreCompact`, `SessionEnd` | Fallback: if files changed but no checkpoint was saved, writes an automatic one (last request, changed files, previous plan carried forward, the agent's last message) |
| `PostCompact` | Records the compaction summary Claude Code generated (`compact_summary`, up to 4000 chars) |

`Stop` also records the agent's **last words** (`last_assistant_message`, up to 1500 chars, skipped if identical to the
session's previous one) as an `agent_message` event. Gemini (`AfterAgent`) and Hermes (`pre_verify`) supply it too.
The resume brief shows the latest message and compaction summary under "Last words from the previous session", but
only when they are newer than the latest checkpoint.

Hooks never break a session. Any error is logged to stderr, the hook exits 0, and it gives up well inside Claude
Code's timeout. A hook takes about 15 ms. In remote mode they call the server instead (see below).

## Other harnesses

One command per harness. It installs that tool's own hooks (or plugin) and registers the MCP server:

```sh
memdusa install codex          # or: cursor | antigravity | gemini | opencode | pi | hermes
memdusa install codex --scope project
memdusa uninstall codex
```

Each tool's hook payload is translated to memdusa's canonical format on your machine, and the output is rendered back
in that tool's dialect. Remote mode, the offline queue, redaction and code state therefore work the same everywhere.

| Harness | How memdusa plugs in | Brief at start | Prompts | Edits / commands | Checkpoint reminder | Fallback checkpoint |
|---|---|---|---|---|---|---|
| Claude Code | hooks in `settings.json` | ✓ (also after compaction) | ✓ | ✓ | Stop | PreCompact, PostCompact (summary), SessionEnd |
| Codex CLI | `~/.codex/hooks.json` + `config.toml` | ✓ (also after compaction) | ✓ | ✓ (patch → files) | Stop | PreCompact, SessionEnd |
| Cursor | `~/.cursor/hooks.json` + `mcp.json` | ✓ | ✓ | ✓ | stop (`followup_message`) | preCompact, sessionEnd |
| Antigravity CLI | `~/.gemini/config/hooks.json` + `mcp_config.json` | ✓ | ✓ (first `PreInvocation`) | ✓ | Stop (`continue`) | (no such hooks) |
| Gemini CLI | `~/.gemini/settings.json` | ✓ | ✓ (`BeforeAgent`) | ✓ | AfterAgent (`deny`) | PreCompress, SessionEnd |
| OpenCode | TypeScript plugin in `.opencode/plugins/` | ✓ (first prompt) | ✓ | ✓ | `experimental.idle` | compaction hook |
| pi | TypeScript extension in `~/.pi/agent/extensions/` | ✓ (first run) | ✓ | ✓ | `agent_before_settle` | session_before_compact, session_shutdown |
| Hermes Agent | shell hooks in `~/.hermes/config.yaml` | ✓ (first turn) | ✓ | ✓ | `pre_verify` (after edits only) | on_session_finalize |

Things to know per tool:
- **Codex** runs new hooks only after you trust them in `/hooks`.
- **Cursor** also runs Claude Code hooks by default ("Third-Party Imports"). If both are installed, turn that off so events
  aren't recorded twice; `memdusa install cursor` warns you.
- **Antigravity** has no session-end or compaction hooks, so there's no fallback checkpoint there; the stop reminder does
  that job. Gemini CLI remains for enterprise and paid-API users (consumer accounts moved to Antigravity in June 2026).
- **pi** hides MCP tools behind "code mode" by default; memdusa registers its server with `exposure: "direct"`.
- **Hermes** asks once to approve each hook (headless: `--accept-hooks`). memdusa writes a marked block into
  `config.yaml`, or prints it for you to merge if the file already has `hooks:` / `mcp_servers:`.
- The checkpoint reminder fires **at most once per session per window** for every harness, so it can't trap an agent that
  has no way to checkpoint.

Any other MCP client can use memdusa as a plain MCP server (`memdusa mcp --agent <name>`), without hooks.

### Git hooks

```sh
memdusa install git       # in the repo; per clone
memdusa uninstall git
```

Commits, branch switches and merges are recorded as events in the journal, whoever made them (any agent or a human),
through the same hook pipeline as the harness hooks (redaction, offline queue, remote mode). The hooks run in the
background and never block or fail a git command. They are installed in `git rev-parse --git-path hooks`, so
`core.hooksPath` (for example Husky) is respected. Existing user hooks are kept; memdusa only adds its own line.

## Importing past sessions

Work done before memdusa was installed, or in a tool without hooks, can still reach memdusa:

```sh
memdusa import-transcript claude-code --latest          # newest session for this repo
memdusa import-transcript codex ~/.codex/sessions/2026/03/02/rollout-….jsonl
memdusa import-transcript pi session1.jsonl session2.jsonl --dry-run
```

Supported: `claude-code` (`~/.claude/projects/…`), `codex` (`~/.codex/sessions/…`; `.zst`-compressed files are skipped,
decompress them first) and `pi` (`~/.pi/agent/sessions/…`). A transcript becomes the events its hooks would have
recorded (prompts, file changes, commands, with their original timestamps), followed by a session end, so the usual
fallback checkpoint is written if files changed. It goes through the same pipeline as live hooks (redaction, remote
mode, project detection from the transcript's recorded directory, or `--project`).

- `--latest` picks the most recent session for the current directory's repo; otherwise pass the files.
- Idempotent: a session that already has events is skipped, unless you pass `--force`.
- `--dry-run` prints what would be imported (counts and time range) and writes nothing.

## Retention

History grows with every edit and command. To keep the database and the brief fast:

| What | Default | How |
|---|---|---|
| Hook activity (edits, commands, prompts, session start/end) | 30 days | **Summarized, then deleted**: each old session becomes one `session_summary` event ("14 files changed (…), 22 commands, 3 prompts. First request: …") |
| Superseded checkpoints | 90 days | Deleted. The latest checkpoint of every task is always kept |
| Finished runs and their steps | 90 days | Deleted |
| Memories, events logged by agents | forever | Never pruned automatically |

`memdusa serve` prunes once a day (`--no-auto-prune` to turn it off). Local mode prunes at the end of a session, at
most once a day. To run it by hand: `memdusa prune [--dry-run] [--only-project P] [--activity-days N]
[--checkpoint-days N] [--run-days N]` (locally, or as an admin against a server). Windows can also be set with
`MEMDUSA_RETENTION_ACTIVITY_DAYS` / `_CHECKPOINT_DAYS` / `_RUN_DAYS`. `0` means keep forever.

## Operations

| | |
|---|---|
| **Backup** | `memdusa backup <file>`: consistent online copy of the SQLite database (`VACUUM INTO`). For Postgres use `pg_dump` |
| **Export / import** | `memdusa export [--only-project P] [-o file]` writes JSON Lines (memories, events, checkpoints, runs with steps; never users or keys). `memdusa import <file\|->` keeps ids and timestamps, skips what already exists (safe to re-run), and re-applies redaction |
| **Local → hosted** | `memdusa --database-url sqlite://$HOME/.memdusa/memdusa.db export \| memdusa import -` while logged in to a server (needs an admin key) |
| **Rate limiting** | Per user (or IP for anonymous callers): `--rate-limit 50` req/s with bursts up to 10x, `0` = off. Over the limit you get `429` with `Retry-After`, and hooks queue instead of losing data |
| **Body limits** | 1 MB per request, 16 MB for hook batches, 64 MB for imports |
| **Metrics** | Prometheus at `/metrics` (`--no-metrics` to turn it off): requests and latency by route *template*, hook events, rate-limit and auth failures, users, running and stale runs. No ids, projects or users in labels |
| **Logs** | One access-log line per request (method, route template, status, latency, user; never bodies or query strings). `MEMDUSA_LOG_FORMAT=json` for JSON, `RUST_LOG` for levels |

## Secret redaction

Hooks capture raw prompts and shell commands, and agents paste things into memories, so secrets will show up.
Everything is redacted **before it is stored**: in the store layer, which every write path goes through, and again on
the client before a remote hook sends anything. Secrets become `[REDACTED:<kind>]` and the surrounding text is kept:

```
PGPASSWORD=[REDACTED:assignment] psql postgres://app:[REDACTED:url_password]@db/prod
```

| Detected | Examples |
|---|---|
| Provider tokens | AWS, GitHub, GitLab, Anthropic, OpenAI, Stripe, Slack, Google, npm, JWTs, memdusa `mdk_` keys |
| Structure | private key blocks, `user:password@` in URLs, `Bearer …` |
| Assignments and flags | `*_TOKEN=…`, `password: …`, `"api_key": "…"`, `--password …`, `--api-key=…` |

Not redacted on purpose: variable references (`TOKEN=$GH_TOKEN`), numbers (`max_tokens=1000`), paths
(`--token-file ~/.config/token`), and ordinary words ("rotate tokens monthly").

- Extra patterns: one regex per line in `MEMDUSA_REDACT_PATTERNS` or `~/.memdusa/redact-patterns.txt` (`#` comments).
  Matches become `[REDACTED:custom]`.
- `MEMDUSA_REDACT=off` stores everything verbatim.
- Step outputs are redacted too, so a replayed step returns the redacted output. Don't pass secrets through runs.

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

**When the server is unreachable**, nothing is lost:
- Prompts, tool use, compaction and session end are queued in `~/.memdusa/spool/` and replayed (sorted by when they
  happened, with their original timestamps) by the next hook that reaches the server, or by `memdusa sync`.
- SessionStart serves the last cached brief, marked as possibly out of date.
- `memdusa status` shows whether the server is reachable and how many events are queued.
- Rejected requests (revoked key, bad input) are not queued, and delivery is at-least-once.

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

`resume`, `memory_save`, `memory_update`, `memory_search`, `memory_forget`, `checkpoint`, `log_event`, `run_start`,
`run_step`, `run_step_complete`, `run_step_fail`, `run_finish`, `run_claim`, `run_get`, `run_list`

### Memory hygiene

- `memory_update` (REST: `PATCH /v1/memories/{id}`) corrects a memory in place: any of `content`, `kind`, `tags`.
  Redaction applies and the search index follows. On a multi-user server, a global memory can only be updated or
  forgotten by its owner or an admin (403 otherwise); project memories stay editable by any user.
- Saving a near-duplicate merges instead of adding: a memory of the same kind and scope (project, or your globals)
  whose significant words overlap by at least 75% (Jaccard; short words and stopwords ignored, "auth" ~
  "authentication") is updated with the newer text and merged tags. The result says so (`merged: true` in REST/JSON).
- The brief flags a memory with ` _(may be outdated: src/auth/jwt.rs changed 2h ago)_` when it mentions a file that
  a recent `file_change` event touched after the memory was last updated.

### Search and recall

`memory_search` is ranked full-text search over memory content and tags (SQLite FTS5 with `bm25`, Postgres
`tsvector` with `ts_rank`, `simple` config, no stemming). Any word may match; the best match comes first, newest
first among ties. Query text is split into plain words, never interpreted as query syntax. No query lists the most
recent memories.

On every prompt, the `UserPromptSubmit` hook searches the project's memories (plus visible globals) with the prompt
and injects up to 5 that match at least two of its distinctive words (one, if the prompt has only one) in a
`<memdusa-recall>` block. A memory is injected at most once per session, and not if it was already in the session's
brief. Harnesses that can't inject context on a prompt (Cursor) get nothing. Set `MEMDUSA_RECALL=off` to disable it.

### MCP resources and prompts

For clients that surface resources and prompts better than tool calls, the brief is also exposed as:

- Resources (Markdown): `memdusa://brief` (the same text `resume` returns, for the default project),
  `memdusa://memories` (project and visible global memories, grouped by kind), and the template
  `memdusa://brief/{project}` for an explicit, URL-encoded project key (`memdusa://brief/github.com%2Facme%2Fshop`).
  Unknown URIs return an MCP "resource not found" error.
- Prompts: `resume` (optional `project` argument; the brief plus "continue from this brief") and `checkpoint`
  (asks the agent to save a checkpoint and `memory_save` its decisions). Many clients show these as slash commands.

The stdio→HTTP proxy forwards resources and prompts to the remote server, like tools.

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
| POST | `/v1/hooks/batch` `{items: [{event, request}]}`: replay queued hooks (max 1000) |
| GET/POST | `/v1/me/keys`, DELETE `/v1/me/keys/{id-or-prefix}`: your own keys |
| GET/POST | `/v1/admin/users` (POST `{name, admin?, key_name?}` returns the user and their first key) |
| POST | `/v1/admin/users/{name}/disable`, `/enable`, `/keys` `{name}` |
| GET | `/v1/admin/keys?user=`, DELETE `/v1/admin/keys/{id-or-prefix}` |
| POST | `/v1/admin/prune` `{policy?, project?, dry_run?}` |
| GET | `/v1/admin/export?project=` (JSON Lines), POST `/v1/admin/import` (JSON Lines body) |
| GET | `/metrics` (Prometheus, no auth), `/health` |
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

## License

memdusa is free and open source under the [Apache License 2.0](LICENSE). You may use it, including commercially,
modify it and redistribute it. If you redistribute memdusa or a work based on it, in source or binary form, you must
include the [NOTICE](NOTICE) file and the license, keep the copyright notices, and state your changes. Please credit
it visibly, for example "Based on memdusa".

## Development

CI (`.github/workflows/ci.yml`) runs fmt, clippy and tests on Linux, macOS and Windows, the store tests against
Postgres 15 and 17, and a Docker build. Pushing a `v*` tag runs `release.yml`: binaries for five targets with
SHA-256 checksums attached to a GitHub release, and a multi-arch image pushed to GHCR.

The release workflow also updates the formula in the `homebrew-tap` repository automatically (rendered by
`packaging/homebrew/render-formula.sh`, skipped for pre-release tags). It needs the repository secret
`HOMEBREW_TAP_DEPLOY_KEY`: the private half of a deploy key with write access on the tap repo. Without it the job
only prints a warning.

```sh
cargo test                                                      # SQLite
MEMDUSA_TEST_POSTGRES_URL=postgres://localhost/memdusa_test cargo test   # + Postgres
```

Schema changes: append a new entry to `MIGRATIONS` in `crates/memdusa-core/src/store.rs` and never edit released entries.

## Roadmap

- [x] Claude Code hooks: auto-resume, journaling, checkpoint guard, fallback checkpoints
- [x] Remote mode: `memdusa login`, hooks over REST, stdio→HTTP MCP proxy
- [x] Capture `PostCompact` summaries and Stop's `last_assistant_message` into the journal
- [x] Adapters: Codex, Cursor, Antigravity, Gemini CLI, OpenCode, pi, Hermes
- [x] Handoff eval with real agents (`eval/`)
- [x] Git hooks (commits, branch switches) as events
- [x] MCP resources and prompts (brief, memories, /resume)
- [x] Transcript import (Claude Code, Codex, pi; OpenCode not yet)
- [x] Ranked full-text memory search
- [x] Relevant memories injected on each prompt
- [ ] **Easy installation on every platform** (today: download a release archive, `cargo install`, or Docker)
  - ✓ macOS/Linux: a Homebrew tap (`brew install krishnacore/tap/memdusa`) whose formula the release workflow updates
    automatically (version, URLs, SHA-256 per platform); later, a homebrew-core submission (not done yet)
  - one-line install script (`curl -fsSL …/install.sh | sh`) that picks the right archive, verifies its checksum and
    installs to `~/.local/bin`, plus a PowerShell equivalent for Windows
  - Windows: Scoop bucket and winget manifest, generated per release
  - `cargo binstall memdusa` (metadata pointing at the release archives) and publishing to crates.io
  - Linux packages (`.deb`/`.rpm`) and an AUR package; container images are already published to GHCR
  - `memdusa self-update` that uses the same release archives and checksums
- [ ] **Multi-agent coordination** (one harness coordinating others, like a lead agent delegating to Claude Code,
      OpenCode or Codex workers via `hc`, or a harness's own subagents). Observed while building memdusa itself:
      when a worker hit its usage limit, its replacement in another harness started from scratch. Ideas:
  - parent/child sessions: link a worker's session to its coordinator (from hook fields like `agent_id`/`agent_type`,
    Hermes `conversation_id`, or an explicit `MEMDUSA_PARENT_SESSION`), so the coordinator's brief gets a
    "Delegated work" section with each worker's status, checkpoint and last words
  - task-scoped checkpoints by default for workers (`task` = the assignment), so they never overwrite the
    coordinator's checkpoint, and a worker's replacement resumes exactly that task
  - worktree/branch awareness: workers in different worktrees share the project key, but code-state warnings and
    "files changed" should be per worktree/branch, not mixed
  - durable runs as the coordination primitive: the coordinator creates a run whose steps are the delegated tasks;
    workers claim steps with leases, so a crashed or rate-limited worker's step is visibly stale and can be
    claimed by another harness, and finished steps are never redone
  - attribution in the brief: which agent (and harness) did what, across the whole team
- [ ] Semantic memory search (embeddings behind a trait; local model or API)
- [x] Memory hygiene: update, near-duplicate merge, stale flags
- [ ] Memory consolidation: summarize old events
- [x] Per-user API keys, server-side attribution, private global memories
- [ ] Per-project access control (who can read/write which projects), audit log
- [ ] OAuth for remote MCP (so harnesses can connect without pasting a key)
- [x] Local → hosted migration (`export | import`)
- [ ] Continuous local ⇄ hosted sync
