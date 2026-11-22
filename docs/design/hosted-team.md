# Design: memdusa as a team server

Status: H1 in progress; H2 and H3 proposed.

A memdusa server (`memdusa serve`, Postgres or SQLite) already has per-user API keys, server-side attribution
(`user/agent`) and private global memories. What a team needs next, in dependency order:

| | What | Why first/later |
|---|---|---|
| **H1** | Per-project access control + audit log | Everything else (OAuth tokens, sync) needs "who may read/write which project" |
| **H2** | OAuth for remote MCP | Harnesses connect without pasting an API key into config files |
| **H3** | Continuous local ⇄ hosted sync | Offline-first work against a local DB, merged into the team server |

## H1: per-project access control and audit log

### Today
Any authenticated user can read and write every project. Admins (and the shared `--token`, and an open server) can do
everything, including `/v1/admin/*`.

### Model
`project_grants`: `(id, pattern, user_name, role, created_by, created_at)`
- `pattern`: an exact project key (`github.com/acme/billing`) or a prefix ending in `/*` (`github.com/acme/*`). Exact
  beats prefix; the longest prefix wins among prefixes.
- `role`: `read` (search, recall, resume, export of that project) or `write` (read + save memories, checkpoints,
  events, hooks, runs/steps, delegate, forget).
- Admins bypass grants. The shared `--token` and an open server are root (unchanged).

Default policy, server setting `MEMDUSA_DEFAULT_ACCESS`:
- `write` (default, backward compatible): users without any matching grant keep today's behaviour.
- `read`: everyone may read every project; writing needs a grant.
- `none`: nothing without a grant (recommended for multi-team servers).
A grant can also be `role = none` to carve out a project from a broader rule (`github.com/acme/* write` +
`github.com/acme/payroll none`).

Global memories (project NULL) keep their current rule: private to their owner (admins see all).

### Enforcement
One function, `authorize(&Identity, project, Access) -> Result<(), Forbidden>`, called by:
- every REST endpoint that takes or implies a project (memories, checkpoints, events, runs/steps/claims, resume,
  hooks + hooks batch, delegate, review, export with `only_project`),
- the remote MCP server for each tool call (project comes from the call/default),
- list endpoints (`run_list`, search across projects, export without project) filter to readable projects.
Runs and steps: authorize against the run's project, not a caller-supplied one.
Denials return 403 with the project and the needed access; they are audited.

Grants are cached per request (one query per request at most); a grant change takes effect on the next request.

### Admin API and CLI
- `POST /v1/admin/grants {pattern, user, role}`, `GET /v1/admin/grants[?user=]`, `DELETE /v1/admin/grants/{id}`
- `memdusa grant <user> <pattern> --role read|write|none`, `memdusa grants [--user U]`, `memdusa revoke-grant <id>`
  (local mode writes the table directly; remote mode calls the API).
- `GET /v1/whoami` lists the caller's effective grants and the default policy.

### Audit log
`audit_events`: `(id, at, actor, action, project, target, detail JSON, outcome)` where `outcome` is `ok` or `denied`.
Recorded: grant create/delete, user create/disable/enable, key create/revoke, memory forget, prune, import, export,
backfill, run finish/cancel by someone other than the owner, and every denied request. Not recorded: ordinary reads and
hook writes (too noisy; they're already attributed in the data).
- `GET /v1/admin/audit?since=&actor=&project=&limit=` and `memdusa audit [...]`.
- Retention: kept 365 days by default (`MEMDUSA_AUDIT_DAYS`), pruned by the existing retention job.
Redaction applies to `detail` like everything else.

### Migration
v7 (portable): `project_grants`, `audit_events` + indexes. No data changes; default policy `write` = no behaviour
change for existing servers.

## H2: OAuth for remote MCP (proposed)

Follow the MCP authorization spec: the memdusa server is an OAuth 2.1 resource server and its own authorization server.
- Protected resource metadata (RFC 9728) at `/.well-known/oauth-protected-resource`; authorization server metadata
  (RFC 8414) at `/.well-known/oauth-authorization-server`; dynamic client registration (RFC 7591) so harnesses register
  themselves; authorization code + PKCE.
- Login page: the user signs in with an existing memdusa API key once (or, later, an external IdP such as GitHub OIDC)
  and approves the client; memdusa issues short-lived access tokens + refresh tokens tied to the user, scoped to
  `memdusa:read` / `memdusa:write`. H1 grants still decide project access.
- Tokens stored hashed (like API keys), revocable from `memdusa keys` / admin API; audited.
Open questions: external IdP support first or later; token lifetimes; whether hooks (not MCP) should also use OAuth
tokens (probably via `memdusa login --browser`).

## H3: continuous local ⇄ hosted sync (proposed)

Goal: agents always write to a fast local DB (works offline, no hook latency); a background `memdusa sync --watch`
replicates to and from the team server.
- IDs are already UUIDv7 (time-ordered, globally unique); every table gets `updated_at` (most have it) and a
  `sync_state` table stores per-remote cursors.
- Push: local rows changed since the push cursor, via a batch endpoint; pull: server rows changed since the pull cursor,
  filtered by H1 grants.
- Conflict rules: events and checkpoints are append-only (no conflicts); memories last-writer-wins by `updated_at`
  with the loser kept as a superseded revision; runs/steps are server-authoritative (claims/leases must be decided in
  one place, so delegated work goes through the server, never the local copy).
- Deletes propagate as tombstones.
- The existing offline spool (`~/.memdusa/spool`) becomes a special case of push.
Open questions: which projects sync (all readable vs opt-in list); embedding vectors (re-embed locally vs replicate).
