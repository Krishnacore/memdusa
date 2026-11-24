# Sync

Sync replicates chosen projects between a local SQLite database and a team server. It is for offline-first work:
agents write locally with no network in the hot path, and a one-shot command, a watcher or sync mode copies the
result to the server. Nothing leaves a project you have not opted into.

## Set up sync

Sync needs both sides. The local side is `--database-url` (default `~/.memdusa/memdusa.db`), and the remote side is
your `memdusa login`, with either an API key or an OAuth sign-in.

```sh
memdusa login https://memdusa.example.com --token mdk_…   # or --browser / --device
memdusa sync add github.com/acme/shop                      # opt in; first run does a full push and pull
memdusa sync add --all                                     # every project you can read on the server
memdusa sync add --globals                                 # your private global memories, separately
memdusa sync                                               # push local changes, then pull remote ones
```

`memdusa sync add` is best effort. If the server is offline, the opt-in is still recorded and the next
`memdusa sync` performs the initial full sync. Cursors live in the local `sync_state` table. A stable per-server
client id in `~/.memdusa/config.json` marks changes you pushed, so your own echo comes back on pull but is skipped.
Pushes are in batches of at most 500 changes and are idempotent, because records have globally unique UUIDv7 ids.
Pull pages have a cursor and `has_more`; cursors advance only after a successful page, so a failure resumes where it
stopped.

```sh
memdusa sync list                           # opted-in projects and cursors
memdusa sync status                         # last ok, pending changes, last error, conflicts, watcher
memdusa sync remove github.com/acme/shop    # stop syncing; local data is kept
memdusa memories revisions <id>             # superseded versions of one memory
```

## Resolve conflicts

Every memory, event and checkpoint write appends a `changes` row in the same transaction, on both sides. The same
rules apply when pushing and pulling:

- **Events and checkpoints** are inserted if absent by id. They never conflict and are never duplicated on re-push.
- **Memories** are last-writer-wins by `updated_at`. Ties go to the larger id, then to the larger content for an
  exact millisecond race. The losing version is kept in `memory_revisions` on both sides. `memdusa sync status`
  lists recent losers and `memdusa memories revisions <id>` shows them.
- **Deletes** win unless the edit is newer than the tombstone. An older offline edit is ignored, and a stale
  tombstone cannot delete a newer edit. Ties favour the delete.

Push requires write access and pull requires read access. Both are audited as `sync.push` and `sync.pull` summaries
with counts only, never record bodies. A user cannot push records attributed to another user; agent names are
re-qualified as `user/agent` on the server, as hooks do.

Pruning deletes old events and checkpoints without writing tombstones. A pull of an `upsert` reads the current row
and skips the change if that row was pruned, so a fresh client cannot resurrect pruned data. Real deletes (`op =
delete`) need no row and always travel. A pruned local row is likewise skipped when pushing. The change log is
append-only, so cursors never reuse timestamps.

**Not synced:** runs and steps (server-authoritative), embeddings (each side embeds its own pulled memories if an
embedder is configured), and users, keys, grants, audit and OAuth state. Global memories are private to their
server-side owner and need the separate `--globals` opt-in.

## Use sync mode

`memdusa login <url> --sync` (also with `--token`, `--browser` or `--device`) saves the server *and* a flag. Hooks,
`memdusa mcp` and all CLI reads and writes then use the local database, so there is no network in the hot path. Only
delegated work and background sync contact the server.

```sh
memdusa login https://memdusa.example.com --sync --token mdk_…
memdusa sync mode off                        # or on; toggles the saved login
memdusa status                               # reports both sides in sync mode
```

The first hook in a project that is not opted in adds it automatically: the opt-in and the seeding of existing
records happen locally, and the first sync pushes and pulls them. Set `MEMDUSA_SYNC_AUTO_ADD=0` to require an
explicit `memdusa sync add <project>` instead.

Delegated work stays server-authoritative even in sync mode. `delegate`, `run_*`, `task_complete` / `task_fail`, and
the SessionStart step claim and heartbeats go straight to the server. Nothing creates a local lease. If the server
is unreachable, the claim fails visibly at the top of the brief notice (telling you no local lease was created) and
heartbeats are skipped silently. Ordinary memory and event writes keep working offline.

`memdusa status` in sync mode shows the local database, whether the server is reachable, opted-in projects, pending
local changes, watcher state and the last sync time.

## Run the watcher

```sh
memdusa sync --watch                         # push and pull every opted-in project, forever
memdusa sync --watch --interval 30s          # custom interval (default 15s)
memdusa sync status                          # also shows the watcher pid and last sync
```

Only one watcher runs per `MEMDUSA_HOME`, using a pid lock file; a second instance exits with a message. Failures
back off exponentially from 1 second up to 5 minutes and reset after a success. A pass that moved data prints one
log line; a no-op pass is silent. SIGINT and SIGTERM stop it cleanly and release the lock.

After SessionEnd — and after Stop when files changed — sync mode spawns a detached `memdusa sync --quiet`. It is
skipped when a watcher holds the lock or a sync ran in the last 10 seconds. Set `MEMDUSA_SYNC_OPPORTUNISTIC=0` to
turn that off.

### Run as a service

```sh
memdusa sync service install                 # launchd agent on macOS, systemd user unit on Linux
memdusa sync service uninstall
```

The command prints the file it wrote and how to start and stop it (`launchctl load -w …` on macOS,
`systemctl --user enable --now memdusa-sync.service` on Linux). On Windows it prints Task Scheduler instructions
instead. All flags and environment variables are listed in the [CLI reference](../reference/cli.md) and
[Configuration](../reference/configuration.md).

See also: [Team server overview](index.md), [Deploying the server](deploy.md),
[Access control](access-control.md), [Operations](operations.md).
