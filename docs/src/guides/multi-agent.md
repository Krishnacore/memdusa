# Multi-agent work and delegation

You can have a coordinator agent delegate tasks to workers in other harnesses (Claude Code, OpenCode, Codex, ...) on the same project. memdusa records which task each worker owns, gives each worker its own brief, and can make the tasks durable steps so no two workers do the same work and a crashed worker can be replaced.

## Mark a worker with `MEMDUSA_TASK`

Launch the worker with its task in the environment:

```sh
MEMDUSA_TASK=install-scripts MEMDUSA_PARENT=lead-session claude   # or opencode, codex, ...
```

`MEMDUSA_TASK` marks the worker and is required for worker behavior. `MEMDUSA_PARENT` is free text naming the coordinator and is optional. Use one short, stable name per task; it becomes the worker's task key.

For a headless worker, put the task on the launch line:

```sh
MEMDUSA_RUN=0198f0a4-7c2e-7d31-9a40-2b1e6f0d9c11 MEMDUSA_TASK=refunds   claude -p "Continue your delegated task."
MEMDUSA_RUN=0198f0a4-7c2e-7d31-9a40-2b1e6f0d9c11 MEMDUSA_TASK=invoices  codex exec "Continue your delegated task." < /dev/null
MEMDUSA_RUN=0198f0a4-7c2e-7d31-9a40-2b1e6f0d9c11 MEMDUSA_TASK=docs      opencode run --standalone "Continue your delegated task."
```

`codex exec` reads stdin when it is not a terminal, so close it with `< /dev/null` or it waits.

This changes what memdusa records:

- **Worker sessions are recorded.** At session start memdusa journals a `delegated_session` event with the task, parent and harness. This happens in local mode and hosted mode alike.
- **Worker checkpoints are task-scoped.** A checkpoint saved without an explicit `task` (with the `checkpoint` tool or the automatic fallback checkpoint) gets `task = MEMDUSA_TASK`, so it never replaces the coordinator's checkpoint. An explicit `task` argument wins.
- **A worker's brief is its task.** It shows only that task's latest checkpoints and recent activity. Its "Last words" come from previous sessions of the same task, whatever harness they ran in. When a worker hits its usage limit, a replacement started with the same `MEMDUSA_TASK` resumes exactly there. Both the `resume` tool and `memdusa resume` use that task; the tool also accepts an optional `task` argument.
- **The coordinator's brief gets "Delegated work".** Without `MEMDUSA_TASK`, it lists tasks delegated in the last 7 days (up to 10, newest first): harness, status (`active` until the worker's session ends, then `ended`), the task's latest checkpoint goal and next steps, and the worker's last message. Workers' checkpoints and messages stay out of the coordinator's own "Where things stand" and "Last words".
- **Worktrees stay separate.** Workers in sibling git worktrees share the project key, but the brief compares code state with the latest checkpoint from this worktree, lists this worktree's recent file edits first, and summarizes the others as `Other worktrees: ../memdusa-eval2 (4 files)`. A checkpoint from another worktree is noted calmly instead of raising a "Code state differs" warning.

`MEMDUSA_PARENT` alone does not create a worker. It only names the coordinator in the record.

## Delegate through a durable run

`MEMDUSA_TASK` alone records who works on what. To also stop two workers taking the same task and to take over from a worker that died, delegate through a durable run. Each task becomes a step with its own lease.

```sh
memdusa delegate "windows release" install-scripts docs     # or the `delegate` MCP tool
```

```text
Delegated run `0198f0a4-7c2e-7d31-9a40-2b1e6f0d9c11` (windows release): 2 task(s).
- MEMDUSA_RUN=0198f0a4-7c2e-7d31-9a40-2b1e6f0d9c11 MEMDUSA_TASK=install-scripts
- MEMDUSA_RUN=0198f0a4-7c2e-7d31-9a40-2b1e6f0d9c11 MEMDUSA_TASK=docs
```

Launch one worker per task with its env line (any harness). Delegating again with the same name, or with `--key`, reuses the running run and adds the missing tasks.

## Store an assignment with each task

Store what the coordinator actually asked for at delegation time. Append `=<assignment>` to a task name, or pass a JSON file with `--from-file` (`-` reads stdin):

```sh
memdusa delegate "windows release" 'docs=Write the installer guide' install-scripts
memdusa delegate "release refunds" --from-file tasks.json
```

The JSON file is a list of task objects. Plain names are allowed alongside them:

```json
[
  {"task": "refunds", "assignment": "Build refunds in shop/refunds.py and add tests."},
  "docs"
]
```

