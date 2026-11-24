# Team server

A team server is one `memdusa serve` process that several people and agents share. It stores memory, checkpoints,
events and durable runs in one database (SQLite or Postgres) and gives each caller an identity, an API key and
per-project access. You need one when memory should outlive a single machine, a session or one harness.

If you only use memdusa on one machine, you do not need a server: the CLI, hooks and MCP work against a local
SQLite database by default. See [Installation](../getting-started/installation.md) to get started locally.

## What the server adds

| | Local only | With a team server |
|---|---|---|
| Identity | none | every request is attributed to a user and an agent |
| API keys | none | per-user `mdk_…` keys, revocable individually |
| Access control | none | per-project grants and a default policy |
| Global memories | private to you | private to their author, even on a shared server |
| Durable runs and delegated work | one machine | one owner, with leases that survive a crashed agent |
| Audit log | local changes only | user, key, grant, OAuth and denial events |

## Three ways to work

The same `memdusa` binary works in three modes. The mode is a property of the *machine*, chosen by `memdusa login`
(and the saved login in `~/.memdusa/config.json`). It is not a setting you pass to every hook.

| Mode | How you enter it | Where hooks, MCP and the CLI read and write |
|---|---|---|
| Local (default) | no `memdusa login` (or `--database-url` is given) | the local SQLite database |
| Remote mode | `memdusa login https://memdusa.example.com` | the server, over the REST API and `/mcp` |
| Sync mode | `memdusa login https://memdusa.example.com --sync` | the local database; chosen projects replicate to the server in the background |

You can switch modes at any time: `memdusa logout` returns to local mode, `memdusa login <url>` uses plain remote
mode, and `memdusa sync mode on|off` toggles sync mode on an existing login. `memdusa status` tells you which mode
this machine is in, whether the server is reachable and whether any events are waiting in the offline queue. For the
full flag list, see the [CLI reference](../reference/cli.md).

## Set up remote mode

On the server, create a user and start the HTTP service:

```sh
memdusa user add alice --admin              # prints alice's first API key once
memdusa serve --addr 0.0.0.0:7077 --allowed-host memdusa.example.com
```

On each machine:

```sh
memdusa login https://memdusa.example.com --token mdk_…
memdusa status
memdusa install claude-code                 # the same install command as local mode
```

The token is saved in `~/.memdusa/config.json` with mode `0600`. It never goes into a committable file such as
`.mcp.json`. `MEMDUSA_URL` and `MEMDUSA_TOKEN` override the saved login, and `--database-url` forces local mode even
when you are logged in. A plain `http://` URL is refused for a non-loopback host; see
[Security](security.md#transport) for why, and [Deploying the server](deploy.md) for the server side.

You can also sign in with a browser or a device code instead of pasting a key. See
[OAuth sign-in](oauth.md#sign-in-from-the-cli).

### What remote mode changes

| Command | Local mode | Remote mode |
|---|---|---|
| `memdusa hook …` | runs the hook logic against the local database | one `POST /v1/hooks/{event}`; the server runs the same hook logic |
| `memdusa mcp` | an MCP server over stdio on the local database | a stdio-to-HTTP proxy to `/mcp` (works in every stdio-only harness) |
| `memdusa resume`, `remember`, `recall` | local database | the REST API |

The project key is always detected on the client from git and sent with each request. The server sets attribution
from the key: your agent is recorded as `alice/claude-code`, so two users' agents with the same name never share a
lease, and a client cannot claim to be someone else.

### When the server is unreachable

Nothing is lost. Prompts, tool use, compaction and session end are queued in `~/.memdusa/spool/` and replayed by the
next hook that reaches the server, in the order they happened and with their original timestamps. SessionStart
serves the last cached brief, marked as possibly out of date. `memdusa status` shows reachability and the queue
length. Rejected requests (a revoked key, bad input) are not queued, and delivery is at least once.

If a hook cannot reach the server it gives up quietly, in about 50 ms when the connection is refused. Hooks never
break a session in any mode. Before v0.1.9, `memdusa sync` flushed this queue by hand; it now runs project sync, and
the queue still flushes on its own with the next hook that reaches the server.

## Set up sync mode

Use sync mode when your agents should keep working against a local database, with no network in the hot path, and
you still want a shared copy on the team server. The local side is the database given by `--database-url` (default
`~/.memdusa/memdusa.db`); the remote side is your `memdusa login`. Sign in with an API key or with OAuth.

```sh
memdusa login https://memdusa.example.com --sync --token mdk_…
memdusa sync add github.com/acme/shop
memdusa sync                                    # push local changes, then pull remote ones
memdusa status                                  # reports both sides
```

Nothing leaves a project you have not opted into. The first hook in a new project opts it in automatically unless
`MEMDUSA_SYNC_AUTO_ADD=0`. A background watcher (`memdusa sync --watch`) keeps things current; see [Sync](sync.md)
for the full protocol, conflict rules and service setup.

Delegated work stays server-authoritative even in sync mode: `delegate`, `run_*`, `task_complete` / `task_fail` and
the SessionStart step claim go straight to the server. No local lease is created.

## Choose a mode

| Situation | Use |
|---|---|
| One developer, one machine, no sharing | local |
| Cloud agents, or a team that wants one up-to-date source of truth | remote |
| Laptop that must work offline, or hooks that must never touch the network | sync |
| Several developers sharing some projects but not others | remote or sync, plus [access control](access-control.md) |
| Large histories where every machine keeps its own full copy | sync |

All three modes use the same install command for a harness and the same hooks. Redaction, the offline queue and code
state work the same everywhere.

See also: [Deploying the server](deploy.md), [Users and API keys](users-and-keys.md),
[Access control](access-control.md), [Sync](sync.md), [Security model](security.md).
