# REST API

`memdusa serve` exposes a JSON REST API under `/v1`, an MCP endpoint at `/mcp`, and Prometheus metrics at `/metrics`.
This page lists every route, taken from the server's routers. For full command options, see the
[CLI reference](cli.md); for environment variables, see [Configuration](configuration.md).

## Authentication

| Auth in the table | Meaning |
|---|---|
| public | No credential needed |
| bearer | Any valid credential (API key, OAuth access token, or the shared `--token`) |
| user | A credential tied to a user; the shared token and an open server are rejected |
| admin | `bearer`, and the caller must be an admin |
| project: read / write | `bearer`, plus the matching access on the project (admins, the shared token and an open server bypass grants) |
| metrics | public when no users exist; otherwise `bearer` |

Before the first user exists, and without `--token`, the server is open and every caller is root. Once any user
exists, unauthenticated requests get `401`. On a read-only OAuth token (`memdusa:read`), writes get `403`. OAuth
sessions and identity endpoints are only mounted when `MEMDUSA_PUBLIC_URL` is set. See
[Users and API keys](../team/users-and-keys.md) and [Access control](../team/access-control.md).

## Identity and credentials

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/v1/whoami` | bearer | Who am I: user, admin, grants, default access, server version and backend |
| GET | `/v1/me/keys` | user | List your API keys |
| POST | `/v1/me/keys` | user | Create a key (`{name}`), printed once |
| DELETE | `/v1/me/keys/{id}` | user | Revoke one of your keys (by id or display prefix) |
| GET | `/v1/admin/users` | admin | List users |
| POST | `/v1/admin/users` | admin | Create a user (`{name, admin?, key_name?}`); returns the user and their first key |
| POST | `/v1/admin/users/{name}/disable` | admin | Disable a user; all their keys stop working |
| POST | `/v1/admin/users/{name}/enable` | admin | Re-enable a user |
| POST | `/v1/admin/users/{name}/keys` | admin | Create a key for a user (`{name}`) |
| GET | `/v1/admin/keys?user=` | admin | List keys, optionally for one user |
| DELETE | `/v1/admin/keys/{id}` | admin | Revoke a key (by id or display prefix) |

## Grants, audit and server tasks

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/v1/admin/grants[?user=]` | admin | List project grants |
| POST | `/v1/admin/grants` | admin | Create a grant (`{pattern, user, role}`) |
| DELETE | `/v1/admin/grants/{id}` | admin | Revoke a grant by id |
| GET | `/v1/admin/audit?since=&actor=&project=&limit=` | admin | Query the audit log |
| POST | `/v1/admin/prune` | admin | Prune old data (`{policy?, project?, dry_run?}`) |
| POST | `/v1/embeddings/backfill` | admin | Embed memories missing a vector (`{all?, project?}`) using the server's embedding config |
| GET | `/v1/admin/export?project=` | admin, or read on that project | Export JSON Lines (memories, events, checkpoints, runs; never users or keys). A full export is admin-only |
| POST | `/v1/admin/import` | admin | Import JSON Lines from the request body; idempotent |

## Hooks, brief and memory

| Method | Path | Auth | Purpose |
|---|---|---|---|
| POST | `/v1/hooks/{event}` | project: write | Process one hook event (`{project, agent, journal_prompts, stop_guard_mins, input}`); the server runs the same hook logic as local mode |
| POST | `/v1/hooks/batch` | project: write on each item | Replay queued hooks (`{items: [{event, request}]}`); at most 1000 items, applied in order |
| GET | `/v1/resume?project=&git=&task=` | project: read | Build the resume brief (`git` is caller code state; `task` narrows it to one delegated task) |
| POST | `/v1/memories` | project: write | Save a memory (a memory with no project is a private global) |
| GET | `/v1/memories?query=&project=&kind=&limit=` | project: read | Search memories; without `project`, search all readable projects and visible globals |
| GET | `/v1/memories/review?project=` | project: read | List memories that need consolidating |
| DELETE | `/v1/memories/{id}` | project: write | Forget a memory (a global requires its owner or an admin) |
| PATCH | `/v1/memories/{id}` | project: write | Update `content`, `kind` or `tags` of a memory |
| GET | `/v1/memories/{id}/revisions` | project: read | Show superseded versions kept by sync conflicts |

## Events, checkpoints and runs

