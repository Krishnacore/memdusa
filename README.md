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
- **One-line installers** (no Homebrew needed):
  - macOS / Linux (arm64 or x86_64): `curl -fsSL https://raw.githubusercontent.com/Krishnacore/memdusa/main/install.sh | sh`
  - Windows (PowerShell, x86_64): `irm https://raw.githubusercontent.com/Krishnacore/memdusa/main/install.ps1 | iex`

  Both download the matching release archive, verify its SHA-256 checksum and install `memdusa` to `~/.local/bin`
  (Windows: `%LOCALAPPDATA%\memdusa\bin`). Set `MEMDUSA_VERSION=v0.1.1` to pin a version and `MEMDUSA_INSTALL_DIR`
  to change the destination. Then connect a harness, e.g. `memdusa install claude-code`.
- **Prebuilt binaries** (Linux x86_64/arm64, macOS Intel/Apple Silicon, Windows): download from the GitHub release
  for each `v*` tag, unpack, and put `memdusa` on your `PATH`.
- **Self-update**: `memdusa self-update` fetches the latest release for your platform, verifies its SHA-256 checksum
  and replaces the running binary. `memdusa self-update --check` only reports the available version (no download);
  `memdusa self-update --version v0.1.1` installs a specific one. Installs managed by Homebrew or cargo are not
  replaced: use `brew upgrade memdusa` or `cargo install` / `cargo binstall memdusa` instead.
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
| Antigravity CLI | `~/.gemini/config/hooks.json` + `mcp_config.json` | ✓ | ✓ (first `PreInvocation`) | ✓ | Stop (`continue`) | Stop when fully idle (once per session; last words from the transcript) |
| Gemini CLI | `~/.gemini/settings.json` | ✓ | ✓ (`BeforeAgent`) | ✓ | AfterAgent (`deny`) | PreCompress, SessionEnd |
| OpenCode | TypeScript plugin in `.opencode/plugins/` | ✓ (first prompt) | ✓ | ✓ | `experimental.idle` | compaction hook |
| pi | TypeScript extension in `~/.pi/agent/extensions/` | ✓ (first run) | ✓ | ✓ | `agent_before_settle` | session_before_compact, session_shutdown |
| Hermes Agent | shell hooks in `~/.hermes/config.yaml` | ✓ (first turn) | ✓ | ✓ | `pre_verify` (after edits only) | on_session_finalize |

`--database-url URL` (or `MEMDUSA_DATABASE_URL`), when given to `memdusa install`, is written into every generated hook
command, MCP entry and plugin, so they use that database instead of the default `~/.memdusa/memdusa.db`. Without it the
generated config is unchanged.

### Trust and first run

Most harnesses refuse to run a project's hooks or MCP servers until you approve them once:

| Harness | One-time step |
|---|---|
| Codex | Project hooks run only after trust: approve in `/hooks` (automation: `--dangerously-bypass-hook-trust`). A project `.codex/config.toml` also needs the project trusted (trust prompt / `codex --trust`). |
| Antigravity CLI (`agy`) | Project hooks in `.agents/` run in `agy -p` without a trust prompt. Headless mode soft-denies MCP tools it would have to ask about, so `memdusa install antigravity` adds a PreToolUse hook that auto-approves **only** calls to memdusa's own server (`checkpoint` works with plain `agy -p`; no `--dangerously-skip-permissions` needed). `MEMDUSA_RUN` / `MEMDUSA_TASK` from the launching shell do reach the MCP server — no `env` entry is needed in `mcp_config.json`. |
| Cursor CLI (`cursor-agent`) | Untrusted folders won't start in print mode: trust the folder interactively or pass `--trust`; `--approve-mcps` auto-approves the MCP server. Cursor runs memdusa through its Claude Code hooks import. |
| pi | Project extensions (`.pi/`) load only for trusted projects: answer the trust prompt, or pass `--approve` / `-a` for one run. |
| Hermes | Shell hooks must be allowlisted: run Hermes once with `--accept-hooks` (or approve at its prompt). `hermes hooks doctor` shows their status. |
| OpenCode v2 | Plugins and MCP servers run in a shared background service that does not inherit the environment of the shell you start `opencode` from. Configure memdusa at install time (`memdusa install opencode --database-url …`, or `memdusa login`, which is saved in `~/.memdusa` and read by every hook), not with shell env vars. A delegated worker must launch with `opencode run --standalone` (a private server that does inherit env), or its MCP tools won't see `MEMDUSA_RUN` / `MEMDUSA_TASK`. |
| Gemini CLI | Google stopped serving individual accounts in Gemini CLI (June 2026): use Antigravity (`agy`, `memdusa install antigravity`). |

Things to know per tool:
- **Codex** runs new hooks only after you trust them in `/hooks`.
- **Cursor** also runs Claude Code hooks by default ("Third-Party Imports"). If both are installed, turn that off so events
  aren't recorded twice; `memdusa install cursor` warns you.
