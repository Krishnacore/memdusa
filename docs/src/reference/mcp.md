# MCP tools, resources and prompts

Any MCP-capable harness connects to memdusa over stdio or streamable HTTP and gets the same tools, resources and prompts. A local harness launches `memdusa mcp --agent <name>`; a team server exposes them at `/mcp`. This page lists every definition in `crates/memdusa/src/mcp.rs`.

Arguments are shown with their JSON type. **Required** means the tool call fails without them. Optional arguments use the stated default. A `project` argument falls back to the current project, and a `task` argument falls back to the `MEMDUSA_TASK` the agent was launched with.

## Memory and progress tools

### `resume`

Get the hand-off brief: latest checkpoints, unfinished durable runs, memories and recent activity. Call this first in every session.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `project` | string | no | Project key, e.g. `github.com/acme/shop`. Defaults to the current project. |
| `task` | string | no | Brief this delegated task only (checkpoints and last words of its workers). Defaults to `MEMDUSA_TASK`; omit as a coordinator to see all delegated work. |
| `git` | object | no | Code state, attached automatically by the client. Hidden from the model's advertised schema. |

### `memory_save`

Remember a fact, preference, decision or todo across sessions and tools. Identical memories are deduplicated.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `content` | string | yes | The knowledge to remember, as a self-contained sentence. |
| `kind` | `fact` \| `preference` \| `decision` \| `todo` \| `note` | no (`fact`) | Memory kind. |
| `tags` | array of strings, or one newline-separated string | no (`[]`) | Tags for the memory. |
| `global` | boolean | no (`false`) | Make the memory visible from every project. |
| `project` | string | no | Project key. Ignored when `global` is true. |

### `memory_update`

Correct or refine an existing memory in place (by id) instead of saving a near-copy. Only the fields you pass change.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | yes | Id of the memory to change. |
| `content` | string | no | The corrected knowledge, replacing the old content. |
| `kind` | `fact` \| `preference` \| `decision` \| `todo` \| `note` | no | Replaces the kind. |
| `tags` | array of strings, or one newline-separated string | no | Replaces the tags. |

### `memory_search`

Search memories (project and global) by words, optionally filtered by kind.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `query` | string | no | Words to match. Omit to list the most recent memories. |
| `kind` | `fact` \| `preference` \| `decision` \| `todo` \| `note` | no | Only return memories of this kind. |
| `limit` | integer | no | Maximum number of results. |
| `project` | string | no | Project key. Defaults to the current project. |

### `memory_review`

Find memories that need consolidating: related ones to merge, ones mentioning files changed since they were written, and old todos. See [Memory hygiene](../guides/memory-hygiene.md).

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `project` | string | no | Project key. Defaults to the current project. |

### `memory_forget`

Delete a memory that is wrong or obsolete.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | yes | Id of the memory to delete. |

### `checkpoint`

Save a progress checkpoint so any agent can resume this work later. Call after meaningful progress and before ending a session.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `goal` | string | yes | What you are trying to achieve overall. |
| `done` | array of strings, or one newline-separated string | no (`[]`) | What is already done. |
| `next` | array of strings, or one newline-separated string | no (`[]`) | Concrete next actions, in order. |
| `blockers` | array of strings, or one newline-separated string | no (`[]`) | Blockers. |
| `files` | array of strings, or one newline-separated string | no (`[]`) | Files a successor should look at first. |
| `notes` | string | no | Free-form notes. |
| `task` | string | no | Workstream name. Defaults to the delegated task (`MEMDUSA_TASK`) this agent was launched with. |
| `project` | string | no | Project key. Defaults to the current project. |
| `git` | object | no | Code state, attached automatically by the client. Hidden from the model's advertised schema. |

### `log_event`

Append an entry to the project's activity journal (notable actions, commits, test results, discoveries).

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `kind` | string | yes | e.g. `message`, `tool_call`, `file_change`, `commit`, `test_run`, `note`. |
| `content` | string | yes | What happened. |
| `session` | string | no | Session id to attribute the event to. |
| `data` | JSON object | no | Structured details. |
| `project` | string | no | Project key. Defaults to the current project. |

## Durable run tools

The protocol these tools implement is described in [Durable run protocol](durable-runs.md).

### `run_start`

Start a durable multi-step run, or re-enter an existing one with an idempotency key. Returns the run with its journaled steps.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `name` | string | yes | Human-readable name, e.g. `migrate users table`. |
| `idempotency_key` | string | no | Reuse an existing run with the same key instead of starting a new one. |
| `input` | JSON value | no | Run input. |
| `lease_secs` | integer | no (`600`) | Lease length in seconds. Each step call renews it. |
| `project` | string | no | Project key. Defaults to the current project. |

### `run_step`

