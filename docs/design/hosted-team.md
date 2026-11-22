# Design: memdusa as a team server

Status: H1 and H2 done; H3 in progress.

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

## H2: OAuth sign-in (MCP, hooks, CLI)

Status: in progress (H2a core first; H2b GitHub/Google and H2c CLI browser login build on it).

memdusa is an OAuth 2.1 **authorization server** and **resource server** for its own API and MCP endpoint, following
the MCP authorization spec, so a harness connecting to `https://memdusa.example.com/mcp` gets a browser sign-in
instead of a pasted API key. Users sign in with an existing memdusa API key, with GitHub, or with Google.

### Decisions
| Topic | Decision |
|---|---|
| Sign-in methods | memdusa API key (always), GitHub OAuth, Google OIDC; each upstream enabled by the admin with its client id/secret |
| Who may sign in via GitHub/Google | Only identities linked to a memdusa user. Linking: an admin runs `memdusa user link <user> github:<login>` / `google:<email>`, or the user links while signed in with an API key. Optional auto-provisioning by allowlist: `MEMDUSA_OAUTH_ALLOW_GOOGLE_DOMAINS=acme.com`, `MEMDUSA_OAUTH_ALLOW_GITHUB_ORGS=acme` creates a non-admin user on first sign-in (default: off). Google requires `email_verified`. |
| Access token | Opaque random token (`mdo_…`), stored hashed like API keys, **1 hour** |
| Refresh token | Opaque (`mdr_…`), hashed, **30 days sliding**, rotated on every use; reuse of an old refresh token revokes the whole grant chain (theft detection) |
| Absolute session limit | **365 days** from the last interactive sign-in; then sign in again |
| Scopes | `memdusa` (full user access, still limited by H1 grants); `memdusa:read` (read-only regardless of grants) |
| Clients | Dynamic client registration (RFC 7591) for public clients (harnesses), PKCE S256 required, exact redirect URI match; loopback redirects (`http://127.0.0.1:<any port>/…`) allowed per RFC 8252 |
| Hooks/CLI | `memdusa login <url> --browser` (authorization code + PKCE with a loopback redirect) and `--device` (device authorization grant, RFC 8628) for SSH/headless; stores the refresh token in `~/.memdusa`, refreshes transparently |
| Revocation | `memdusa sessions` / `memdusa sessions revoke <id>` (own), admin API for everyone; token revocation endpoint (RFC 7009); all audited (H1 audit log) |
| Existing API keys | Keep working unchanged as bearer tokens |

### Endpoints
- `/.well-known/oauth-protected-resource` (RFC 9728): resource = `<base>/mcp` (and `<base>/v1`), authorization_servers =
  [`<base>`]. `401` responses from `/mcp` and `/v1` carry `WWW-Authenticate: Bearer resource_metadata="…"`.
- `/.well-known/oauth-authorization-server` (RFC 8414): issuer, endpoints, `code_challenge_methods_supported: [S256]`,
  `grant_types_supported: [authorization_code, refresh_token, urn:ietf:params:oauth:grant-type:device_code]`,
  `token_endpoint_auth_methods_supported: [none]`, scopes.
- `POST /oauth/register` (DCR), `GET /oauth/authorize` → sign-in page (API key form + "Continue with GitHub/Google"
  buttons when configured) → consent screen (client name, redirect host, scopes) → redirect with `code` + `state`.
- `GET /oauth/callback/{github,google}` (upstream returns here; memdusa itself uses PKCE + state with the upstream).
- `POST /oauth/token` (code + PKCE verifier; refresh_token; device_code), `POST /oauth/revoke`,
  `POST /oauth/device_authorization` + `GET /oauth/device` (user enters the code).
- The `resource` parameter (RFC 8707) is accepted and bound into the token audience.

### Storage (migration v8, portable)
`oauth_clients`, `oauth_codes` (hashed, 10 min, single use), `oauth_grants` (a sign-in: user, client, scopes, method,
created/last_interactive_at, revoked_at), `oauth_tokens` (hashed access/refresh, grant id, kind, expires_at,
rotated_from, revoked_at), `user_identities` (user, provider, subject, email/login, linked_at),
`oauth_device_codes`.

