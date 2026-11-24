# Configuration

memdusa is configured with command-line flags and environment variables. Every variable here can be set
in your shell, in a service file, or in a harness's environment. When a flag and its variable are both
set, the flag wins. For each command's flags, see the [CLI reference](cli.md).

Values that mean "on/off" accept `1`/`0`, `true`/`false`, `on`/`off` and `yes`/`no` unless noted.

## Where data and settings live

| Variable | Default | Meaning |
|---|---|---|
| `MEMDUSA_HOME` | `~/.memdusa` | memdusa's own directory: the local database, the saved login (`config.json`), the offline queue, logs and extra redaction patterns. Created private to your user (`0700`). |
| `MEMDUSA_DATABASE_URL` | `sqlite:` path to `$MEMDUSA_HOME/memdusa.db` | The database for local mode and for `memdusa serve`: `sqlite:/path/to/file.db` or `postgres://user:pass@host/db`. Same as `--database-url`. |
| `MEMDUSA_PROJECT` | detected | Override the project key. By default it is the normalized git remote (`github.com/acme/shop`), else the repository root, else the current directory. Same as `--project`. |

## Connecting to a team server

These apply on a machine that uses a remote server (see [Team server](../team/index.md)).

| Variable | Default | Meaning |
|---|---|---|
| `MEMDUSA_URL` | from `memdusa login` | The server URL. Overrides the saved login. Must be `https://` unless the host is loopback. |
| `MEMDUSA_TOKEN` | from `memdusa login` | An API key (`mdk_…`) for the server. Overrides the saved one. On `memdusa serve` it is instead the shared bearer token the server accepts (`--token`). |
| `MEMDUSA_ALLOW_INSECURE_HTTP` | unset | Set to `1` to allow `http://` to a non-loopback server on a network you trust. Without it, memdusa refuses to send tokens over plain HTTP. |
| `MEMDUSA_LOGIN_TIMEOUT_SECS` | `300` | How long `memdusa login` waits for the browser sign-in to finish. |

## Hooks and sessions

| Variable | Default | Meaning |
|---|---|---|
| `MEMDUSA_AGENT` | `agent` (hooks: the harness name) | The name recorded as the source of events and memories. `memdusa install` sets it per harness. |
| `MEMDUSA_RECALL` | on | Set to `off` to stop injecting relevant memories into each prompt. The resume brief at session start is unaffected. |
| `MEMDUSA_NO_PROMPTS` | off | Don't journal the user's prompts (`--no-prompts` on `memdusa hook`). |
| `MEMDUSA_STOP_GUARD_MINS` | `15` | After an agent changes files, how many minutes may pass before the stop hook asks it to save a checkpoint. `0` turns the reminder off. |
| `MEMDUSA_OPENCODE_API` | detected | `1` or `2`: which OpenCode plugin API `memdusa install opencode` targets. Normally detected from `opencode --version`. |

## Multi-agent work

Set by a coordinator for each worker it launches. See [Multi-agent work and delegation](../guides/multi-agent.md).

| Variable | Default | Meaning |
|---|---|---|
| `MEMDUSA_RUN` | unset | The durable run a worker belongs to (printed by `memdusa delegate`). |
| `MEMDUSA_TASK` | unset | The worker's task: the step of the run it claims at session start. |
| `MEMDUSA_PARENT` | unset | Free text naming the coordinator, shown in its "Who did what" summary. Optional. |
| `MEMDUSA_STEP_LEASE_SECS` | `900` | How long a worker's claim on its task lasts without activity. After it expires the task counts as stale and a replacement can take it over. |

## Semantic search

See [Semantic search](../guides/semantic-search.md).

| Variable | Default | Meaning |
|---|---|---|
| `MEMDUSA_EMBED_URL` | unset (keyword search only) | An OpenAI-compatible embeddings endpoint, or `local` / `local:<model>` for the built-in model. |
| `MEMDUSA_EMBED_PROVIDER` | from the URL | `local` to use the built-in model; anything else means an HTTP provider. |
| `MEMDUSA_EMBED_MODEL` | `text-embedding-3-small` (HTTP), `all-MiniLM-L6-v2` (local) | The embedding model. |
| `MEMDUSA_EMBED_API_KEY` | `OPENAI_API_KEY` | API key for the HTTP provider. |
| `MEMDUSA_EMBED_RECALL_MIN` | `0.4` (built-in model), `0.5` (others) | Minimum cosine similarity for a memory to be recalled into a prompt. |

## Secret redaction

See [Secret redaction](../guides/redaction.md).

| Variable | Default | Meaning |
|---|---|---|
| `MEMDUSA_REDACT` | on | Set to `off` to disable redaction (not recommended). |
| `MEMDUSA_REDACT_PATTERNS` | unset | Extra regular expressions to redact, one per line. They are added to those in `$MEMDUSA_HOME/redact-patterns.txt`. |