Enter a step of a durable run. Returns `{ "action": "replay", "output": … }` if the step already completed (do not redo it), or `{ "action": "execute", "attempt": … }` when you must perform it and then call `run_step_complete` or `run_step_fail`.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `run_id` | string | yes | Id of the run. |
| `step_key` | string | yes | Stable, deterministic key for this step, e.g. `1-create-table`. |
| `input` | JSON value | no | Step input. |

### `run_step_complete`

Record the result of a step you executed. The output is journaled and replayed on resume.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `run_id` | string | yes | Id of the run. |
| `step_key` | string | yes | Key of the step. |
| `output` | JSON value | no | Result to journal; returned verbatim on replay. |

### `run_step_fail`

Record that a step failed. Calling `run_step` again for it retries it.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `run_id` | string | yes | Id of the run. |
| `step_key` | string | yes | Key of the step. |
| `error` | string | yes | Why the step failed. |

### `run_finish`

Finish a durable run as completed, failed or cancelled, releasing its lease.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `run_id` | string | yes | Id of the run. |
| `outcome` | `completed` \| `failed` \| `cancelled` | yes | Final outcome. |
| `output` | JSON value | no | Run output. |
| `error` | string | no | Error detail, e.g. when the outcome is `failed`. |

### `run_claim`

Take over a durable run, for example a STALE one left by a crashed agent or another tool. Returns the run and its step journal.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `run_id` | string | yes | Id of the run. |
| `force` | boolean | no (`false`) | Take over even if another agent holds a live lease. |

### `run_get`

Get a durable run with its full step journal.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | yes | Id of the run. |

### `run_list`

List durable runs for the project (or all projects), newest first.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `status` | `running` \| `completed` \| `failed` \| `cancelled` | no | Only return runs in this state. |
| `all_projects` | boolean | no (`false`) | List runs from every project you can read. |
| `project` | string | no | Project key. Defaults to the current project. |

## Delegation tools

See [Multi-agent work and delegation](../guides/multi-agent.md).

### `delegate`

Split work into tasks for other agents (any harness). Each task is a plain name or a `{ "task": "…", "assignment": "…" }` object. Creates one durable run with a pending step per task and returns, per task, the `MEMDUSA_RUN=… MEMDUSA_TASK=…` line to launch its worker with.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `name` | string | yes | Name of the piece of work, e.g. `windows release`. |
| `tasks` | array of strings or `{task, assignment}` objects | yes | One short, stable name per task; each becomes a worker's `MEMDUSA_TASK`. A stored assignment is shown in the worker's brief even if no prompt was recorded. A plain string keeps the first-prompt-as-assignment behavior. |
| `idempotency_key` | string | no | Calling again with the same key (or, without one, the same name) reuses the run and adds missing tasks. |
| `project` | string | no | Project key. Defaults to the current project. |

### `task_complete`

Worker: mark your delegated task done, with a short summary of what was done and where. Do this once, when the task is finished.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `summary` | string | no | What was done and where to find it; shown to the coordinator and to anyone who starts this task again. |
| `run` | string | no | Defaults to the `MEMDUSA_RUN` this worker was launched with. |
| `task` | string | no | Defaults to the `MEMDUSA_TASK` this worker was launched with. |

### `task_fail`

Worker: give up on your delegated task, with the reason. A replacement worker can claim it again.

| Argument | Type | Required | Meaning |
|---|---|---|---|
| `reason` | string | yes | Why the task cannot be finished. |
| `run` | string | no | Defaults to the `MEMDUSA_RUN` this worker was launched with. |
| `task` | string | no | Defaults to the `MEMDUSA_TASK` this worker was launched with. |

## Resources

Resources return Markdown (`text/markdown`). They are useful in clients that surface resources better than tool calls.

| URI | Type | Meaning |
|---|---|---|
| `memdusa://brief` | Resource | The same text `resume` returns, for the default project. |
| `memdusa://memories` | Resource | Saved memories for the current project plus visible globals, grouped by kind. |
| `memdusa://brief/{project}` | Resource template | The brief for an explicit, URL-encoded project key, e.g. `memdusa://brief/github.com%2Facme%2Fshop`. |

An unknown URI returns an MCP "resource not found" error. The template argument is the project key; it has no separate options.

## Prompts

| Name | Arguments | Meaning |
|---|---|---|
| `resume` | `project` (string, optional; defaults to the current project) | The brief plus "Continue from this brief: verify the code state first, then carry on with the next steps." |
| `checkpoint` | none | Asks the agent to save a checkpoint now (goal, done, next, blockers, key files) and then call `memory_save` for decisions, preferences and project facts. |

Many clients show prompts as slash commands. The stdio→HTTP proxy forwards resources and prompts to the remote server, like tools.

See also: [Durable run protocol](durable-runs.md), [Memory hygiene](../guides/memory-hygiene.md), [Configuration](configuration.md).