### Security notes
- All secrets random 256-bit, stored as SHA-256 hashes; constant-time comparison.
- CSRF: `state` on every redirect; sign-in/consent forms carry a per-request token; `SameSite=Lax` cookie only for the
  short sign-in transaction (no long-lived browser session needed).
- The sign-in page never reveals whether a user exists; rate-limit failed API-key sign-ins per IP.
- Upstream tokens (GitHub/Google) are used once to read identity and are not stored.
- Requires `MEMDUSA_PUBLIC_URL` (the external https base URL) when OAuth is enabled; refuse non-https issuers except
  localhost.

### Phases
- **H2a**: metadata, DCR, authorize with API-key sign-in + consent, token endpoint (code+PKCE, refresh rotation, reuse
  detection, absolute limit), revocation, bearer validation for `/mcp` and `/v1`, sessions CLI/API, audit.
- **H2b**: GitHub + Google upstream sign-in, identity linking, optional allowlist auto-provisioning.
- **H2c**: `memdusa login --browser` / `--device`, transparent refresh in the client.

## H3: continuous local ⇄ hosted sync

Status: in progress (H3a protocol + one-shot sync; H3b background sync mode).

Goal: agents write to a fast local SQLite database (works offline, no network in hooks); memdusa replicates the
chosen projects to and from the team server.

### Decisions
| Topic | Decision |
|---|---|
| What syncs | Projects the user opts into: `memdusa sync add <project>` / `memdusa sync add --all` (every project the user can read on the server). Global memories of the user: opt-in with `--globals`. |
| Tables | memories (+ deletions), events, checkpoints. **Not** synced: runs/steps (server-authoritative: in sync mode, delegate/claim/step calls go straight to the server), embeddings (each side embeds its own copy), users/keys/grants/audit/OAuth. |
| Change tracking | A `changes` log on both sides: `(seq INTEGER/BIGSERIAL, kind, record_id, project, op, at)`, appended in the same transaction as each write (op = `upsert` / `delete`). Server `seq` is the global pull cursor; local `seq` is the push cursor. Timestamps are never used as cursors (clock skew). |
| Push | `POST /v1/sync/push {project, changes:[{kind, op, record}]}` in batches; server applies idempotently (ids are UUIDv7, globally unique): events/checkpoints insert-if-absent; memories last-writer-wins by `updated_at` (tie → larger id); a losing version is kept in `memory_revisions`; deletes become tombstones. Requires write access (H1). Response: per-change result + the server `seq` of each applied change. |
| Pull | `GET /v1/sync/pull?project=&after=<seq>&limit=` returns changes after the cursor (records inline, tombstones for deletes), filtered by H1 read access; local applies with the same rules. Changes the client pushed itself come back with an `origin` marker and are skipped. |
| Attribution | Records keep their original `source`/`user` (server checks the pusher may write them: a user can't push records attributed to another user; agent names are re-qualified as `user/agent` on the server, like hooks are). |
| Redaction | Applied by each store on write (the server redacts pushed content again). |
| Conflicts surfaced | `memdusa sync status` lists recent LWW losers per project; `memdusa memories revisions <id>` shows them. |
| Mode | `memdusa sync` (one-shot push+pull), `memdusa sync --watch [--interval 15s]` (loop; H3b), `memdusa sync status`. In sync mode (`memdusa login <url> --sync`), hooks/MCP/CLI use the local DB; a background sync runs from `memdusa sync --watch` (launchd/systemd snippets in README) and opportunistically after SessionEnd. |
| Offline spool | Superseded in sync mode (local DB is the queue); kept for plain remote mode. |

### Migration
v11 (portable): `changes`, `memory_revisions`, `sync_state(remote, project, push_after, pull_after, last_ok_at,
last_error)`. Deletes stay hard deletes; the tombstone lives only in `changes` (so it can be pushed/pulled). Existing
rows get no backfilled changes: the first `memdusa sync add` does an initial full push/pull of that project.
