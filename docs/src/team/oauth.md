# OAuth sign-in

With OAuth 2.1, an MCP client can sign in through a browser instead of having an API key pasted into its config.
Memdusa is both the authorization server and the resource server for its own API and `/mcp` endpoint. API keys and
the shared `--token` keep working unchanged.

OAuth is off until you set `MEMDUSA_PUBLIC_URL` on the server. If you only use API keys, you do not need this page.

## Enable OAuth

```sh
export MEMDUSA_PUBLIC_URL=https://memdusa.example.com
memdusa serve --addr 0.0.0.0:7077 --allowed-host memdusa.example.com
```

`MEMDUSA_PUBLIC_URL` is the external base URL, and it must be `https://`. For local development, `localhost`,
`127.0.0.1` and `[::1]` over `http://` are allowed. It must not contain credentials, a query or a fragment. With the
URL set, `memdusa serve` logs the callback URLs to register when you enable GitHub or Google sign-in below.

Never point a production server at the endpoint overrides used by the test suite. The server logs a warning if it
detects them.

## What an MCP client sees

- `GET /.well-known/oauth-protected-resource` (RFC 9728) and `GET /.well-known/oauth-authorization-server` (RFC 8414)
  describe the server. An unauthenticated `/mcp` or `/v1` request returns `401` with
  `WWW-Authenticate: Bearer resource_metadata="…", scope="memdusa"`.
- `POST /oauth/register` (RFC 7591) registers a public client from its `redirect_uris` and optional `client_name`, and
  returns a `client_id`. Only `token_endpoint_auth_method: none` is supported. Redirect URIs must be `https://`, or
  loopback `http://127.0.0.1:<port>/…` / `http://localhost:<port>/…` (RFC 8252). At most 10 redirects are accepted.
- `GET /oauth/authorize` checks `response_type=code`, the registered `redirect_uri`, `state`, the scope and the
  optional `resource` (RFC 8707). It requires PKCE with `code_challenge_method=S256`; plain PKCE is rejected. It then
  renders a small sign-in page: paste an existing memdusa API key, then approve the client and scopes on the consent
  screen. If you configured GitHub or Google, their buttons appear here too.
- `POST /oauth/token` exchanges the one-time authorization code (10-minute expiry) together with the PKCE verifier.
  It also handles refresh tokens and the device-code grant. Refresh tokens rotate on every use; reusing an old one
  revokes the whole grant chain and is audited as theft. `POST /oauth/revoke` implements RFC 7009.
- `POST /oauth/device_authorization`, `GET/POST /oauth/device` and the
  `urn:ietf:params:oauth:grant-type:device_code` grant implement the device flow (RFC 8628) and are advertised in
  the authorization-server metadata.

Scopes are `memdusa` (full user access, still limited by your project grants) and `memdusa:read` (read-only: a write
gets `403`). The default scope is `memdusa`. The sign-in page shows the scope the token will receive.

### Tokens and sessions

| Credential | Prefix | Lifetime |
|---|---|---|
| Access token | `mdo_…` | 1 hour |
| Refresh token | `mdr_…` | 30 days, sliding (each use renews it) |
| Session | — | 365 days absolute from the last interactive sign-in |
| Authorization code | — | 10 minutes |
| Device code | — | 10 minutes, 5-second polling interval |

All tokens, codes and keys are stored as SHA-256 hashes, never in cleartext. API keys and OAuth tokens are accepted
by the same endpoints. An OAuth token carries the scope chosen at sign-in, so a `memdusa:read` token cannot write
even if your grants allow it.

You can shorten the lifetimes with `MEMDUSA_OAUTH_ACCESS_SECS`, `MEMDUSA_OAUTH_REFRESH_SECS`,
`MEMDUSA_OAUTH_ABSOLUTE_SECS`, `MEMDUSA_OAUTH_CODE_SECS`, `MEMDUSA_OAUTH_SIGNIN_SECS`,
`MEMDUSA_OAUTH_DEVICE_SECS` and `MEMDUSA_OAUTH_DEVICE_INTERVAL_SECS`. They are described in
[Configuration](../reference/configuration.md).

