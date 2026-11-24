# Operations

This page covers running a team server day to day: backups, exports, rate limits, metrics, logs, audit and upgrades.
For the access checks behind the audit log, see [Access control](access-control.md).

## Back up data

| Backend | How |
|---|---|
| SQLite | `memdusa backup <file>` makes a consistent online copy of the database (`VACUUM INTO`). The destination file must not exist. |
| Postgres | use `pg_dump` against the database. |

`memdusa backup` copies the local SQLite file; against a server it errors and tells you to use `memdusa export`
instead. Back up the whole `MEMDUSA_HOME` directory as well if you need the saved client config, spool and redaction
patterns. See [Retention and backups](../guides/retention.md) for what is pruned automatically.

### Export and import

```sh
memdusa export --only-project github.com/acme/shop -o shop.jsonl
memdusa export                                  # all projects, JSON Lines on stdout
memdusa import shop.jsonl                       # idempotent: existing records are skipped
```

An export contains memories, events, checkpoints and runs with their steps. It never contains users, keys, grants or
audit events. `memdusa import -` reads from stdin; it keeps ids and timestamps, skips records that already exist, so
it is safe to re-run, and re-applies redaction.

To move a local database to a hosted server, run this while logged in to the server. It needs an admin key, because
a full export and an import are admin operations:

```sh
memdusa --database-url sqlite://$HOME/.memdusa/memdusa.db export | memdusa import -
```

For ongoing replication instead of a one-time move, use [Sync](sync.md).

## Set rate and body limits

`memdusa serve --rate-limit <n>` (or `MEMDUSA_RATE_LIMIT`) sets the sustained requests per second per caller, with
bursts up to 10×. The default is `50`; `0` disables the limit. A caller is a user, or the client IP for anonymous
requests. Over the limit you get `429` with a `Retry-After` header, and hooks queue the event instead of losing it.

Request bodies are limited to 1 MB, with larger exceptions where they are needed: 16 MB for hook batches and sync
pushes, and 64 MB for imports.

## Scrape metrics

Prometheus metrics are served at `GET /metrics`. `--no-metrics` (or `MEMDUSA_NO_METRICS=1`) turns them off. They
cover requests and latency by route *template* (for example `/v1/runs/{id}`), hook events, rate-limit and auth
failures, users, and running and stale durable runs. Labels never contain ids, project names or users, so cardinality
stays bounded and metrics cannot leak data.

**The scrape needs a bearer credential once any user exists.** On a local server with no users, `/metrics` is open.
As soon as a user exists — or a shared `--token` is set — scrapes use the same auth as the API:

```sh
curl -H "authorization: Bearer $TOKEN" https://memdusa.example.com/metrics
```

`$TOKEN` is any valid API key or OAuth access token. Give your monitoring system a dedicated key; an OAuth token
scoped to `memdusa:read` is also sufficient. `/health` remains public for probes.

## Read logs

One access-log line is written per request, including rejected ones. It contains the method, route template, status,
latency and user, and never bodies or query strings. Logs go to stderr. `MEMDUSA_LOG_FORMAT=json` switches from text
to one JSON object per line; `RUST_LOG` sets tracing levels (the default is `memdusa=info,warn`). The audit log is a
separate, queryable record of security-relevant changes; see [Access control](access-control.md#audit-log).

## Review the audit log

The server records user, key, grant and identity changes, sign-ins and revocations, memory forgets, prune, import,
export, backfill, sync summaries, run finishes by a non-owner and every denied request. Details are redacted like
memory content.

```sh
memdusa audit --since 7d --limit 50
```

Events are kept for 365 days by default (`MEMDUSA_AUDIT_DAYS`; `0` keeps them forever). See
[Access control](access-control.md#audit-log) for filters and the API.

## Prune old data

`memdusa serve` prunes once a day unless `--no-auto-prune` is set. Local mode prunes at the end of a session, at most
once a day. To run it by hand:

```sh
memdusa prune --dry-run
memdusa prune --only-project github.com/acme/shop --activity-days 30
```

Retention defaults and the guide are in [Retention and backups](../guides/retention.md).

## Upgrade

For a binary install, use `memdusa self-update`. It fetches the latest release for your platform, verifies its
SHA-256 checksum and replaces the running binary. `memdusa self-update --check` only reports the available version;
`memdusa self-update --version v0.1.12` installs a specific one.

Homebrew and cargo installs are not replaced by `self-update`: use `brew upgrade memdusa`, or rerun
`cargo install --path crates/memdusa` on an updated checkout. For Docker, pull the new image and recreate the container; the database lives in the
`/data` volume. For Postgres, schema migrations run at startup and are serialized across replicas with an advisory
lock. Check the release notes before upgrading across a major version, and take a backup first.

See also: [Deploying the server](deploy.md), [Users and API keys](users-and-keys.md),
[Access control](access-control.md), [Security model](security.md), [REST API](../reference/rest-api.md).
