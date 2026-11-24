# Users and API keys

Every request to a team server carries a credential, and the server decides who the caller is. This page covers
users, API keys, attribution and the shared bootstrap token. For what a user may *do* with a project, see
[Access control](access-control.md).

## How authentication works

| | |
|---|---|
| Keys | `mdk_` followed by 256 random bits. Only a SHA-256 hash is stored. A key is shown once, when it is created. Keys are named (`laptop`, `ci`) and record when they were last used. |
| When auth turns on | As soon as the first user exists — even if you created that user with the CLI while the server was running. Before then the server is open and logs a warning. Fine for localhost only. |
| Attribution | The server sets the identity from the key. Records and leases belong to `alice/claude-code`; two users' agents with the same name never share a lease, and a client cannot claim to be someone else. |
| Global memories | Private to their author. Alice's "prefers small PRs" never appears in Bob's brief. Project data is shared by users with access to that project. |
| Shared `--token` | Still accepted as an admin credential not tied to a user. Use it for bootstrap or break-glass, not for everyday work. |

## Create the first admin

Run this on the server (or from a machine with direct database access):

```sh
memdusa user add alice --admin
```

It creates the user and prints their first API key once. Copy it into your password manager. If you lose it, create
a new key; the old key's hash cannot be recovered. In a container, use
`docker exec <container> memdusa user add alice --admin`.

User names are 1–40 characters of `[a-z0-9._-]`.

## Manage users

```sh
memdusa user list
memdusa user disable bob                    # all of bob's keys stop working at once
memdusa user enable bob
```

Disabling a user invalidates all their keys immediately without deleting their data. Enabling them again does not
restore a revoked key.

## Manage keys

```sh
memdusa key create --name ci                # your own new key on a logged-in machine
memdusa key list
memdusa key revoke mdk_1a2b3c4d              # by display prefix, or by full id
```

Each command acts on the local database when run on the server, or through the API from any logged-in machine.
`memdusa status` shows who you are. An admin can act on another user:

```sh
memdusa user add bob
memdusa key create --user bob --name laptop
memdusa key list --user bob
```

In local mode, `key create` requires `--user`, because there is no signed-in caller to attribute the key to.
`key list` shows everyone's keys in local mode and only your own (or `--user`'s) in remote mode. `--name` defaults
to `default`. A revoked key cannot be used again; revocation is permanent.

## Sign in without a key

With `MEMDUSA_PUBLIC_URL` set, MCP clients and the CLI can use OAuth 2.1 instead of pasting an API key. The resulting
access tokens (`mdo_…`) are accepted anywhere an API key is, with the scopes chosen at sign-in. See
[OAuth sign-in](oauth.md).

## REST equivalents

The admin API mirrors these commands.

| Method | Path | Purpose |
|---|---|---|
| GET | `/v1/whoami` | Your user, admin flag, default access, grants, server version and backend |
| GET, POST | `/v1/me/keys` | List or create your own keys |
| DELETE | `/v1/me/keys/{id-or-prefix}` | Revoke one of your keys |
| GET, POST | `/v1/admin/users` | List users; create a user and their first key |
| POST | `/v1/admin/users/{name}/disable`, `/enable` | Disable or enable a user |
| POST | `/v1/admin/users/{name}/keys` | Create a key for a user |
| GET | `/v1/admin/keys?user=` | List keys |
| DELETE | `/v1/admin/keys/{id-or-prefix}` | Revoke a key |

User, key and grant changes are written to the audit log. See [Audit](../reference/rest-api.md) for the full route
list and [Access control](access-control.md) for grants.

See also: [Deploying the server](deploy.md), [Access control](access-control.md),
[OAuth sign-in](oauth.md), [Security model](security.md).