Sign-ins, registrations, refresh reuse, revocations and denials go to the audit log (`memdusa audit`). Failed
API-key sign-ins are limited per IP: after more than 10 failures in five minutes, that IP is blocked. Every `/oauth/*` route is
rate limited as well, and the unauthenticated routes that create rows (`/oauth/register` and
`/oauth/device_authorization`) have a tighter limit, because a real client calls them once. Sign-in pages are served
with anti-framing and no-store headers. See [Security](security.md#oauth-protections).

### Inspect and revoke sessions

```sh
memdusa sessions list                       # your sign-in sessions
memdusa sessions list --user alice          # admin: anyone's sessions
memdusa sessions revoke <id>                # revokes that session's whole grant chain
```

The same data is available through `GET /v1/me/sessions`, `DELETE /v1/me/sessions/{id}`,
`GET /v1/admin/sessions` and `DELETE /v1/admin/sessions/{id}`.

## Sign in from the CLI

`memdusa login` supports three credential types: `--token` (an API key), `--browser` and `--device`. Browser and
device sign-in register a public client with the server and save the resulting tokens locally, refreshing them
transparently. You must have `MEMDUSA_PUBLIC_URL` set on the server for these flows.

### Browser flow

On a machine with a browser:

```sh
memdusa login https://memdusa.example.com --browser
```

Memdusa discovers the authorization server metadata, registers a public client (the `client_id` is cached per server
in `~/.memdusa/oauth-clients.json`), starts a loopback listener on `127.0.0.1` with a random port, prints the sign-in
URL and tries to open it with `open`, `xdg-open` or `start`. Sign in with an API key or an upstream provider, then
approve the scopes. The authorization code returns to the loopback listener, is exchanged with PKCE S256 and `state`,
and the access and refresh tokens are saved in `~/.memdusa/config.json` (mode `0600`). Tokens are never printed.

If no browser opens, use the printed URL. If you do not finish in five minutes, the command exits with a clear
timeout message; use `--device` instead if you need longer.

### Device flow

On an SSH or headless machine:

```sh
memdusa login https://memdusa.example.com --device
```

```text
To sign in, visit:
  https://memdusa.example.com/oauth/device?user_code=ABCD-EFGH
and enter the code: ABCD-EFGH
```

The CLI prints the verification URL and a short user code, then polls the token endpoint every five seconds,
handling `authorization_pending`, `slow_down`, `expired_token` and `access_denied`. The code expires after ten
minutes. In the browser you see the same sign-in and consent pages; approvals, denials, token issuance and audit
events are recorded.

### After sign-in

```sh
memdusa status                  # sign-in method, user, expiry, scopes, reachability, queue
memdusa logout                  # revoke the refresh token at the server and delete local tokens
```

The REST client and the `memdusa mcp` proxy refresh access tokens before they expire (or once on a `401`). Rotation
is saved atomically and a lock file (`~/.memdusa/oauth-refresh.lock`) stops concurrent hook processes from racing.
If refresh fails — for example, an admin revoked the session — hooks keep the event in the offline spool and exit
successfully, and `memdusa status` tells you to run `memdusa login` again.

## Sign in with GitHub or Google

Set a provider's client id **and** secret on the server to enable it. With `MEMDUSA_PUBLIC_URL` set,
`memdusa serve` prints the exact callback URL to register at startup:

```sh
export MEMDUSA_PUBLIC_URL=https://memdusa.example.com
export MEMDUSA_OAUTH_GITHUB_CLIENT_ID=Iv1.…
export MEMDUSA_OAUTH_GITHUB_CLIENT_SECRET=…
export MEMDUSA_OAUTH_GOOGLE_CLIENT_ID=…apps.googleusercontent.com
export MEMDUSA_OAUTH_GOOGLE_CLIENT_SECRET=…
memdusa serve --addr 0.0.0.0:7077 --allowed-host memdusa.example.com
```

```text
register this github OAuth callback URL: https://memdusa.example.com/oauth/callback/github
register this google OAuth callback URL: https://memdusa.example.com/oauth/callback/google
```

- **GitHub:** create an OAuth App (Settings → Developer settings → OAuth Apps) and set its Authorization callback
  URL to the printed `/oauth/callback/github` URL. Memdusa requests `read:user`, plus `read:org` when an org
  allowlist is configured, sends `state` and PKCE `S256`, exchanges the code at GitHub, and reads `GET /user`. The
  subject is the stable numeric user id; the display name is the GitHub login.
- **Google:** create an OAuth client of type *Web application* in the Google Cloud Console and add the printed
  `/oauth/callback/google` URL as an authorized redirect URI. Memdusa uses OpenID Connect (scopes `openid email`,
  `state`, PKCE `S256` and `nonce`), verifies the ID token's issuer, audience, expiry, nonce and RS256 signature
  against Google's cached JWKS, and requires `email_verified = true`. The subject is the `sub` claim.

The sign-in page shows a **Continue with GitHub** / **Continue with Google** button for each configured provider.
The pending memdusa authorization transaction is carried through the upstream round trip with a short-lived cookie
and `state`; you land back on the same consent screen. Upstream access tokens are used once to read the identity and
are never stored.

## Who may sign in

By default, only an identity linked to a memdusa user may sign in. An unknown identity gets a "not linked" page and
is audited. Link an identity as an admin:

```sh
memdusa user link alice github:octocat          # binds on first GitHub sign-in by login
memdusa user link alice github:424242           # binds directly to the numeric user id
memdusa user link alice google:alice@acme.com   # binds on first Google sign-in by verified email
memdusa user identities alice
memdusa user unlink alice github:octocat
```

A handle that is not a numeric GitHub id is stored as a *pending* link and bound on the first successful sign-in.
**GitHub pending links bind by login only**, because the email returned by GitHub's `/user` is not verified. Google
pending links bind by verified email. If you know the numeric GitHub user id, use it to bind immediately.

A user who is already signed in with an API key can also choose **Link GitHub** / **Link Google** on the consent
page to link their own identity. This self-service link takes precedence over auto-provisioning. All link and unlink
changes are audited.

### Auto-provisioning

Off by default. Setting either allowlist creates a non-admin user on first successful sign-in, named after the GitHub
login or the email local part (deduplicated, for example `alice-2`):

```sh
export MEMDUSA_OAUTH_ALLOW_GITHUB_ORGS=acme,other-org   # active org membership required
export MEMDUSA_OAUTH_ALLOW_GOOGLE_DOMAINS=acme.com       # verified email domain, or the hd claim
```

For hosted-domain accounts, Google's `hd` claim is matched as well as the verified email domain. Auto-provisioning
never creates an admin, and it never fires for a link-only round trip from the consent page.

See also: [Deploying the server](deploy.md), [Users and API keys](users-and-keys.md),
[Access control](access-control.md), [Security model](security.md), [REST API](../reference/rest-api.md).
