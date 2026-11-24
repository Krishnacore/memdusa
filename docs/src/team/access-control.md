# Access control

By default any authenticated user can read and write every project on a team server. Access control lets you narrow
that: each user gets grants for the projects they may see, and a server-wide default decides what happens when no
grant matches. Admins, the shared `--token` and an open server bypass grants entirely. Global memories (project
`NULL`) stay private to their author.

## Set the default policy

Set `MEMDUSA_DEFAULT_ACCESS` where `memdusa serve` runs and restart the server:

```sh
MEMDUSA_DEFAULT_ACCESS=none memdusa serve --addr 0.0.0.0:7077 --allowed-host memdusa.example.com
```

| Value | User without a matching grant |
|---|---|
| `write` (default, backward compatible) | full read and write access, as before |
| `read` | may read every project; writing needs a grant |
| `none` (recommended for multi-team servers) | nothing without a grant |

The server prints the effective default next to its auth mode at startup. `memdusa status` and
`GET /v1/whoami` show it too, along with your effective grants. Grants take effect on the next request: the server
loads one grants snapshot per request.

## Add and revoke grants

A grant matches an exact project key or a prefix ending in `/*`:

```sh
memdusa grant alice 'github.com/acme/*' --role write
memdusa grant alice 'github.com/acme/payroll' --role none   # carve out one project
memdusa grants --user alice
memdusa revoke-grant <id>                                   # id from `grants`
```

`--role` defaults to `write` and accepts `read`, `write` or `none`. `write` implies read, `read` is read-only and
`none` denies access. A `none` grant removes access even when the default would allow it. The matching rules are:

1. An exact grant beats any prefix.
2. Among prefixes, the longest matching prefix wins.
3. A `none` grant wins over a broader prefix or the default.

A `/*` prefix matches the prefix itself and everything below it on a `/` boundary. `github.com/acme/*` covers
`github.com/acme`, `github.com/acme/shop` and `github.com/acme/shop/sub`, but not the sibling
`github.com/acme-internal/payroll`. A pattern must be a project key or end in `/*`; `**` is rejected, and only the
trailing `/*` acts as a wildcard.

**Note:** grants only apply to users. The shared token, an open server and local/direct database access are root
and bypass them.

## Where access is checked

Every project-scoped REST endpoint and every remote MCP tool call uses the same check: memories, checkpoints, events,
resume, hooks and hooks batch, delegate, runs and steps and claims, review, and project export. A denial returns
`403` naming the project and the access level needed, and writes a `denied` event to the audit log.

| Operation | Access needed |
|---|---|
| Search, recall, resume, read events/checkpoints, list runs, export one project | read on that project |
| Save or forget memories, write checkpoints/events, hooks, delegate, runs and steps | write on that project |
| List runs across projects | filtered to projects you can read |
| Export without a project filter | admin only (a single-project export needs read) |

Run and step endpoints authorize against the project stored on the run, never one supplied by the caller. A user
cannot push sync records attributed to another user. A read-only OAuth token (`memdusa:read` scope) cannot write,
whatever the grants say.

### Connect a harness straight to the URL

You can register the server as a remote MCP endpoint without installing memdusa locally. The optional headers save
the agent from passing a project on every call:

```sh
claude mcp add --transport http memdusa https://memdusa.example.com/mcp \
  --header "Authorization: Bearer $TOKEN" \
  --header "X-Memdusa-Project: github.com/acme/shop" \
  --header "X-Memdusa-Agent: claude-code"
```

`$TOKEN` is an API key or an OAuth access token. Without `X-Memdusa-Project`, the MCP client must name the project
in each tool call.

## Audit log

The server records user, key and grant changes, memory forgets, prune, import, export, embedding backfill, runs
finished by someone other than their owner, sync push/pull summaries, sign-ins, registrations, revocations and every
denied request. Ordinary reads and hook writes are not logged separately: they are already attributed in the data.
Each event has an actor, action, project, target, redacted detail JSON and an `ok` or `denied` outcome.

```sh
memdusa audit                                                       # newest 100 events
memdusa audit --since 7d --actor alice --project github.com/acme/shop --limit 50
```

`--since` accepts unix milliseconds or a relative `24h`, `7d` or `2w`. The default limit is 100. The equivalent API
is `GET /v1/admin/audit?since=&actor=&project=&limit=` and is admin-only. Events are kept for 365 days by default
(`MEMDUSA_AUDIT_DAYS`; `0` keeps them forever) and pruned by the same job that handles data retention. Detail text
is redacted with the same rules as memory content, so secrets are not copied into the audit log.

The admin API mirrors the grant commands:

| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/admin/grants` | Create a grant from `{pattern, user, role}` |
| GET | `/v1/admin/grants[?user=]` | List grants |
| DELETE | `/v1/admin/grants/{id}` | Revoke a grant by id |

See also: [Users and API keys](users-and-keys.md), [OAuth sign-in](oauth.md),
[Operations](operations.md), [REST API](../reference/rest-api.md), [Security model](security.md).