- **Antigravity** has no session-end or compaction hooks. Its Stop hook payload has no assistant text, so memdusa reads
  the agent's last words from the session transcript (`transcriptPath`, last `PLANNER_RESPONSE` step) and writes a single
  fallback checkpoint when the agent goes fully idle after editing files. The Stop reminder still asks for a richer
  checkpoint when the memdusa MCP tools are available. Gemini CLI remains for enterprise and paid-API users (consumer
  accounts moved to Antigravity in June 2026).
- **pi** hides MCP tools behind "code mode" by default; memdusa registers its server with `exposure: "direct"`.
- **Hermes** hooks need allowlisting (see above). memdusa merges into your existing `config.yaml` by inserting marked
  `# >>> memdusa` / `# <<< memdusa` blocks (under your existing `hooks:` / `mcp_servers:` keys, or as new sections),
  leaving the rest of the file untouched; `memdusa uninstall hermes` removes exactly those blocks. Hermes runs hooks from
  its own install dir, so memdusa reads the session's real workspace from `$HERMES_HOME/state.db` (read-only) to pick the
  project, falling back to the payload `cwd` and then `MEMDUSA_PROJECT`.
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

## Multi-agent setups

A coordinator agent can delegate tasks to workers in other harnesses (Claude Code, OpenCode, Codex, ...) in the same
project. Launch each worker with environment variables:

```sh
MEMDUSA_TASK=install-scripts MEMDUSA_PARENT=lead-session claude   # or opencode, codex, ...
```

For a headless worker, pass the run and task on the launch line — one example per harness verified live:

```sh
MEMDUSA_RUN=… MEMDUSA_TASK=refunds   claude -p "Continue your delegated task."
MEMDUSA_RUN=… MEMDUSA_TASK=invoices  codex exec "Continue your delegated task." < /dev/null
MEMDUSA_RUN=… MEMDUSA_TASK=docs      opencode run --standalone "Continue your delegated task."
```