| Method | Path | Auth | Purpose |
|---|---|---|---|
| POST | `/v1/events` | project: write | Append an event |
| GET | `/v1/events?project=&session=&limit=` | project: read | Recent events |
| POST | `/v1/checkpoints` | project: write | Save a checkpoint |
| GET | `/v1/checkpoints?project=&limit=` | project: read | Latest checkpoints |
| POST | `/v1/runs` | project: write | Start a durable run |
| GET | `/v1/runs?project=&status=&limit=` | project: read (or filtered) | List runs; without `project`, only projects you can read |
| POST | `/v1/delegate` | project: write | Create a durable run with one step per task (`{project, name, tasks, idempotency_key?}`) |
| GET | `/v1/runs/{id}` | project: read | Get a run and its steps; the project is taken from the stored run |
| POST | `/v1/runs/{id}/claim` | project: write | Claim a run (`{owner, lease_secs?, force?}`) |
| POST | `/v1/runs/{id}/finish` | project: write | Finish a run (`{outcome, output?, error?}`) |
| POST | `/v1/runs/{id}/steps/{key}/begin` | project: write | Begin or replay a step (`{owner, input?}`) |
| POST | `/v1/runs/{id}/steps/{key}/claim` | project: write | Claim a delegated task (`{owner, lease_secs?, force?}`) |
| POST | `/v1/runs/{id}/steps/{key}/heartbeat` | project: write | Renew a delegated lease (`{owner, lease_secs?}`) |
| POST | `/v1/runs/{id}/steps/{key}/complete` | project: write | Complete a step (`{output?}`) |
| POST | `/v1/runs/{id}/steps/{key}/fail` | project: write | Fail a step (`{error}`) |

All run and step endpoints authorize against the project stored on the run, not a caller-supplied project. A finish
by someone other than the run's owner is audited as `run.finish_foreign`.

## Sync

| Method | Path | Auth | Purpose |
|---|---|---|---|
| POST | `/v1/sync/push` | project: write | Apply a batch of up to 500 local changes (`{project, changes: [...]}`); the reserved project `memdusa:globals` means your private globals |
| GET | `/v1/sync/pull?project=&after=&limit=` | project: read | Pull a page of changes after a cursor |
| GET | `/v1/sync/projects` | bearer | List projects you can read (used by `memdusa sync add --all`) |

Sync push and pull are audited as summaries with counts only, never record bodies. See [Sync](../team/sync.md).

## MCP, health and metrics

| Method | Path | Auth | Purpose |
|---|---|---|---|
| any MCP method | `/mcp` | bearer | Streamable HTTP MCP server. Optional `X-Memdusa-Project` and `X-Memdusa-Agent` headers set the default project and agent; unauthenticated requests carry a `WWW-Authenticate` challenge when OAuth is enabled |
| GET | `/health` | public | Liveness probe; returns `ok` |
| GET | `/metrics` | metrics | Prometheus metrics. Labels are route templates, never ids, projects or users |

`/health` and loopback hosts are exempt from the `--allowed-host` check. See
[Security model](../team/security.md#allowed-hosts-and-health-checks) and [Operations](../team/operations.md#scrape-metrics).

## OAuth discovery and clients

These routes exist only when `MEMDUSA_PUBLIC_URL` is set. They are rate limited by IP; `POST /oauth/register` and
`POST /oauth/device_authorization` have a tighter limit because a real client calls them once.

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/.well-known/oauth-protected-resource` | public | RFC 9728 protected-resource metadata |
| GET | `/.well-known/oauth-authorization-server` | public | RFC 8414 authorization-server metadata |
| POST | `/oauth/register` | public | RFC 7591 dynamic client registration for public clients (`{redirect_uris, client_name?, ...}`) |
| GET | `/oauth/authorize?client_id=&redirect_uri=&code_challenge=&state=&scope=&resource=` | public | Validate the authorization request and render the sign-in page; PKCE `S256` required |
| POST | `/oauth/signin` | public | Sign in with an API key (`{csrf, api_key}`) from the sign-in form |
| GET | `/oauth/signin/{provider}` | public | Start a GitHub or Google sign-in (`provider` is `github` or `google`) |
| GET | `/oauth/callback/{provider}` | public | Upstream OAuth/OIDC callback |
| POST | `/oauth/consent` | public | Approve or deny the client and scopes (`{csrf, decision}`, where `decision` is `allow` or `deny`) |
| POST | `/oauth/device_authorization` | public | RFC 8628: create a device code and user code |
| GET | `/oauth/device` | public | Device sign-in page (enter the user code) |
| POST | `/oauth/device` | public | Submit the user code (`{csrf, user_code}`); the following consent form approves or denies |
| POST | `/oauth/token` | public | Exchange an authorization code with its PKCE verifier, refresh a token, or redeem a device code |
| POST | `/oauth/revoke` | public | RFC 7009 token revocation |

## OAuth sessions and identities

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/v1/me/sessions` | user | List your active sign-in sessions |
| DELETE | `/v1/me/sessions/{id}` | user | Revoke one of your sessions and its grant chain |
| GET | `/v1/admin/sessions` | admin | List all sign-in sessions |
| DELETE | `/v1/admin/sessions/{id}` | admin | Revoke a session and its grant chain |
| GET | `/v1/admin/users/{name}/identities` | admin | List a user's linked external identities |
| POST | `/v1/admin/users/{name}/identities` | admin | Link an identity (`{provider, handle}`) |
| DELETE | `/v1/admin/users/{name}/identities/{provider}/{handle}` | admin | Unlink an identity |

See [OAuth sign-in](../team/oauth.md) for the flows, scopes and lifetimes, and
[Security model](../team/security.md#oauth-protections) for the protections around them.
