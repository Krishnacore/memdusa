# Security model

This page explains memdusa's security in plain terms: what is stored, what travels over the network, who can see what,
and what an admin should do. It describes the current behavior of `memdusa serve` and the clients that talk to it.

## What is stored

| Data | How it is stored |
|---|---|
| API keys | A key is `mdk_` plus 256 bits of randomness. The server stores only its SHA-256 hash and a 12-character display prefix, so it can list and revoke keys but cannot print the original. A new key is shown once. |
| OAuth tokens and codes | Access tokens (`mdo_…`), refresh tokens (`mdr_…`), authorization codes and device codes are random 256-bit secrets stored as SHA-256 hashes. They are never stored in cleartext. |
| Client config | `~/.memdusa/config.json` holds the saved login and any OAuth tokens. It is written atomically with mode `0600`, so only your user can read it. Tokens are never printed by `memdusa login`. |
| Memory content | Stored as text, but redacted before it is written (see below). Audit detail text is redacted with the same rules. |
| Upstream identities | GitHub and Google access tokens are used once to read your identity and are never stored. Only the provider, the account's stable id, and its login and email are kept. |

On a team server, the server sets attribution from your credential: your records are attributed to `alice/claude-code`,
not to a name your client chose. Global memories (project `NULL`) are private to their author, even from other users
on the same server.

## Redaction before data leaves the machine

Hooks capture raw prompts and shell commands, and agents paste things into memories, so secrets will appear in the
input. Memdusa redacts them with patterns for provider tokens, private keys, `user:password@` URLs, `Bearer` values,
assignments such as `*_TOKEN=…` and CLI flags such as `--password`. A secret becomes `[REDACTED:<kind>]` and the
surrounding text is kept:

```text
PGPASSWORD=[REDACTED:assignment] psql postgres://app:[REDACTED:url_password]@db/prod
```

Redaction happens in the server's store layer, which every write goes through, and again on the client before a
remote hook or a transcript import sends its payload. That second pass means a secret never leaves the machine in a
hook event, even if the server is compromised or misconfigured. Step outputs are redacted too, so a replayed step
returns redacted output. Do not pass secrets through durable runs.

Variable references (`TOKEN=$GH_TOKEN`), numbers, paths and ordinary words ("rotate tokens monthly") are not redacted
on purpose. You can add your own patterns in `MEMDUSA_REDACT_PATTERNS` or `~/.memdusa/redact-patterns.txt`.
`MEMDUSA_REDACT=off` disables redaction and is not recommended. See
[Secret redaction](../guides/redaction.md).

## Transport

**A remote `http://` URL is refused.** A bearer token sent over plain HTTP can be read by anyone on the path. Client
logins and REST/MCP clients require `https://` unless the host is loopback (`localhost`, `127.*`, `[::1]`) — that is,
the local development server. This check applies to a saved login too, so a config written by an older version is
rechecked at use time.

If you run a server on a trusted private network and accept the risk, set `MEMDUSA_ALLOW_INSECURE_HTTP=1` on the
client to allow a non-loopback `http://` URL. Do not use it on an untrusted network. `MEMDUSA_PUBLIC_URL` on the
server has the same rule: it must be an absolute `https://` URL, with `http://` allowed only for loopback development.