## Retention

See [Retention and backups](../guides/retention.md). The same values are flags on `memdusa prune`.

| Variable | Default | Meaning |
|---|---|---|
| `MEMDUSA_RETENTION_ACTIVITY_DAYS` | `30` | Hook activity older than this is summarized and deleted. |
| `MEMDUSA_RETENTION_CHECKPOINT_DAYS` | `90` | Superseded checkpoints older than this are deleted. |
| `MEMDUSA_RETENTION_RUN_DAYS` | `90` | Finished durable runs older than this are deleted. |
| `MEMDUSA_AUDIT_DAYS` | `365` | Team server: how long audit events are kept. |

## Sync

See [Sync](../team/sync.md).

| Variable | Default | Meaning |
|---|---|---|
| `MEMDUSA_SYNC_AUTO_ADD` | on | In sync mode, opt a project into sync automatically the first time a session runs in it. `0` turns this off. |
| `MEMDUSA_SYNC_OPPORTUNISTIC` | on | In sync mode without a running watcher, start a background sync when a session stops after changing files (at most once every ten seconds). `0` turns this off. |

## Team server (`memdusa serve`)

See [Deploying the server](../team/deploy.md) and [Operations](../team/operations.md).

| Variable | Default | Meaning |
|---|---|---|
| `MEMDUSA_ADDR` | `127.0.0.1:7077` | Address to listen on (`--addr`). Use `0.0.0.0:7077` in a container. |
| `MEMDUSA_TOKEN` | unset | A shared bearer token every request must carry (`--token`). Users and API keys are the better option for a team. |
| `MEMDUSA_ALLOWED_HOSTS` | unset | Comma-separated `Host` values the server accepts (`--allowed-host`), for DNS-rebinding protection. `/health` and loopback hosts always pass. |
| `MEMDUSA_DEFAULT_ACCESS` | `write` | Access for users without a matching grant: `write`, `read` or `none`. See [Access control](../team/access-control.md). |
| `MEMDUSA_RATE_LIMIT` | `50` | Requests per second per user (or IP), with bursts up to 10×. `0` disables it. |
| `MEMDUSA_NO_AUTO_PRUNE` | off | Don't prune old data automatically (otherwise once a day). |
| `MEMDUSA_NO_METRICS` | off | Don't serve Prometheus metrics at `/metrics`. |
| `MEMDUSA_LOG_FORMAT` | text | `json` for one JSON object per log line. |

### OAuth sign-in

Setting `MEMDUSA_PUBLIC_URL` turns on the OAuth server. See [OAuth sign-in](../team/oauth.md).

| Variable | Default | Meaning |
|---|---|---|
| `MEMDUSA_PUBLIC_URL` | unset | The server's public URL (`https://memdusa.example.com`). Must be `https://` unless it is a loopback address. |
| `MEMDUSA_OAUTH_GITHUB_CLIENT_ID`, `MEMDUSA_OAUTH_GITHUB_CLIENT_SECRET` | unset | Enable "Continue with GitHub". |
| `MEMDUSA_OAUTH_GOOGLE_CLIENT_ID`, `MEMDUSA_OAUTH_GOOGLE_CLIENT_SECRET` | unset | Enable "Continue with Google". |
| `MEMDUSA_OAUTH_ALLOW_GITHUB_ORGS` | unset | Comma-separated GitHub organizations whose active members get an account on first sign-in. |
| `MEMDUSA_OAUTH_ALLOW_GOOGLE_DOMAINS` | unset | Comma-separated Google Workspace domains whose verified users get an account on first sign-in. |
| `MEMDUSA_OAUTH_ACCESS_SECS` | `3600` (1 hour) | Access token lifetime. |
| `MEMDUSA_OAUTH_REFRESH_SECS` | `2592000` (30 days) | Refresh token lifetime; each use renews it. |
| `MEMDUSA_OAUTH_ABSOLUTE_SECS` | `31536000` (365 days) | How long a session can be refreshed before you must sign in again. |
| `MEMDUSA_OAUTH_CODE_SECS` | `600` | Authorization code lifetime. |
| `MEMDUSA_OAUTH_SIGNIN_SECS` | `600` | How long a sign-in page stays valid. |
| `MEMDUSA_OAUTH_DEVICE_SECS` | `600` | Device code lifetime (`memdusa login --device`). |
| `MEMDUSA_OAUTH_DEVICE_INTERVAL_SECS` | `5` | Minimum polling interval for device codes. |

## Installers

Used by `install.sh` and `install.ps1`.

| Variable | Default | Meaning |
|---|---|---|
| `MEMDUSA_VERSION` | latest release | Install a specific version, e.g. `v0.1.12`. |
| `MEMDUSA_INSTALL_DIR` | `~/.local/bin` (Windows: `%LOCALAPPDATA%\memdusa\bin`) | Where to put the binary. |
