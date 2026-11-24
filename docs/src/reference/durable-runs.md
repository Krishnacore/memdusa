# Durable run protocol

A durable run is multi-step work whose steps are journaled. If a step completes, a later agent reuses its recorded output instead of redoing the side effect. Use one for migrations, deploys, batch edits and anything else that must not happen twice.

## The protocol

```text
run_start {name, idempotency_key?}            -> run (re-entered if the key exists)
for each step (stable, deterministic keys):
  run_step {run_id, step_key}
    -> {action: "replay", output}             already done: use output, do NOT redo it
    -> {action: "execute", attempt}           do the work, then:
         run_step_complete {output} | run_step_fail {error}
run_finish {outcome: completed|failed|cancelled}
```

Use a stable, deterministic key for every step, such as `1-create-table`. The key identifies the unit of work across agents and restarts. If a step returns `replay`, use the recorded output and move on. If it returns `execute`, do the work, then report the result or the error.

`run_start` accepts an optional `input`, `lease_secs` (default `600`) and `project`, as well as `idempotency_key`. Starting again with the same key re-enters the existing run and tries to take its lease; a terminal or still-leased run is returned as-is. `run_finish` accepts an optional `output` or `error` and releases the lease. See [MCP tools](../reference/mcp.md) for every argument and the [REST API reference](../reference/rest-api.md) for the HTTP equivalents.

## Leases, crashes and takeover

One agent holds the run's lease at a time. Each `run_step` call renews it, so a working agent never goes stale. The default length is 600 seconds.

If the owner disappears, the lease expires and `resume` shows the run as **STALE**. Any agent can take it over:

```text
run_claim {run_id}                 -> run and its full step journal
```

The claim fails while another agent holds a live lease. Pass `force: true` only when you know that agent is gone. On takeover, completed steps replay unchanged; the step that crashed mid-flight re-executes with `attempt: 2` and the previous error attached. `run_get` returns a run with its step journal, and `run_list` lists runs by status (`running`, `completed`, `failed`, `cancelled`).

Step outputs are journaled and redacted before storage, so a replayed step returns the redacted output. Do not pass secrets through runs. See [Secret redaction](../guides/redaction.md).

## Delegated runs

A coordinator can give each delegated task its own step and lease with `memdusa delegate`. Those steps use a separate, 15-minute lease claimed at the worker's session start and renewed by its hook events. The worker finishes its step with `task_complete` or `task_fail`, and the coordinator's brief shows the status of every task. See [Multi-agent work and delegation](../guides/multi-agent.md).

## Where runs live

There is no general `memdusa run` CLI command: use the MCP tools or the REST API. `memdusa delegate` is the CLI for creating a run whose steps are delegated tasks.

In local mode, runs and steps live in the local database. On a team server they are server-authoritative: `run_start`, `run_step`, `run_claim`, `task_complete` and `task_fail` go to the server. This stays true in [sync mode](../team/sync.md), where ordinary memory and event writes use the local database. Finished runs and their steps are removed by [retention](../guides/retention.md) after 90 days by default.

See also: [MCP tools, resources and prompts](mcp.md), [Multi-agent work and delegation](../guides/multi-agent.md).
