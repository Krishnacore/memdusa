# Retention and backups

History grows with every edit and command. memdusa prunes old data so the database and the brief stay fast, and it gives you backups and JSON Lines export for moving data between databases.

## What gets pruned

| What | Default | How |
|---|---|---|
| Hook activity (edits, commands, prompts, session start/end, compactions) | 30 days | Summarized, then deleted: each old session becomes one `session_summary` event, e.g. `Session a1b2c3d4: 14 file(s) changed (src/main.rs, ...), 22 command(s), 3 prompt(s). First request: "..."`. |
| Superseded checkpoints | 90 days | Deleted. The latest checkpoint of every task is always kept. |
| Finished runs and their steps | 90 days | Deleted. |
| Memories, and events agents logged explicitly | forever | Never pruned automatically. |

Team server audit events are kept for 365 days by default (`MEMDUSA_AUDIT_DAYS`, `0` keeps them forever) and are pruned by the same job.

`memdusa serve` prunes once a day; `--no-auto-prune` turns that off. In local mode, pruning runs at the end of a session, at most once a day.

## Run a prune by hand

```sh
memdusa prune --dry-run
memdusa prune --only-project github.com/acme/shop --activity-days 14
```

The command accepts `--dry-run`, `--only-project P`, `--activity-days N`, `--checkpoint-days N` and `--run-days N`. It runs locally or as an admin against a team server. A value of `0` means keep forever. The same windows can be set with `MEMDUSA_RETENTION_ACTIVITY_DAYS`, `MEMDUSA_RETENTION_CHECKPOINT_DAYS` and `MEMDUSA_RETENTION_RUN_DAYS`; see [Configuration](../reference/configuration.md) and the [CLI reference](../reference/cli.md).

**Note:** Pruning deletes old events and checkpoints without writing sync tombstones. A pull that encounters an upsert for a pruned row skips it, so a fresh client cannot resurrect pruned data. Real deletes always travel. See [Sync](../team/sync.md).

## Back up the database

```sh
memdusa backup /path/to/backup.db
```

The destination file must not exist. The command makes a consistent online copy of the local SQLite database with `VACUUM INTO`, so it is safe to run while memdusa is in use. It works only on a local SQLite database: against a team server it fails and tells you to use `memdusa export`. For Postgres, use `pg_dump`.

## Export and import as JSON Lines

```sh
memdusa export --only-project github.com/acme/shop -o memdusa-export.jsonl
memdusa import memdusa-export.jsonl
```

`memdusa export` writes memories, events, checkpoints and runs with their steps, but never users or API keys. Omit `--only-project` to export everything; omit `-o` to write to stdout. `memdusa import <file|->` keeps ids and timestamps, skips records that already exist (so it is safe to re-run), and re-applies [redaction](redaction.md).

To move a local history onto a team server, log in to the server first (you need an admin key) and pipe the export into the import:

```sh
memdusa --database-url sqlite://$HOME/.memdusa/memdusa.db export | memdusa import -
```

Server exports and imports are also available through the REST API. See the [REST API reference](../reference/rest-api.md) and [Users and API keys](../team/users-and-keys.md).

See also: [Memory hygiene](memory-hygiene.md), [Operations](../team/operations.md).