The `delegate` MCP tool and `POST /v1/delegate` accept the same tasks as plain names or `{"task": "…", "assignment": "…"}` objects. Assignments are redacted, capped at 8,000 characters, and stored in the task's step input. The output reports `N assignments stored`; the launch env lines stay clean. Reusing a run updates the assignment of a pending task. A running or done task keeps its original, and is named in the output.

A worker's brief opens with "## Your assignment" using the stored text, marked `_stored by the coordinator_`, even if no prompt was ever recorded for the task. If that worker was told something different, its earliest prompt is quoted below as `_first worker was told…_`, followed by any later instruction that differs. With no stored assignment the brief falls back to the earliest prompt. The coordinator's "Delegated work" shows the first line (up to 120 characters) of each stored assignment.

## Claim, work and finish a task

With `MEMDUSA_RUN` and `MEMDUSA_TASK` set, the worker's SessionStart hook claims its step. The claim is owned by the agent name and session id and carries a 15-minute lease:

- The brief opens with a notice saying the task was *claimed* (and, when it took over, that the previous attempt by X went stale), that it is *already done* (with the recorded result: stop, or confirm with the user), or that it is *held by another live worker* (coordinate; do not duplicate).
- Restarting a worker with `/clear` or after compaction re-claims its own step.
- Every other hook event renews the lease (prompts, tool use, stop), so a working agent never goes stale. `SessionEnd` does not finish the task: the lease just expires, which is what a worker that stopped mid-task should look like.
- Finishing is explicit. Use the `task_complete` MCP tool or:

  ```sh
  memdusa task done --summary "Installer scripts added in scripts/."
  memdusa task fail "Blocked: the signing key is unavailable."
  ```

  `task_fail` / `memdusa task fail` gives the task up so a replacement can claim it and is told why it failed. The MCP tools and the CLI default `run` and `task` to `MEMDUSA_RUN` / `MEMDUSA_TASK`; the `memdusa mcp` proxy passes them over a hosted server. A finished task is never redone: a worker started on it is told so.
- **Stale takeover.** A `running` step whose lease expired is *stale*. A replacement launched with the same env line, in any harness, claims it. A live lease is never taken silently.

The coordinator's "Delegated work" shows each task's step status instead of the session-based guess: `pending`, `running by X`, `stale (owner X, lease expired 12m ago)`, `done` (with the result) or `failed: reason`. With several runs, tasks are grouped under the run's name. Tasks delegated by `MEMDUSA_TASK` alone keep the earlier behavior. Close the run with `run_finish` when the work is over.

`MEMDUSA_STEP_LEASE_SECS` overrides the lease length (default `900` seconds). It is read where the claim happens: on the server in hosted mode. See [Configuration](../reference/configuration.md).

## Pass the environment to workers by harness

How the worker's `MEMDUSA_RUN` / `MEMDUSA_TASK` reach memdusa depends on the harness:

| Harness | What to do |
|---|---|
| Claude Code | It passes the launch environment to its stdio MCP servers. The headless launch line above works as-is. |
| Codex CLI | `memdusa install codex` writes the four variables into `env_vars` in `~/.codex/config.toml`, plus `default_tools_approval_mode = "approve"` so headless tool calls do not need an interactive approver. Use `codex exec ... < /dev/null`. |
| OpenCode v2 | Plugins and MCP servers run in a shared background service that does not inherit the shell environment. A delegated worker must launch with `opencode run --standalone` (a private server that does inherit it). |
| Hermes Agent | Hermes runs every session, including `hermes -z`, in one long-lived gateway, so hooks and MCP servers never see a worker's shell environment. memdusa ignores `MEMDUSA_RUN` / `MEMDUSA_TASK` there. Put `delegate`'s `MEMDUSA_RUN=<id> MEMDUSA_TASK=<task>` line in the worker's prompt instead; the claim notice then gives it the `run` / `task` to pass to `task_complete`. |
| pi, Cursor, Antigravity CLI | Configure the database at install time with `--database-url` or `memdusa login` rather than relying on shell environment variables. |

Antigravity's headless mode soft-denies MCP calls it cannot ask approval for. `memdusa install antigravity` adds a PreToolUse hook that auto-approves only calls to memdusa's own server, so `checkpoint` works with plain `agy -p`. Its environment does reach the MCP server. See the [harness overview](../harnesses/index.md) for installation and trust steps.

**Note:** A single Hermes prompt line is recognized only when it contains exactly one distinct `MEMDUSA_RUN` / `MEMDUSA_TASK` pair. Pasting the whole `delegate` output into a prompt does not make the agent a worker.

See also: [Durable run protocol](../reference/durable-runs.md), [MCP tools, resources and prompts](../reference/mcp.md), [Sync mode](../team/sync.md) (delegated work stays server-authoritative in sync mode).
