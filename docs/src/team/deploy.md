# Deploying the server

A team server is one `memdusa serve` process reachable over HTTPS. The server itself speaks plain HTTP on
`127.0.0.1:7077` by default; you put a TLS-terminating reverse proxy in front of it. This page covers the binary and
container options, the database, the first admin user and how machines connect.

## Run with the binary

Install the binary on the host (see [Installation](../getting-started/installation.md); from source it is
`cargo install --path crates/memdusa`). Then create the first admin and start the server:

```sh
export MEMDUSA_DATABASE_URL=postgres://user:pass@db/memdusa
memdusa user add alice --admin             # prints alice's first API key once
memdusa serve --addr 0.0.0.0:7077 --allowed-host memdusa.example.com
```

`--addr` defaults to `127.0.0.1:7077` and is also set by `MEMDUSA_ADDR`. Use `0.0.0.0` only behind a proxy or on a
private network. Every server flag has a matching environment variable; see the
[CLI reference](../reference/cli.md) and [Configuration](../reference/configuration.md).

At startup the server logs its auth mode, the effective default project access and, when OAuth is enabled, the exact
callback URLs to register. `GET /health` returns `ok` and is public.

### Host checks

`--allowed-host` (or `MEMDUSA_ALLOWED_HOSTS`, comma-separated) is your defence against DNS rebinding. With it set, a
request whose `Host` header is not one of the listed hosts is refused with `403`. The check covers the whole server:
REST, MCP and OAuth. Two exceptions always pass: `/health`, so load balancers and orchestrators can probe by pod
address, and loopback hosts, so an admin's CLI on the server still works.

Entries may be a host or a `host:port`, and a host-only entry matches any port. If you omit the flag, the check is
disabled. For the external URL your clients use, put that host (or `host:port`) in the list:

```sh
memdusa serve --addr 0.0.0.0:7077 --allowed-host memdusa.example.com
```

## Run with Docker

The published image is `ghcr.io/krishnacore/memdusa`. Its entrypoint is `memdusa` and its default command is `serve`.
It sets `MEMDUSA_HOME=/data`, `MEMDUSA_DATABASE_URL=sqlite:///data/memdusa.db` and `MEMDUSA_ADDR=0.0.0.0:7077`, and
runs as an unprivileged user. Mount a volume at `/data` so the database survives a restart.

```sh
docker run -d --name memdusa \
  -p 7077:7077 \
  -v memdusa:/data \
  -e MEMDUSA_ALLOWED_HOSTS=memdusa.example.com \
  ghcr.io/krishnacore/memdusa
docker exec memdusa memdusa user add alice --admin
```

The second command prints the first API key once. For Postgres, set `MEMDUSA_DATABASE_URL=postgres://…` on the
container instead and keep `/data` for memdusa's own state.

## Choose a database

| | When to use | Notes |
|---|---|---|
| SQLite | one server, small to medium team, simple backups | Default (`~/.memdusa/memdusa.db`, or `/data/memdusa.db` in the image). Consistent online copies with `memdusa backup`. |
| Postgres | several replicas, or you already run Postgres | Set `MEMDUSA_DATABASE_URL=postgres://user:pass@db/memdusa`. Several replicas can share one database; schema migrations are serialized with an advisory lock. Back up with `pg_dump`. |

Both backends run the same migrations at startup. Point `MEMDUSA_DATABASE_URL` at a dedicated database; memdusa
creates its tables there.

## Put TLS in front

Memdusa does not terminate TLS. Terminate HTTPS at a reverse proxy (nginx, Caddy, Traefik, a cloud load balancer)
and forward to the server's `--addr`. Forward the original `Host` header so the allowed-host check sees the public
name. The proxy is also the right place to set security headers and to limit who can reach `/metrics`.

Tell memdusa the public URL so it can build OAuth discovery documents, redirect URLs and `WWW-Authenticate`
challenges:

```sh
export MEMDUSA_PUBLIC_URL=https://memdusa.example.com
memdusa serve --addr 127.0.0.1:7077 --allowed-host memdusa.example.com
```

`MEMDUSA_PUBLIC_URL` must be an absolute `https://` URL. `http://` is allowed only for `localhost`, `127.0.0.1` and
`[::1]` in development. It must not contain credentials, a query or a fragment. Setting it turns on the OAuth
endpoints; without it, clients can still sign in with API keys. See [OAuth sign-in](oauth.md) and
[Security](security.md#transport).

## Create users and connect machines

Create one admin on the server, then sign in each machine with that user's key:

```sh
memdusa user add alice --admin
```

```sh
memdusa login https://memdusa.example.com --token mdk_…
memdusa status
memdusa install claude-code                 # the same command as in local mode
```

`memdusa login` checks the key with `/v1/whoami` and saves it in `~/.memdusa/config.json` with mode `0600`. The
project key is still detected from git on each machine. For browser or device sign-in instead of a key, see
[OAuth sign-in](oauth.md#sign-in-from-the-cli). For sync mode, use `memdusa login <url> --sync`; see [Sync](sync.md).

**Note:** before the first user exists, a server with no `--token` is open and logs a warning. That is fine for
localhost development only. Authentication turns on as soon as any user exists, even one created by the CLI while
the server is running. Create the admin before exposing the server.

See also: [Users and API keys](users-and-keys.md), [Access control](access-control.md),
[Operations](operations.md), [Security model](security.md).