Memdusa does not terminate TLS. Put a TLS-terminating reverse proxy in front of the server and set
`MEMDUSA_PUBLIC_URL` to the external HTTPS base URL. See [Deploying the server](deploy.md#put-tls-in-front).

## Access checks

On an open server with no users, any caller is root. As soon as one user exists, every API and MCP request needs a
credential. Authentication also turns on if a shared `--token` is set.

For a normal user, access is checked per project. The default `MEMDUSA_DEFAULT_ACCESS=write` means every user can
read and write every project; `read` and `none` tighten that. Grants can give or remove access to an exact project
or a `/*` prefix. Denials return `403` and are written to the audit log. Admins, the shared token and an open server
bypass grants. Run and step endpoints check the project stored on the run, so a caller cannot redirect the check by
supplying a different project. OAuth tokens scoped `memdusa:read` cannot write, whatever the grants say.

See [Access control](access-control.md) for the grant rules and [Users and API keys](users-and-keys.md) for
credentials.

## OAuth protections

The OAuth implementation follows OAuth 2.1 and the MCP authorization spec:

- PKCE with `S256` is required; plain PKCE and implicit flows are not supported. Redirect URIs must be registered
  exactly, may be `https://` or loopback `http://`, and are limited in number and length.
- Every authorization carries `state` and a short-lived sign-in transaction cookie, with CSRF checks on the forms.
  The `resource` parameter (RFC 8707) is bound into the token. Authorization codes are one-time and expire after
  10 minutes. Refresh tokens rotate on every use, and reusing an old one revokes the whole grant chain and is
  audited as theft. Sessions have a 365-day absolute limit and can be revoked at any time.
- Sign-in and consent pages set anti-framing and no-store headers (`frame-ancestors 'none'`, `X-Frame-Options: DENY`,
  `Referrer-Policy: no-referrer`, `Cache-Control: no-store`). Failed API-key sign-ins are limited per IP (more than
  10 failures in five minutes).
- GitHub and Google sign-in use `state`, PKCE `S256` and, for Google, `nonce`. Google ID tokens are checked for
  issuer, audience, expiry, nonce and RS256 signature against a cached JWKS, and require `email_verified = true`.
  GitHub pending links bind by login only, because GitHub's `/user` email is not verified; Google pending links bind
  by verified email. Unknown identities are not provisioned unless an admin configures an allowlist.
- All OAuth routes are rate limited by IP. Client registration and device authorization are limited more tightly,
  because a real client calls them once. See [OAuth sign-in](oauth.md).

## Rate limits

The API is limited per user, or per IP for anonymous callers: `--rate-limit 50` requests per second with bursts up
to 10×, and `0` turns it off. Over the limit, requests get `429` with `Retry-After`, and hooks queue the event rather
than dropping it. OAuth has its own per-IP limit, with a tighter one for the endpoints that create rows. Request
bodies are capped at 1 MB, with larger limits for hook batches, sync pushes and imports. See
[Operations](operations.md#set-rate-and-body-limits).

## Allowed hosts and health checks

`--allowed-host` is a DNS-rebinding defence for the whole router, not just MCP. With it set, a request must name one
of the listed `Host` values or it gets `403`. Two exceptions pass: `/health`, so load balancers and orchestrators can
probe by pod address, and loopback hosts, so the admin's CLI on the server still works. With no flag, the check is
off. See [Deploying the server](deploy.md#host-checks).

## Deploy safely

Before exposing a team server:

1. Terminate TLS at a reverse proxy and set `MEMDUSA_PUBLIC_URL` to the external HTTPS URL.
2. Set `--allowed-host` (or `MEMDUSA_ALLOWED_HOSTS`) to the public host, and listen on `0.0.0.0` only behind the proxy.
3. Create an admin user with `memdusa user add alice --admin` and store the printed key securely. Do not leave the
   server open or rely on the shared `--token` for daily use.
4. For a multi-team server, set `MEMDUSA_DEFAULT_ACCESS=none` and give users explicit grants. Prefer read-only tokens
   where write access is not needed.
5. Keep rate limiting on, restrict who can reach `/metrics` (it needs a bearer token once users exist), and monitor
   `memdusa audit` and rate-limit/auth-failure metrics.
6. Keep redaction on, back up the database, and upgrade with `memdusa self-update` or a new container image.
7. Never set the test-suite endpoint overrides used for GitHub/Google sign-in on a real server.

## Report a vulnerability

Please report security issues privately, not in a public issue. Use GitHub's private vulnerability reporting for this
repository: [Report a vulnerability](https://github.com/Krishnacore/memdusa/security/advisories/new).
Include the affected version, how to reproduce the issue and its impact. You should get an acknowledgement; please
give us time to fix it before disclosing it publicly.

See also: [Deploying the server](deploy.md), [Operations](operations.md),
[OAuth sign-in](oauth.md), [Access control](access-control.md).