`codex exec` reads stdin when it is not a terminal, so close it (`< /dev/null`) or it waits. Claude Code passes the
launch env to its stdio MCP servers, and `memdusa install codex` writes `env_vars` (plus
`default_tools_approval_mode = "approve"`, so headless tool calls don't need an interactive approver) into
`~/.codex/config.toml`. OpenCode v2 runs plugins and MCP servers in a shared background service that does not inherit
the launcher's env, so a delegated worker must use `opencode run --standalone`. For pi, Cursor, Antigravity and Hermes
we have not verified env forwarding to MCP servers; configure memdusa at install time (`--database-url` / `memdusa
login`) instead of relying on shell env vars.

`MEMDUSA_TASK` marks the worker (required); `MEMDUSA_PARENT` is free text naming the coordinator (optional). Then:

- **Worker sessions are recorded.** At session start memdusa journals a `delegated_session` event (task, parent,
  harness), in local and hosted mode alike.
- **Worker checkpoints are task-scoped.** A checkpoint saved without a `task` (the `checkpoint` tool, or the automatic
  fallback checkpoint) gets `task = MEMDUSA_TASK`, so it never replaces the coordinator's. An explicit `task` wins.
  Over a hosted server the `memdusa mcp` proxy passes the env along.
- **A worker's brief is its task.** It shows only that task's latest checkpoints and recent activity, and its "Last
  words" are the last message of *previous sessions of the same task*, whatever harness they ran in. When a worker hits
  its usage limit, its replacement started with the same `MEMDUSA_TASK` resumes exactly there. The `resume` tool and
  `memdusa resume` take the same task (the tool also has an optional `task` argument).
- **The coordinator's brief gets "Delegated work".** Without `MEMDUSA_TASK`, the brief lists tasks delegated in the last
  7 days (up to 10, newest first): harness, status (`active` until the worker's session ends, then `ended`), the task's
  latest checkpoint goal and next steps, and the worker's last message. Workers' checkpoints and messages are kept out
  of the coordinator's own "Where things stand" and "Last words".
- **Worktrees are separate.** Workers in sibling git worktrees share the project key, but the brief compares code state
  with the latest checkpoint from *this* worktree (on this machine), lists this worktree's recent file edits first and
  summarizes the others as `Other worktrees: ../memdusa-eval2 (4 files)`; a checkpoint from another worktree is noted
  calmly instead of raising a "Code state differs" warning.

### Delegating through a durable run

`MEMDUSA_TASK` alone only records who works on what. To also stop two workers doing the same task, and to take over
from a worker that died, delegate through a durable run: each task is a step with its own lease.

```sh
memdusa delegate "windows release" install-scripts docs     # or the `delegate` MCP tool
# Delegated run `0198…` (windows release): 2 task(s).
# - MEMDUSA_RUN=0198… MEMDUSA_TASK=install-scripts
# - MEMDUSA_RUN=0198… MEMDUSA_TASK=docs
```

The coordinator launches each worker (any harness) with its env line. Delegating again with the same name (or
`--key`) while the run is running reuses it and adds the missing tasks.

- **Claim at session start.** With `MEMDUSA_RUN` and `MEMDUSA_TASK` set, the worker's SessionStart hook claims its step
  (owner: agent name and session id) with a 15 minute lease, and the brief opens with a notice: *claimed* (and, when it
  took over, *the previous attempt by X went stale*), *already done* (with the recorded result: stop, or confirm with
  the user) or *held by another live worker* (coordinate, don't duplicate). Restarting a worker with `/clear` or after
  compaction re-claims its own step.
- **Every other hook event renews the lease** (prompts, tool use, stop), so a working agent never goes stale.
  `SessionEnd` does not finish the task: the lease just expires, which is what a worker that stopped mid-task should look
  like.
- **Finishing is explicit:** the `task_complete` MCP tool or `memdusa task done --summary "..."`; `task_fail` /
  `memdusa task fail <reason>` gives the task up (a replacement can claim it and is told why it failed). Run and task
  default to `MEMDUSA_RUN`/`MEMDUSA_TASK` (the `memdusa mcp` proxy passes them over a hosted server). A finished task is
  never redone: a worker started on it is told so.
- **Stale takeover.** A `running` step whose lease expired is *stale*; a replacement launched with the same env line, in
  any harness, claims it. A live lease is never taken silently.
- **The coordinator's "Delegated work" shows each task's step status** instead of the session-based guess:
  `pending`, `running by X`, `stale (owner X, lease expired 12m ago)`, `done` (with the result) or `failed: reason`. With
  several runs, tasks are grouped under the run's name. Tasks delegated by `MEMDUSA_TASK` alone keep the earlier
  behaviour. Close the run with `run_finish` when the work is over.

`MEMDUSA_STEP_LEASE_SECS` overrides the lease length (default 900, read where the claim happens: on the server when
hosted); it exists for tests and tuning.

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

## Semantic search

Search is lexical by default (full-text, any word may match). That misses memories worded differently: "how do we
ship releases" won't find "the release workflow pushes binaries on tag". Turn on **semantic search** and memdusa also
compares meaning, using either an OpenAI-compatible `/embeddings` API or a small model that runs inside memdusa.
It is **off unless you set `MEMDUSA_EMBED_URL`** (or `MEMDUSA_EMBED_PROVIDER`).

```sh
# OpenAI
export MEMDUSA_EMBED_URL=https://api.openai.com/v1      # uses OPENAI_API_KEY (only for api.openai.com)
# fully local with Ollama (ollama pull nomic-embed-text)
export MEMDUSA_EMBED_URL=http://localhost:11434/v1
export MEMDUSA_EMBED_MODEL=nomic-embed-text

memdusa embed          # embed the memories you already have (new and edited ones are embedded when saved)
memdusa status         # "semantic search: on (model ..., 12/12 memories embedded)"
```

### Built-in local model (no server, no API key)

The `local-embed` cargo feature compiles in a retrieval-trained sentence transformer (`all-MiniLM-L6-v2`, 384 dims)
running on ONNX Runtime. Nothing leaves your machine. It stays off until you select it. Where it's included:

| | Built-in local model |
|---|---|
| macOS Apple Silicon (release binary, Homebrew) | ✓ |
| Windows x64 (release binary) | ✓ |
| Docker image (`ghcr.io/krishnacore/memdusa`) | ✓ |
| Linux release binaries, Intel macOS | ✗: ONNX Runtime's prebuilt library needs glibc 2.38+ (the release binaries keep supporting Ubuntu 22.04 / Debian 12 / RHEL 9) and has no Intel-Mac build. Use Ollama (above), or build with `--features local-embed` on a glibc 2.38+ system |

```sh
export MEMDUSA_EMBED_URL=local                    # or MEMDUSA_EMBED_PROVIDER=local
export MEMDUSA_EMBED_MODEL=all-MiniLM-L6-v2       # default; bge-small-en-v1.5 is also supported
memdusa embed --download-model                    # once: ~91 MB into $MEMDUSA_HOME/models/ (default ~/.memdusa/models/)
memdusa embed                                     # backfill existing memories
memdusa status                                    # shows the model and coverage, or how to fetch it
```

From source: `cargo install memdusa --features local-embed` (or `cargo build --release --features local-embed`).

Nothing leaves the machine: model text, memories and queries are processed in-process, and the only network request is
that first Hugging Face download. After that memdusa works offline. Hooks and searches **never download**: if the model
is missing they use lexical search, and `memdusa status` / `memdusa embed` print the one-line fetch command above.
The model is loaded at most once per process; ONNX Runtime binaries (~20 MB per platform, fetched at build time by
`ort`) are separate from the model download.

| Variable | Meaning |
|---|---|
| `MEMDUSA_EMBED_URL` | Base URL of the API (`POST {url}/embeddings`). `local` selects the built-in model. Unset = semantic search off |
| `MEMDUSA_EMBED_PROVIDER` | `http` (default) or `local`. `local` also works when no URL is set |
| `MEMDUSA_EMBED_MODEL` | Model name (default `text-embedding-3-small`). Changing it re-embeds on the next `memdusa embed` |
| `MEMDUSA_EMBED_API_KEY` | Bearer token. Falls back to `OPENAI_API_KEY`, but only when the URL host is `api.openai.com` |
| `MEMDUSA_EMBED_RECALL_MIN` | Similarity (0..1) from which a memory is added to a prompt by meaning alone, without sharing words with it. Default `0.5` (`0.4` with built-in `all-MiniLM-L6-v2`). Scores differ by model: with `nomic-embed-text`, related text scores about 0.45-0.6 and unrelated 0.3-0.42; OpenAI's `text-embedding-3-*` score lower overall, so try `0.35`-`0.4` there. Too low and every prompt pulls in loosely related memories |

How it works:

- One vector per memory (content and tags), stored in the database next to it (SQLite and Postgres alike; no
  extension needed). A memory is re-embedded when its text changes; vectors of another model are ignored until
  `memdusa embed` redoes them (`--all` redoes everything).
- A search embeds the query (one API call) and ranks the 1000 most recent visible memories by
  `0.6 × cosine similarity + 0.4 × lexical relevance`; memories without a vector rank by lexical relevance alone.
  This feeds the `memory_search` tool, `memdusa recall`, the relevant memories shown with each prompt, and the brief.
- `memory_review` also groups memories whose embeddings are very close (cosine ≥ 0.80), not only those sharing words.
  Merging on save stays lexical.
- It never gets in the way: if the API fails or is slow (10 s per request; 3 s for prompt recall and the brief),
  memdusa logs one warning line and uses lexical search; a save or hook never fails because of embedding.
- **Privacy:** with an HTTP provider, memory text (already redacted before it is stored) and search queries are sent
  to that provider; use Ollama or the built-in model to keep everything on your machine.
- **Measured quality (same 3-memory probe as the Ollama/Apple comparison):** `all-MiniLM-L6-v2` gets 3/4 target
  queries with the right memory on top (currency precision 0.603, discount + currency 0.498, new release 0.232 top)
  and leaves unrelated queries at ≤0.193. `bge-small-en-v1.5` also gets 3/4 but scores unrelated text much higher
  (up to 0.586), so MiniLM is the default: smaller (86 MB vs 127 MB on disk) and better calibrated against
  `COSINE_FLOOR` 0.3. The probe is an ignored test: `cargo test -p memdusa --features local-embed -- --ignored --nocapture`.
- **Measured latency** (debug build, Apple Silicon): MiniLM first download + load 3.7 s once; cold single query
  (new process, session load + embedding) ~0.50 s; warm single query ~11 ms — well inside the 3 s prompt-recall cap.
  BGE small: ~4.7 s first download, ~0.74 s cold, ~18 ms warm.
- **Hosted mode:** the server's environment decides. Set the variables where `memdusa serve` runs; clients just call
  the API and need nothing. Backfill a server with `memdusa embed` (admin).

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
  happened, with their original timestamps) by the next hook that reaches the server.
- SessionStart serves the last cached brief, marked as possibly out of date.
- `memdusa status` shows whether the server is reachable and how many events are queued.
- Rejected requests (revoked key, bad input) are not queued, and delivery is at-least-once.
- Before v0.1.9, `memdusa sync` flushed this queue by hand. It now runs project sync (see "Team server: sync");
  the queue still flushes on its own with the next hook that reaches the server.

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
| **Global memories** | Private to their author: alice's "prefers small PRs" isn't in bob's brief. Project data is shared by users with access to that project (see below) |
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

### Team server: access control

Authenticated users only reach the projects they're granted. Admins, the shared `--token` and an open server are root
and bypass grants; global memories (project `NULL`) stay private to their author as before.

| Default (`MEMDUSA_DEFAULT_ACCESS`) | User without a matching grant |
|---|---|
| `write` (default, backward compatible) | full read/write, as today |
| `read` | may read every project; writing needs a grant |
| `none` (recommended for multi-team servers) | nothing without a grant |

The server prints the effective default next to its auth mode at startup. Set it on the server, e.g.
`MEMDUSA_DEFAULT_ACCESS=none memdusa serve --addr 0.0.0.0:7077`. Grants take effect on the next request (one grants
query per request).

**Grants** match an exact project key or a prefix ending in `/*`:

```sh
memdusa grant alice 'github.com/acme/*' --role write
memdusa grant alice 'github.com/acme/payroll' --role none   # carve out one project
memdusa grants [--user alice]
memdusa revoke-grant <id>                                   # by id, from `grants`
```

Rules: an exact grant beats any prefix; among prefixes the longest wins; `none` removes access even when the default
would allow it. `write` implies read, `read` is read-only, `none` denies. All project-scoped REST endpoints and every
remote MCP tool call enforce the same check (memories, checkpoints, events, resume, hooks and hooks batch, delegate,
runs/steps/claims, review, project export). Run/step endpoints authorize against the run's stored project, not one
supplied by the caller; cross-project listings and exports without `only_project` are admin-only (a single-project
export only needs read). Denials return **403** with the project and the access needed. `memdusa status` /
`GET /v1/whoami` show your effective grants and the server's default policy.

The admin API mirrors the CLI: `POST /v1/admin/grants {pattern, user, role}`,
`GET /v1/admin/grants[?user=]`, `DELETE /v1/admin/grants/{id}`.

**Audit log.** The server records grant/user/key changes, memory forgets, prune, import, export, backfill, runs
finished by someone other than their owner, and every denied request (not ordinary reads or hook writes — those are
already attributed in the data). Each event has an actor, action, project, target, redacted detail JSON and an
`ok`/`denied` outcome.

```sh
memdusa audit                                  # newest 100 events
memdusa audit --since 7d --actor alice --project github.com/acme/shop --limit 50
```

`--since` accepts unix milliseconds or a relative `24h` / `7d` / `2w`. Equivalent API:
`GET /v1/admin/audit?since=&actor=&project=&limit=` (admin only). Events are kept for 365 days by default
(`MEMDUSA_AUDIT_DAYS`, `0` = keep forever) and pruned by the same job that handles data retention. Detail text is
redacted with the same rules as memories.

**Connecting a harness straight to the URL** also works. The optional headers save the agent from passing `project`
on every call:
```sh
claude mcp add --transport http memdusa https://memdusa.example.com/mcp \
  --header "Authorization: Bearer $TOKEN" \
  --header "X-Memdusa-Project: github.com/org/repo" --header "X-Memdusa-Agent: claude-code"
```

## Team server: OAuth sign-in

Set `MEMDUSA_PUBLIC_URL` to the server's external base URL to enable OAuth 2.1 sign-in (it must be `https://`,
except for `localhost` / `127.0.0.1` in development):

```sh
export MEMDUSA_PUBLIC_URL=https://memdusa.example.com
memdusa serve --addr 0.0.0.0:7077 --allowed-host memdusa.example.com
```

What an MCP client (or any OAuth client) sees:

- `GET /.well-known/oauth-protected-resource` (RFC 9728) and `GET /.well-known/oauth-authorization-server` (RFC 8414).
  Unauthenticated `/mcp` and `/v1` requests get `401` with `WWW-Authenticate: Bearer resource_metadata="…"`.
- `POST /oauth/register` (RFC 7591) for public clients: send `redirect_uris` (and optionally `client_name`), get a
  `client_id`. Only `token_endpoint_auth_method: none` is supported. Redirect URIs must be `https://`, or loopback
  `http://127.0.0.1:<any port>/…` / `http://localhost:<any port>/…` (RFC 8252).
- `GET /oauth/authorize` checks `response_type=code`, the registered `redirect_uri`, `code_challenge` with
  `code_challenge_method=S256` (plain PKCE is rejected), `state`, `scope` and the optional `resource` (RFC 8707). It
  renders a minimal server-rendered sign-in page: paste an existing memdusa API key, then approve the client and
  scopes on the consent screen. See "Sign in with GitHub / Google" below for the upstream providers.
- `POST /oauth/token` exchanges the one-time code (10-minute expiry) with the PKCE verifier, and rotates refresh
  tokens on every use. Reusing a rotated refresh token revokes the whole grant chain (and is audited as theft).
  `POST /oauth/revoke` is RFC 7009.

Access tokens are opaque `mdo_…` (default **1 hour**); refresh tokens are `mdr_…` (**30 days sliding**, with a
**365-day absolute** limit from the last interactive sign-in). All are stored hashed like API keys. Scopes are
`memdusa` (full user access, still limited by H1 project grants) and `memdusa:read` (read-only: writes get `403`).
API keys and the shared `--token` keep working unchanged. Lifetimes can be shortened for tests with
`MEMDUSA_OAUTH_ACCESS_SECS`, `MEMDUSA_OAUTH_REFRESH_SECS`, `MEMDUSA_OAUTH_ABSOLUTE_SECS`, `MEMDUSA_OAUTH_CODE_SECS`
and `MEMDUSA_OAUTH_SIGNIN_SECS`. Failed API-key sign-ins are rate-limited per IP.

Sign-ins, registrations, refresh reuse, revocations and denials are written to the H1 audit log (`memdusa audit`).

Sessions (OAuth sign-ins) can be inspected and revoked from a machine logged in to the server:

```sh
memdusa sessions list              # your sessions
memdusa sessions list --user alice # admin: anyone's sessions
memdusa sessions revoke <id>       # revokes that session's whole grant chain
```

The same data is available at `GET/DELETE /v1/me/sessions[/{id}]` and `GET/DELETE /v1/admin/sessions[/{id}]`.

### Sign in from the CLI

On a machine with a browser:

```sh
memdusa login https://memdusa.example.com --browser
```

This discovers the authorization server metadata, registers a public client (the `client_id` is cached per server in
`~/.memdusa/oauth-clients.json`), starts a loopback listener on `127.0.0.1:<random port>`, prints the sign-in URL and
attempts to open it (`open` / `xdg-open` / `start`). Sign in with an API key and approve the scopes; the authorization
code comes back to the loopback listener, is exchanged with PKCE S256 + `state`, and the resulting access/refresh tokens
are saved in `~/.memdusa/config.json` (mode `0600`). Tokens are never printed. If no browser opens, use the printed URL;
if you don't finish in five minutes the command exits with a clear timeout message (`--device` is the alternative).

On SSH/headless machines (RFC 8628 device authorization grant):

```sh
memdusa login https://memdusa.example.com --device
# To sign in, visit:
#   https://memdusa.example.com/oauth/device?user_code=ABCD-EFGH
# and enter the code: ABCD-EFGH
```

The CLI prints the verification URL and short user code, then polls the token endpoint every 5 s, handling
`authorization_pending`, `slow_down`, `expired_token` and `access_denied`. The code expires after 10 minutes. In the
browser, the same sign-in + consent pages are used; approvals, denials, token issuance and audit events are recorded.

`--token mdk_…` (API-key login) is unchanged. To check or undo a sign-in:

```sh
memdusa status      # local/remote, sign-in method (API key or OAuth: user, expires, scopes), reachability, queue
memdusa logout      # revokes the refresh token at the server (RFC 7009) and deletes local tokens
```

Access tokens are refreshed transparently by the REST client and the `memdusa mcp` proxy: before expiry (or on a `401`)
they refresh once, rotation is saved atomically, and a lock file (`~/.memdusa/oauth-refresh.lock`) stops concurrent hook
processes from racing. If the refresh fails — e.g. the session was revoked with `memdusa sessions revoke` — hooks keep
the event in the offline spool and exit successfully, and `memdusa status` tells you to run `memdusa login` again. The
server exposes `POST /oauth/device_authorization`, `GET/POST /oauth/device` and the
`urn:ietf:params:oauth:grant-type:device_code` grant, advertised in its AS metadata. Device codes and user codes are
stored hashed; test lifetimes/polling can be shortened with `MEMDUSA_OAUTH_DEVICE_SECS` and
`MEMDUSA_OAUTH_DEVICE_INTERVAL_SECS`.

### Sign in with GitHub / Google

Each upstream is enabled by setting its client id **and** secret on the server. With `MEMDUSA_PUBLIC_URL` set,
`memdusa serve` prints the exact callback URLs to register at startup:

```sh
export MEMDUSA_PUBLIC_URL=https://memdusa.example.com
export MEMDUSA_OAUTH_GITHUB_CLIENT_ID=Iv1.…          # GitHub OAuth App
export MEMDUSA_OAUTH_GITHUB_CLIENT_SECRET=…
export MEMDUSA_OAUTH_GOOGLE_CLIENT_ID=…apps.googleusercontent.com
export MEMDUSA_OAUTH_GOOGLE_CLIENT_SECRET=…
memdusa serve --addr 0.0.0.0:7077 --allowed-host memdusa.example.com
# register https://memdusa.example.com/oauth/callback/github
# register https://memdusa.example.com/oauth/callback/google
```

- **GitHub**: create an OAuth App (Settings → Developer settings → OAuth Apps). Set the *Authorization callback
  URL* to the printed `/oauth/callback/github` URL. memdusa requests `read:user` (plus `read:org` when org
  allowlisting is configured), sends `state` and PKCE `S256`, exchanges the code at
  `github.com/login/oauth/access_token`, and reads `GET /user`. The subject is the stable numeric user id, the
  display/login is the GitHub login.
- **Google**: create an OAuth client (Google Cloud Console → APIs & Services → Credentials) of type *Web
  application*, with the printed `/oauth/callback/google` URL as an authorized redirect URI. memdusa uses OpenID
  Connect (scopes `openid email`, `state`, PKCE `S256` and `nonce`), verifies the ID token's issuer, audience,
  expiry, nonce and RS256 signature against Google's cached JWKS, and requires `email_verified = true`. The subject
  is the `sub` claim.

The sign-in page shows a **Continue with GitHub / Google** button for each configured provider. The pending memdusa
authorization transaction is carried through the upstream round trip (short-lived transaction cookie + `state`);
the user lands on the same consent screen as API-key sign-in. Upstream access tokens are used once to read the
identity and are **never stored**.

#### Who may sign in

By default only identities linked to a memdusa user may sign in. Unknown identities get a friendly "not linked"
page and are audited.

Link an identity as an admin (the handle is resolved lazily on first sign-in by login/email, or immediately if it
is a numeric id):

```sh
memdusa user link alice github:octocat          # binds on first GitHub sign-in by login
memdusa user link alice github:424242           # binds directly to the numeric user id
memdusa user link alice google:alice@acme.com   # binds on first Google sign-in by verified email
memdusa user identities alice
memdusa user unlink alice github:octocat
```

A user who is already signed in with an API key can also click **Link GitHub / Link Google** on the consent page
to link their own identity. All link/unlink changes are audited.

Optional auto-provisioning (off by default) creates a **non-admin** user on first successful sign-in, named after
the GitHub login or the email local part (de-duplicated, e.g. `alice-2`):

```sh
export MEMDUSA_OAUTH_ALLOW_GITHUB_ORGS=acme,other-org   # active org membership required
export MEMDUSA_OAUTH_ALLOW_GOOGLE_DOMAINS=acme.com      # verified email domain or `hd` claim
```

For hosted-domain accounts, Google's `hd` claim is matched as well as the verified email domain.

Test-only overrides for the upstream endpoints (`MEMDUSA_TEST_GITHUB_AUTH_URL`, `MEMDUSA_TEST_GITHUB_TOKEN_URL`,
`MEMDUSA_TEST_GITHUB_API_URL`, `MEMDUSA_TEST_GOOGLE_DISCOVERY_URL`, `MEMDUSA_TEST_GOOGLE_JWKS_URL`,
`MEMDUSA_TEST_GOOGLE_USERINFO_URL`) point the server at an in-process mock and are used by the test suite; do not
set them in production.

## Team server: sync

Offline-first agents can keep working against a local database and replicate chosen projects to a team server with a
one-shot `memdusa sync`, a background `memdusa sync --watch`, or full sync mode (`memdusa login <url> --sync`) where
hooks and MCP never touch the network. The local side is the logged-in server's opposite number: the local database is
given by `--database-url` (default `~/.memdusa/memdusa.db`), the remote by `memdusa login` (API key **or** OAuth).
Nothing leaves an opted-out project.

```sh
memdusa login https://memdusa.example.com --token mdk_…   # or --browser / --device (see above)
memdusa sync add github.com/acme/shop                     # opt in; first run does a full push+pull
memdusa sync add --all                                    # every project you can read on the server
memdusa sync add --globals                                # opt in your private global memories (separate)
memdusa sync                                              # push local changes, then pull remote ones
memdusa sync list                                         # opted-in projects and cursors
memdusa sync status                                       # last ok, pending changes, last error, conflicts
memdusa sync remove github.com/acme/shop                  # stop syncing (local data is kept)
memdusa memories revisions <id>                           # superseded versions of a memory
```

`memdusa sync add` is best effort: if the server is offline, the opt-in is still recorded and the next `memdusa sync`
performs the initial full sync. Cursors live in the local `sync_state` table; a stable per-server client id in
`~/.memdusa/config.json` marks changes you pushed, so your own echo comes back on pull but is skipped. Pushes are in
batches of at most 500 changes and are idempotent (records have globally unique UUIDv7 ids). Pull pages have a cursor
and `has_more`.

**Conflict rules.** Every memory/event/checkpoint write appends a `changes` row in the same transaction, on both sides.
- Events and checkpoints are insert-if-absent by id: no conflicts, no duplicates on re-push.
- Memories are last-writer-wins by `updated_at`; ties go to the larger id (then larger content for an exact
  millisecond race). The losing version is kept in `memory_revisions` on both sides and shown by `memdusa sync status`
  / `memdusa memories revisions <id>`.
- A delete wins unless the edit is **newer** than the tombstone's time. An older offline edit is ignored (and a stale
  tombstone can't delete a newer edit). Ties favour the delete.
- Push requires H1 write access, pull requires read access; both are audited as `sync.push` / `sync.pull` summaries
  (counts only, never record bodies). A user can't push records attributed to another user; agent names are
  re-qualified as `user/agent` on the server, like hooks are.

**Retention and sync.** Prune deletes old events/checkpoints without writing tombstones. A pull of an `upsert` reads the
current row and skips the change if the row was pruned, so a fresh client can't resurrect pruned data; real deletes
(`op = delete`) need no row and always travel. A pruned local row is likewise skipped when pushing. The change log is
append-only (seq cursors never reuse timestamps); the cursor advances past skipped changes.

**Not synced:** runs/steps (server-authoritative), embeddings (each side embeds its own pulled memories if an embedder
is configured), users/keys/grants/audit/OAuth. Global memories are private to their server-side owner and need their
own opt-in.

### Sync mode (hooks write locally)

`memdusa login <url> --sync` (also with `--token`, `--browser` or `--device`) stores the server plus a flag:
hooks, `memdusa mcp` and all CLI reads/writes use the **local database** (`--database-url`, default
`~/.memdusa/memdusa.db`), so there is no network in the hot path. Turn it on or off later with
`memdusa sync mode on|off`; `memdusa login <url>` without `--sync` keeps plain remote mode.

The first hook in a project that is not opted in adds it automatically (the opt-in and initial seeding of existing
records happen locally; the first sync pushes/pulls them). Set `MEMDUSA_SYNC_AUTO_ADD=0` to disable that and opt in
explicitly with `memdusa sync add <project>` instead.

Delegated work stays server-authoritative even in sync mode: `delegate`, `run_*`, `task_complete` / `task_fail`, and
the SessionStart step claim and heartbeats go straight to the server. Nothing creates a local lease. If the server is
unreachable, the claim fails visibly at the top of the brief notice (telling you no local lease was created) and
heartbeats are skipped silently; ordinary memory and event writes keep working offline.

`memdusa status` in sync mode reports both sides: the local database, whether the server is reachable, opted-in
projects, pending local changes, watcher state and last sync time.

### Background watcher

```sh
memdusa sync --watch                   # push+pull every opted-in project, forever
memdusa sync --watch --interval 30s    # custom interval (default 15s)
memdusa sync status                    # also shows watcher pid and last sync
```

Only one watcher runs per `MEMDUSA_HOME` (a pid lock file); a second instance exits with a message. Sync failures back
off exponentially from 1 s up to 5 minutes and reset after a success. One log line is printed for each pass that
actually moved data; a no-op pass is silent. SIGINT/SIGTERM stops it cleanly and releases the lock.

After SessionEnd — and after Stop when files changed — sync mode spawns a detached `memdusa sync --quiet`. It is skipped
when a watcher holds the lock or a sync ran in the last 10 seconds. Set `MEMDUSA_SYNC_OPPORTUNISTIC=0` to disable it.

To run the watcher as a login service:

```sh
memdusa sync service install           # launchd agent on macOS, systemd user unit on Linux
memdusa sync service uninstall
```

The command prints the file it wrote and how to start/stop it. On Windows it prints Task Scheduler instructions
instead. Tests use `MEMDUSA_SERVICE_DIR` to render into a temp directory and never touch real service directories.

## MCP tools

`resume`, `memory_save`, `memory_update`, `memory_search`, `memory_review`, `memory_forget`, `checkpoint`, `log_event`, `run_start`,
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
| POST | `/v1/embeddings/backfill` `{all?}`: embed memories that lack a vector (admin; the server's embedding config) |
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
- [x] Task-aware brief: memories ranked by the latest checkpoint/prompt, preferences always kept, 4,000-character budget
- [ ] **Easy installation on every platform** (today: download a release archive, `cargo install`, or Docker)
  - ✓ macOS/Linux: a Homebrew tap (`brew install krishnacore/tap/memdusa`) whose formula the release workflow updates
    automatically (version, URLs, SHA-256 per platform); later, a homebrew-core submission (not done yet)
  - ✓ one-line install script (`curl -fsSL …/install.sh | sh`) that picks the right archive, verifies its checksum and
    installs to `~/.local/bin`, plus a PowerShell equivalent for Windows
  - Windows: Scoop bucket and winget manifest, generated per release
  - ✓ `cargo binstall memdusa` metadata pointing at the release archives (publishing to crates.io is still to do)
  - Linux packages (`.deb`/`.rpm`) and an AUR package; container images are already published to GHCR
  - ✓ `memdusa self-update` that uses the same release archives and checksums
- [x] **Multi-agent coordination** (one harness coordinating others, like a lead agent delegating to Claude Code,
      OpenCode or Codex workers via `hc`, or a harness's own subagents). Observed while building memdusa itself:
      when a worker hit its usage limit, its replacement in another harness started from scratch. Ideas:
  - [x] parent/child sessions via env (`MEMDUSA_TASK`, `MEMDUSA_PARENT`): the coordinator's brief gets a
    "Delegated work" section with each worker's status, checkpoint and last words (see "Multi-agent setups"; linking
    from hook fields like `agent_id`/`agent_type` or Hermes `conversation_id` is not done)
  - [x] task-scoped checkpoints by default for workers (`task` = the assignment), so they never overwrite the
    coordinator's checkpoint, and a worker's replacement resumes exactly that task
  - [x] worktree/branch awareness: workers in different worktrees share the project key, but code-state warnings and
    "files changed" are per worktree/branch, not mixed
  - [x] durable runs as the coordination primitive: the coordinator creates a run whose steps are the delegated tasks;
    workers claim steps with leases, so a crashed or rate-limited worker's step is visibly stale and can be
    claimed by another harness, and finished steps are never redone
  - [x] attribution in the brief: a "Who did what" summary per agent (files, commands, task, last active) when
    several agents worked recently
- [x] Semantic memory search (embeddings behind a trait): API provider (OpenAI-compatible, so Ollama works too); a
  built-in local model provider later
- [x] Memory hygiene: update, near-duplicate merge, stale flags
- [x] Memory consolidation: agent-driven review. `memory_review` (or `memdusa review`) finds related memories to merge, memories that mention files changed since, and old todos; the agent merges them with `memory_update` / `memory_forget`, and the brief nudges when there is enough to review. Old events are already summarized by retention.
- [x] Per-user API keys, server-side attribution, private global memories
- [x] Per-project access control (who can read/write which projects), audit log
- [ ] OAuth for remote MCP (so harnesses can connect without pasting a key)
- [x] Local → hosted migration (`export | import`)
- [ ] Continuous local ⇄ hosted sync
