# Architecture

memdusa is one Rust binary built from two crates. This page is a map for contributors: where things live
and how a request flows through them.

## Crates

| Crate | What it holds |
|---|---|
| `crates/memdusa-core` | The domain model and storage: one portable SQL store on `sqlx::Any` (SQLite and Postgres), the resume brief, access control, users and API keys, OAuth storage, the sync protocol, redaction, retention, export/import and memory review. No I/O besides the database. |
| `crates/memdusa` | The binary: the CLI, the MCP server (rmcp, over stdio and streamable HTTP), the REST API and OAuth server (axum), hook handling and the per-harness adapters, installers, remote mode, sync and the background watcher, embedding providers and self-update. |

## Main modules

**memdusa-core**

| Module | Responsibility |
|---|---|
| `store.rs` | The `Store`: schema migrations (append-only `MIGRATIONS`), the `Q` query builder (every value is a bound parameter), memories, events, checkpoints, durable runs and steps. |
| `model.rs` | Types shared by the store, the HTTP API and the MCP server. |
| `brief.rs` | Builds the resume brief: checkpoints, unfinished runs, memories ranked for the current task, recent activity, delegated work. |
| `access.rs` | Per-project grants, the default policy and the audit log. |
| `auth.rs`, `oauth.rs` | Users, API keys and OAuth clients, codes, tokens and grants. Every secret is stored as a SHA-256 hash. |
| `sync.rs` | The change log, last-writer-wins with revisions, tombstones, and push/pull pages. |
| `redact.rs` | Secret redaction, applied before anything is stored. |
| `retention.rs`, `transfer.rs`, `review.rs`, `embed.rs` | Pruning, JSON Lines export/import, memory review, and the embedding trait. |

**memdusa**

| Module | Responsibility |
|---|---|
| `main.rs` | The CLI (clap) and the hook entry point. |
| `hooks.rs` | The canonical hook pipeline: session start (brief, task claim), prompts (journal, recall), tool use, stop (checkpoint reminder), compaction and session end (fallback checkpoints). |
| `adapters.rs` | Translates each harness's hook payload to the canonical one, and the output back. |
| `install.rs`, `hermes_yaml.rs` | `memdusa install`/`uninstall` for every harness. |
| `mcp.rs` | The MCP tools, resources and prompts. |
| `http.rs`, `oauth.rs`, `oauth_upstream.rs`, `ops.rs` | `memdusa serve`: REST API, MCP over HTTP, OAuth 2.1 server with GitHub/Google sign-in, rate limiting and metrics. |
| `remote.rs`, `proxy.rs`, `spool.rs`, `config.rs`, `cli_auth.rs` | Remote mode: the REST client, the stdio-to-HTTP MCP proxy, the offline queue, the saved login, and CLI sign-in. |
| `sync.rs`, `watch.rs`, `service.rs` | Sync mode, the background watcher and its launchd/systemd service. |
| `embedder.rs`, `local_embed.rs` | Embedding providers: OpenAI-compatible HTTP and the built-in local model. |

## How a hook event flows

1. The harness runs `memdusa hook <event> --harness <name>` with its JSON payload on stdin.
2. `adapters.rs` turns the payload into memdusa's canonical format (or ignores the event).
3. Secrets are redacted on the client.
4. Depending on the mode:
   - **Local:** `hooks.rs` handles the event against the local database.
   - **Remote:** the event is sent to the team server, which runs the same handler. If the server is
     unreachable, the event waits in the offline queue.
   - **Sync:** the event is written locally and replicated later.
5. The handler's output (the brief, recall or a checkpoint reminder) is rendered back in the harness's
   format.

A hook never breaks the session: failures are logged and the hook exits 0.

## Design rules

- One portable SQL implementation for SQLite and Postgres; schema changes only append to `MIGRATIONS`.
- Every value in SQL is a bound parameter (`Q::text`, `Q::int`), never formatted into the string.
- Every write is redacted, and every credential is stored hashed.
- Authorization is checked against the stored record's project, never a project the caller supplies.
