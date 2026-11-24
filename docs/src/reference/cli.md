# CLI reference

Every `memdusa` command with its options, generated from `memdusa --help` by
`scripts/gen-cli-docs.sh` (CI checks that this page is up to date). For what the commands are for,
start with the [quick start](../getting-started/quick-start.md) and the guides.

## `memdusa`

```text
Universal memory & durable execution for AI agents and harnesses

Usage: memdusa [OPTIONS] <COMMAND>

Commands:
  mcp                Run the MCP server over stdio (what a local harness launches)
  serve              Run the HTTP server: REST API at /v1 and MCP (streamable HTTP) at /mcp
  prune              Summarize old hook activity and delete superseded checkpoints and finished runs
  resume             Print the hand-off brief for the current project
  delegate           Coordinator: delegate tasks as one durable run and print the env line to launch each worker with
  task               Worker: finish the task this agent was launched with (MEMDUSA_RUN + MEMDUSA_TASK)
  remember           Save a memory from the command line
  recall             Search memories
  review             List memories that need consolidating: related ones to merge, possibly outdated ones, old todos
  embed              Compute embeddings for memories that lack one for the current model (semantic search, MEMDUSA_EMBED_URL)
  project            Print the detected project key
  login              Use a remote memdusa server for hooks, MCP and the CLI on this machine
  logout             Forget the remote server and go back to the local database
  status             Show whether this machine uses a local database or a remote server, and check it
  sync               Replicate opted-in projects between the local database and the team server. Without a subcommand: push local changes, then pull remote ones (one shot)
  memories           Inspect memories (superseded versions kept by sync conflicts)
  export             Write all data as JSON Lines (memories, events, checkpoints, runs; not users or keys)
  import             Load a `memdusa export` file. Idempotent: existing records are skipped
  backup             Consistent online copy of the local SQLite database
  user               Manage users (admin). Acts on the local database, or on the logged-in server
  key                Manage API keys: your own, or (as admin, with --user) anyone's
  grant              Grant a user access to a project key or a prefix ending in `/*` (admin)
  grants             List project grants (admin)
  revoke-grant       Revoke a project grant by id (admin)
  sessions           List or revoke OAuth sign-in sessions (remote mode)
  audit              Show the audit log (admin)
  hook               Handle a Claude Code hook event (JSON on stdin). Installed by `memdusa install`
  git-hook           Record a git hook event. Installed by `memdusa install git`
  import-transcript  Import saved agent sessions (work done before memdusa was installed, or without hooks)
  install            Wire memdusa into a harness: installs its hooks and registers the MCP server
  uninstall          Remove memdusa's hooks (and MCP registration) from a harness
  self-update        Update memdusa from the latest GitHub release (or a specific version)
  help               Print this message or the help of the given subcommand(s)

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
  -V, --version                      Print version
```

## `memdusa mcp`

```text
Run the MCP server over stdio (what a local harness launches)

Usage: memdusa mcp [OPTIONS]

Options:
      --agent <AGENT>                Name of the agent/harness, recorded as author and lease owner [env: MEMDUSA_AGENT=] [default: agent]
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa serve`

```text
Run the HTTP server: REST API at /v1 and MCP (streamable HTTP) at /mcp

Usage: memdusa serve [OPTIONS]

Options:
      --addr <ADDR>                   [env: MEMDUSA_ADDR=] [default: 127.0.0.1:7077]
      --database-url <DATABASE_URL>   Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>             Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
      --token <TOKEN>                 Require `Authorization: Bearer <token>` on every request [env: MEMDUSA_TOKEN=]
      --allowed-host <ALLOWED_HOSTS>  Host header values the server accepts (DNS-rebinding protection; `/health` and loopback hosts always pass). Without it, /mcp accepts localhost only and the rest of the server any host [env: MEMDUSA_ALLOWED_HOSTS=]
      --no-auto-prune                 Don't prune old data automatically (otherwise once a day, see `memdusa prune`) [env: MEMDUSA_NO_AUTO_PRUNE=]
      --rate-limit <RATE_LIMIT>       Requests per second per user (or IP), with bursts up to 10x. 0 = no limit [env: MEMDUSA_RATE_LIMIT=] [default: 50]
      --no-metrics                    Don't serve Prometheus metrics at /metrics [env: MEMDUSA_NO_METRICS=]
  -h, --help                          Print help
```

## `memdusa prune`

```text
Summarize old hook activity and delete superseded checkpoints and finished runs

Usage: memdusa prune [OPTIONS]

Options:
      --database-url <DATABASE_URL>
          Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --dry-run
          Show what would be pruned without changing anything
      --only-project <ONLY_PROJECT>
          Only this project (default: all)
      --project <PROJECT>
          Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
      --activity-days <ACTIVITY_DAYS>
          Days of raw hook activity to keep before summarizing it [env: MEMDUSA_RETENTION_ACTIVITY_DAYS=] [default: 30]
      --checkpoint-days <CHECKPOINT_DAYS>
          Days to keep superseded checkpoints (the latest per task is always kept) [env: MEMDUSA_RETENTION_CHECKPOINT_DAYS=] [default: 90]
      --run-days <RUN_DAYS>
          Days to keep finished runs [env: MEMDUSA_RETENTION_RUN_DAYS=] [default: 90]
  -h, --help
          Print help
```

## `memdusa resume`

```text
Print the hand-off brief for the current project

Usage: memdusa resume [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --json                         Print JSON instead of Markdown
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa delegate`

```text
Coordinator: delegate tasks as one durable run and print the env line to launch each worker with

Usage: memdusa delegate [OPTIONS] <NAME> [TASKS]...

Arguments:
  <NAME>      Name of the piece of work, e.g. "windows release"
  [TASKS]...  One short, stable name per task (becomes the worker's MEMDUSA_TASK). Append `=<assignment>` to store the coordinator's assignment for that task, e.g. `refunds="build refunds"`

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --from-file <FROM_FILE>        Read the tasks from a JSON file: `[{"task": "refunds", "assignment": "…"}]` (`-` for stdin)
      --key <KEY>                    Reuse the run with this idempotency key (default: the running run with the same name)
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa task`

```text
Worker: finish the task this agent was launched with (MEMDUSA_RUN + MEMDUSA_TASK)

Usage: memdusa task [OPTIONS] <COMMAND>

Commands:
  done  Mark the task done (it is never redone: a worker started on it later is told so)
  fail  Give the task up; a replacement worker can claim it again
  help  Print this message or the help of the given subcommand(s)

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa task done`

```text
Mark the task done (it is never redone: a worker started on it later is told so)

Usage: memdusa task done [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --summary <SUMMARY>            What was done and where to find it; shown to the coordinator
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa task fail`

```text
Give the task up; a replacement worker can claim it again

Usage: memdusa task fail [OPTIONS] <REASON>

Arguments:
  <REASON>  Why it can't be finished

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa remember`

```text
Save a memory from the command line

Usage: memdusa remember [OPTIONS] <CONTENT>

Arguments:
  <CONTENT>

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --kind <KIND>                  [default: fact]
      --global
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa recall`

```text
Search memories

Usage: memdusa recall [OPTIONS] [QUERY]

Arguments:
  [QUERY]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa review`

```text
List memories that need consolidating: related ones to merge, possibly outdated ones, old todos

Usage: memdusa review [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa embed`

```text
Compute embeddings for memories that lack one for the current model (semantic search, MEMDUSA_EMBED_URL)

Usage: memdusa embed [OPTIONS]

Options:
      --all                          Re-embed every memory, not just the missing or outdated ones
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --download-model               Download the local embedding model first (once, into <MEMDUSA_HOME>/models/). Only needed with MEMDUSA_EMBED_URL=local; hooks never download automatically
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
      --only-project <ONLY_PROJECT>  Only this project (default: all)
  -h, --help                         Print help
```

## `memdusa project`

```text
Print the detected project key

Usage: memdusa project [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa login`

```text
Use a remote memdusa server for hooks, MCP and the CLI on this machine

Usage: memdusa login [OPTIONS] <URL>

Arguments:
  <URL>  Server URL, e.g. https://memdusa.example.com

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --token <TOKEN>                Bearer token (as set with `memdusa serve --token`) [env: MEMDUSA_TOKEN]
      --browser                      Sign in with a browser: authorization code + PKCE on a loopback redirect
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
      --device                       Sign in without a browser (SSH/headless) using a short user code
      --sync                         Sync mode: hooks, MCP and CLI use the local database; only delegated work and background sync use the server
  -h, --help                         Print help
```

## `memdusa logout`

```text
Forget the remote server and go back to the local database

Usage: memdusa logout [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa status`

```text
Show whether this machine uses a local database or a remote server, and check it

Usage: memdusa status [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa sync`

```text
Replicate opted-in projects between the local database and the team server. Without a subcommand: push local changes, then pull remote ones (one shot)

Usage: memdusa sync [OPTIONS] [COMMAND]

Commands:
  add      Opt into syncing a project (or all readable projects / your globals). The first sync does a full push and pull of everything already there
  remove   Stop syncing a project (local data is kept)
  list     List the projects opted into sync for the logged-in server
  status   Per-project cursors, pending changes, last error and recent conflicts
  mode     Turn sync mode on or off for this login
  service  Install or remove a service that runs `memdusa sync --watch`
  help     Print this message or the help of the given subcommand(s)

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
      --watch                        Keep looping push+pull for all opted-in projects
      --interval <INTERVAL>          How often to sync in watch mode (e.g. 15s, 1m) [default: 15s]
      --quiet                        One sync pass with no per-project output (used by hooks and service helpers)
  -h, --help                         Print help
```

### `memdusa sync add`

```text
Opt into syncing a project (or all readable projects / your globals). The first sync does a full push and pull of everything already there

Usage: memdusa sync add [OPTIONS] [PROJECT]

Arguments:
  [PROJECT]

Options:
      --all                          Every project you can read on the server
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --globals                      Your private global memories (opt-in, separate from projects)
  -h, --help                         Print help
```

### `memdusa sync remove`

```text
Stop syncing a project (local data is kept)

Usage: memdusa sync remove [OPTIONS] [PROJECT]

Arguments:
  [PROJECT]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --globals
  -h, --help                         Print help
```

### `memdusa sync list`

```text
List the projects opted into sync for the logged-in server

Usage: memdusa sync list [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa sync status`

```text
Per-project cursors, pending changes, last error and recent conflicts

Usage: memdusa sync status [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa sync mode`

```text
Turn sync mode on or off for this login

Usage: memdusa sync mode [OPTIONS] <VALUE>

Arguments:
  <VALUE>  `on` or `off`

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa sync service`

```text
Install or remove a service that runs `memdusa sync --watch`

Usage: memdusa sync service [OPTIONS] <COMMAND>

Commands:
  install    Write the service file and print how to start it
  uninstall  Remove the service file and print how to stop it
  help       Print this message or the help of the given subcommand(s)

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

#### `memdusa sync service install`

```text
Write the service file and print how to start it

Usage: memdusa sync service install [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

#### `memdusa sync service uninstall`

```text
Remove the service file and print how to stop it

Usage: memdusa sync service uninstall [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa memories`

```text
Inspect memories (superseded versions kept by sync conflicts)

Usage: memdusa memories [OPTIONS] <COMMAND>

Commands:
  revisions  Show superseded versions of a memory, newest first
  help       Print this message or the help of the given subcommand(s)

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa memories revisions`

```text
Show superseded versions of a memory, newest first

Usage: memdusa memories revisions [OPTIONS] <ID>

Arguments:
  <ID>

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa export`

```text
Write all data as JSON Lines (memories, events, checkpoints, runs; not users or keys)

Usage: memdusa export [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --only-project <ONLY_PROJECT>  Only this project (default: everything)
  -o, --output <OUTPUT>              Output file (default: stdout)
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa import`

```text
Load a `memdusa export` file. Idempotent: existing records are skipped

Usage: memdusa import [OPTIONS] <FILE>

Arguments:
  <FILE>  File to read, or `-` for stdin

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa backup`

```text
Consistent online copy of the local SQLite database

Usage: memdusa backup [OPTIONS] <PATH>

Arguments:
  <PATH>  Destination file (must not exist)

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa user`

```text
Manage users (admin). Acts on the local database, or on the logged-in server

Usage: memdusa user [OPTIONS] <COMMAND>

Commands:
  add         Create a user and their first API key (printed once)
  list
  disable     Disable a user: all their keys stop working immediately
  enable
  link        Link an external identity (lazy: binds on first sign-in by login/email). Example: github:octocat, github:12345, google:alice@acme.com
  unlink      Remove a linked or pending external identity
  identities  List a user's linked external identities
  help        Print this message or the help of the given subcommand(s)

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa user add`

```text
Create a user and their first API key (printed once)

Usage: memdusa user add [OPTIONS] <NAME>

Arguments:
  <NAME>

Options:
      --admin
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --key-name <KEY_NAME>          Label for the first key [default: default]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa user list`

```text
Usage: memdusa user list [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa user disable`

```text
Disable a user: all their keys stop working immediately

Usage: memdusa user disable [OPTIONS] <NAME>

Arguments:
  <NAME>

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa user enable`

```text
Usage: memdusa user enable [OPTIONS] <NAME>

Arguments:
  <NAME>

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa user link`

```text
Link an external identity (lazy: binds on first sign-in by login/email). Example: github:octocat, github:12345, google:alice@acme.com

Usage: memdusa user link [OPTIONS] <USER> <IDENTITY>

Arguments:
  <USER>
  <IDENTITY>

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa user unlink`

```text
Remove a linked or pending external identity

Usage: memdusa user unlink [OPTIONS] <USER> <IDENTITY>

Arguments:
  <USER>
  <IDENTITY>

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa user identities`

```text
List a user's linked external identities

Usage: memdusa user identities [OPTIONS] <USER>

Arguments:
  <USER>

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa key`

```text
Manage API keys: your own, or (as admin, with --user) anyone's

Usage: memdusa key [OPTIONS] <COMMAND>

Commands:
  create  Create a key (printed once). Remote: your own unless --user. Local: --user is required
  list    List keys. Remote: your own unless --user. Local: everyone's unless --user
  revoke  Revoke a key by id or by its prefix (e.g. mdk_1a2b3c4d)
  help    Print this message or the help of the given subcommand(s)

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa key create`

```text
Create a key (printed once). Remote: your own unless --user. Local: --user is required

Usage: memdusa key create [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --user <USER>
      --name <NAME>                  Label, e.g. "laptop" or "ci" [default: default]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa key list`

```text
List keys. Remote: your own unless --user. Local: everyone's unless --user

Usage: memdusa key list [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --user <USER>
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa key revoke`

```text
Revoke a key by id or by its prefix (e.g. mdk_1a2b3c4d)

Usage: memdusa key revoke [OPTIONS] <KEY>

Arguments:
  <KEY>

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa grant`

```text
Grant a user access to a project key or a prefix ending in `/*` (admin)

Usage: memdusa grant [OPTIONS] <USER> <PATTERN>

Arguments:
  <USER>
  <PATTERN>

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --role <ROLE>                  [default: write]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa grants`

```text
List project grants (admin)

Usage: memdusa grants [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --user <USER>
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa revoke-grant`

```text
Revoke a project grant by id (admin)

Usage: memdusa revoke-grant [OPTIONS] <ID>

Arguments:
  <ID>

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa sessions`

```text
List or revoke OAuth sign-in sessions (remote mode)

Usage: memdusa sessions [OPTIONS] <COMMAND>

Commands:
  list    List active sign-in sessions (your own, or everyone's with --user)
  revoke  Revoke a session by id (revokes its whole grant chain)
  help    Print this message or the help of the given subcommand(s)

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa sessions list`

```text
List active sign-in sessions (your own, or everyone's with --user)

Usage: memdusa sessions list [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --user <USER>
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

### `memdusa sessions revoke`

```text
Revoke a session by id (revokes its whole grant chain)

Usage: memdusa sessions revoke [OPTIONS] <ID>

Arguments:
  <ID>

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa audit`

```text
Show the audit log (admin)

Usage: memdusa audit [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --since <SINCE>                Events at or after this unix-millis timestamp, or `7d` / `24h` / `2w` ago
      --actor <ACTOR>
      --project <PROJECT>
      --limit <LIMIT>                [default: 100]
  -h, --help                         Print help
```

## `memdusa hook`

```text
Handle a Claude Code hook event (JSON on stdin). Installed by `memdusa install`

Usage: memdusa hook [OPTIONS] <EVENT>

Arguments:
  <EVENT>
          Possible values:
          - session-start
          - user-prompt-submit
          - post-tool-use
          - stop
          - pre-compact
          - post-compact:       Claude Code: after compaction, with the summary the harness generated
          - session-end
          - git:                Commits, branch switches and merges, collected by `memdusa git-hook`
          - mcp-approval:       PreToolUse hook that auto-approves calls to memdusa's own MCP server (Antigravity)

Options:
      --database-url <DATABASE_URL>
          Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db]

          [env: MEMDUSA_DATABASE_URL=]

      --harness <HARNESS>
          Which harness is calling (decides the payload and output format)

          Possible values:
          - claude-code
          - codex
          - cursor
          - gemini
          - antigravity: Google Antigravity CLI (`agy`), Gemini CLI's successor for consumer accounts
          - opencode:    OpenCode v2: a TypeScript plugin that calls `memdusa hook` with canonical payloads
          - pi:          pi coding agent: a TypeScript extension, like OpenCode's plugin
          - hermes:      Nous Research's Hermes Agent: shell hooks in ~/.hermes/config.yaml
          - git:         Git hooks (post-commit, post-checkout, post-merge) in the repo's hooks directory

          [default: claude-code]

      --agent <AGENT>
          Agent name for attribution. Default: the harness name

          [env: MEMDUSA_AGENT=]

      --project <PROJECT>
          Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd

          [env: MEMDUSA_PROJECT=]

      --no-prompts
          Don't journal user prompts

          [env: MEMDUSA_NO_PROMPTS=]

      --stop-guard-mins <STOP_GUARD_MINS>
          Ask for a checkpoint on Stop when files changed and the last one is older than N minutes (0 = off)

          [env: MEMDUSA_STOP_GUARD_MINS=]
          [default: 15]

  -h, --help
          Print help (see a summary with '-h')
```

## `memdusa git-hook`

```text
Record a git hook event. Installed by `memdusa install git`

Usage: memdusa git-hook [OPTIONS] <HOOK> [ARGS]...

Arguments:
  <HOOK>     [possible values: post-commit, post-checkout, post-merge]
  [ARGS]...  Arguments git passes to the hook (checkout: prev, new, flag; merge: squash)

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

## `memdusa import-transcript`

```text
Import saved agent sessions (work done before memdusa was installed, or without hooks)

Usage: memdusa import-transcript [OPTIONS] <HARNESS> [FILES]...

Arguments:
  <HARNESS>   [possible values: claude-code, codex, pi]
  [FILES]...  Transcript files

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --latest                       The most recent session for the current directory's repo
      --force                        Import sessions that were already imported (or recorded by hooks)
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
      --dry-run                      Print what would be imported without writing anything
  -h, --help                         Print help
```

## `memdusa install`

```text
Wire memdusa into a harness: installs its hooks and registers the MCP server

Usage: memdusa install [OPTIONS] <HARNESS>

Arguments:
  <HARNESS>
          Possible values:
          - claude-code
          - codex
          - cursor
          - gemini
          - antigravity: Google Antigravity CLI (`agy`), Gemini CLI's successor for consumer accounts
          - opencode:    OpenCode v2: a TypeScript plugin that calls `memdusa hook` with canonical payloads
          - pi:          pi coding agent: a TypeScript extension, like OpenCode's plugin
          - hermes:      Nous Research's Hermes Agent: shell hooks in ~/.hermes/config.yaml
          - git:         Git hooks (post-commit, post-checkout, post-merge) in the repo's hooks directory

Options:
      --database-url <DATABASE_URL>
          Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db]

          [env: MEMDUSA_DATABASE_URL=]

      --scope <SCOPE>
          Possible values:
          - user:    Your user config: every project on this machine
          - project: This project's config, committed and shared with the team
          - local:   This project, just you (Claude Code only: .claude/settings.local.json)

          [default: user]

      --project <PROJECT>
          Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd

          [env: MEMDUSA_PROJECT=]

      --settings <SETTINGS>
          Settings file to edit instead of the scope's default

      --bin <BIN>
          Command to invoke memdusa with. Default: absolute path of this binary

      --no-mcp
          Only install hooks; don't register the MCP server

      --dry-run
          Print the resulting settings without writing anything

  -h, --help
          Print help (see a summary with '-h')
```

## `memdusa uninstall`

```text
Remove memdusa's hooks (and MCP registration) from a harness

Usage: memdusa uninstall [OPTIONS] <HARNESS>

Arguments:
  <HARNESS>
          Possible values:
          - claude-code
          - codex
          - cursor
          - gemini
          - antigravity: Google Antigravity CLI (`agy`), Gemini CLI's successor for consumer accounts
          - opencode:    OpenCode v2: a TypeScript plugin that calls `memdusa hook` with canonical payloads
          - pi:          pi coding agent: a TypeScript extension, like OpenCode's plugin
          - hermes:      Nous Research's Hermes Agent: shell hooks in ~/.hermes/config.yaml
          - git:         Git hooks (post-commit, post-checkout, post-merge) in the repo's hooks directory

Options:
      --database-url <DATABASE_URL>
          Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db]

          [env: MEMDUSA_DATABASE_URL=]

      --scope <SCOPE>
          Possible values:
          - user:    Your user config: every project on this machine
          - project: This project's config, committed and shared with the team
          - local:   This project, just you (Claude Code only: .claude/settings.local.json)

          [default: user]

      --project <PROJECT>
          Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd

          [env: MEMDUSA_PROJECT=]

      --settings <SETTINGS>


      --no-mcp


  -h, --help
          Print help (see a summary with '-h')
```

## `memdusa self-update`

```text
Update memdusa from the latest GitHub release (or a specific version)

Usage: memdusa self-update [OPTIONS]

Options:
      --database-url <DATABASE_URL>  Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server. [default: the logged-in server if any, else ~/.memdusa/memdusa.db] [env: MEMDUSA_DATABASE_URL=]
      --version <VERSION>            Version to install, e.g. v0.1.1 (default: the latest release)
      --check                        Only report whether an update is available; don't install anything
      --project <PROJECT>            Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd [env: MEMDUSA_PROJECT=]
  -h, --help                         Print help
```

