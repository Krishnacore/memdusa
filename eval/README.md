# Evals

## Handoff eval (`handoff/run.py`)

memdusa's promise is that a different agent (or the same one after a crash) can **continue** interrupted work. This eval
measures exactly that, with real agents, and compares against no memdusa at all.

```sh
uv run eval/handoff/run.py                      # 1 trial, Claude Code on Sonnet, A interrupted after 90s
uv run eval/handoff/run.py --trials 5 --interrupt-after 60
```

Requirements: `cargo`, `uv`, and a logged-in `claude` CLI. **It runs real agent sessions on your account**: one trial
is one agent A session plus one agent B session per condition (a few minutes and roughly a dollar or two on Sonnet).

### How a trial works

1. **Agent A** gets the scenario's full task, which includes constraints the user states in conversation (e.g. "money
   is always integer cents", "refund reasons are stored lowercased"). It runs with memdusa hooks and MCP, and is
   **interrupted** after `--interrupt-after` seconds, like a closed terminal or a crash.
2. The repo and memdusa's database are snapshotted.
3. **Agent B** gets only *"Continue the work in this repository where the previous session left off"*, once per
   condition, starting from the same snapshot:
   - `memdusa`: hooks and MCP (the resume brief is injected at session start)
   - `baseline`: no memdusa, so B only sees the code A left behind
4. B's result is graded deterministically. There is no LLM judge, so scores are stable and free to compute.

Agents are isolated from your own setup: a settings file per agent with `--setting-sources project`,
`--strict-mcp-config`, and a per-trial `MEMDUSA_HOME` with a local database. Bash is limited to tests, the project
CLI and read-only commands.

### Metrics

| Metric | Meaning |
|---|---|
| hidden tests | Acceptance tests the agents never see (scenario's `hidden_test.py`). The headline number |
| constraint violations | Forbidden patterns (e.g. floats in money code) and files that must not change: things only the user's earlier words reveal |
| rework | Share of A's lines that B rewrote or deleted (0 = built on A's work, 1 = started over) |
| turns, cost | B's effort to finish |
| tool calls before first edit | How fast B gets productive |
| memdusa calls | Whether B used the MCP tools at all |

Results go to `eval/results/<timestamp>-<scenario>/`: `report.md`, `results.json`, each agent's full stream-json
transcript, the brief B would see (`brief-after-a.md`), and both B repos for inspection.

### Reading results

One trial is an anecdote: agents are nondeterministic, and *when* A is interrupted matters a lot. Compare conditions
over 5+ trials, and compare changes to memdusa (brief format, hooks) against the same settings.

### Scenarios

| Scenario | What it tests |
|---|---|
| `refunds` | The baseline scenario: A is interrupted mid-task while implementing partial refunds. Its default `a_mode` is `interrupt`. |
| `audit-trail` | A finishes only part 1 (the event queue) and stops; B must add the audit trail. The decisions about part 2 live only in A's last message, and a project convention lives only in a seeded memory. Its `a_mode` is `complete`. |

### Results so far (Sonnet, 3 trials each)

| Scenario | memdusa B: hidden tests | baseline B: hidden tests | baseline runs with constraint violations |
|---|---|---|---|
| `refunds` (A stopped after its first edit) | 12/12 in 3/3 trials | 11/12 | n/a |
| `audit-trail` (2026-11-22) | 100%, 0/3 runs with violations | 20% | 3/3 |

In `audit-trail`, baseline B always found the remaining work (the repo's README mentions an audit trail) but guessed its
own API and used the wall clock, because A's decisions and the timestamp convention were only in memdusa. memdusa B made
one MCP call on average and cost about the same ($0.09 vs $0.08 per run).

### Adding a scenario

`scenarios/<name>/` with:
- `repo/`: the starting project
- `scenario.json`: `name`, `task_prompt` (state constraints that leave no trace in the code until they're implemented:
  that's what memory has to carry), `resume_prompt`, and `checks` (`hidden_tests`, `forbidden_patterns`,
  `unchanged_files`), plus these optional fields:
  - `a_mode`: `"interrupt"` (default) stops A mid-task after `--interrupt-after-edits` edits (the timer is a safety
    limit); `"complete"` lets A run until it ends its turn naturally so the Stop hook records A's
    `last_assistant_message` for the brief. In both modes `--interrupt-after` is the safety timeout. The mode and
    whether A ended naturally are recorded in `results.json` and `report.md`.
  - `seed_memories`: a list of `{"content": "...", "kind": "...", "scope": "project"}` objects written with
    `memdusa remember` into the trial's database **before A runs**. They are project-scoped by default; use
    `"scope": "global"` for a global memory. They live only in the database (not the repo), so they appear in B's
    `brief-after-a.md` for the `memdusa` condition and are invisible to the `baseline` condition. `kind` is passed
    through to `memdusa remember --kind` (`fact`, `preference`, `decision`, `todo`, `note`; unknown values such as
    `convention` are stored as `fact`). This is how a convention stated in an earlier session is seeded.
- `hidden_test.py`: acceptance tests run against B's final repo (with the repo on `PYTHONPATH`)

Check the fixture first: its own tests pass and the hidden tests fail.

## Multi-agent eval (`multiagent/run.py`)

Where the handoff eval moves one task between agents, this one checks the **coordination machinery**: a coordinator
delegates several tasks as one durable run, workers in different harnesses claim their own steps, one worker crashes,
and a replacement in a third harness takes over the *stale* step from a brief that still contains the original
assignment. It is the scripted version of the setup described in the docs under
[Multi-agent work and delegation](../docs/src/guides/multi-agent.md).

```sh
uv run eval/multiagent/run.py --dry-run      # stub harnesses: exercises orchestration + grading for free
uv run eval/multiagent/run.py --self-test    # unit checks for the grading functions
uv run eval/multiagent/run.py                # 1 real trial
uv run eval/multiagent/run.py --trials 3 --lease-secs 45
uv run eval/multiagent/run.py --harnesses worker1=opencode,worker2=codex,replacement=claude

# Matrix: rotate every harness through every role (one trial per harness)
uv run eval/multiagent/run.py --matrix --dry-run            # 7 stub trials, incl. a fake hermes in a temp HOME
uv run eval/multiagent/run.py --matrix                       # 6 real trials: opencode, codex, claude, cursor, pi, agy
uv run eval/multiagent/run.py --matrix --allow-user-config   # also includes hermes (7 trials)
uv run eval/multiagent/run.py --matrix --harnesses-only opencode,cursor,pi
```

Requirements: `cargo`, `uv`, plus **logged-in CLIs for whichever harnesses you select**. All seven headless
launchers are verified live on this machine:

| Harness | Headless command | Install scope |
|---|---|---|
| OpenCode | `opencode run --standalone --auto --model opencode/hy4-flash-free` | project (`--standalone` so the private server inherits the worker env) |
| Codex | `codex exec --trust --dangerously-bypass-hook-trust … < /dev/null` | project |
| Claude Code | `claude -p … --model haiku --strict-mcp-config` | project |
| Cursor | `cursor-agent -p --trust --approve-mcps --output-format text` | project |
| pi | `pi -p --approve --model anthropic/claude-haiku-4-5` | project (logged in with an Anthropic account) |
| Antigravity | `agy -p` (`~/.local/bin/agy`) | project (`.agents/` hooks run without a trust prompt) |
| Hermes | `hermes --accept-hooks -z` (`~/.local/bin/hermes`) | **user** — see below |

Nothing except Hermes is installed at user scope: memdusa is installed with `--scope project --database-url …`
into a fresh scratch repo per trial, and every worker environment is stripped of `MEMDUSA_*` except `MEMDUSA_RUN`,
`MEMDUSA_TASK`, `MEMDUSA_PARENT` and `MEMDUSA_STEP_LEASE_SECS`. All workers run with stdin from `/dev/null`, output
captured to the trial log, a timeout and their own process group (so a crashing worker can be killed the instant its
first `file_change` reaches the database).

**Rough cost:** most runners use a subscription or a free model (OpenCode's `hy4-flash-free`; pi's Haiku on the
operator's Anthropic account; Cursor, Codex, Antigravity and Hermes on their subscriptions), so a trial is usually
well under $1 and often effectively free. Claude Code's replacement on Haiku is the only metered line (a few cents
per trial). `--dry-run` is completely free: every harness is replaced by a stub that drives the real hooks and
database but makes no model calls.

**Hermes is opt-in.** It has only one config (`~/.hermes/config.yaml`), so it is skipped in `--matrix` unless you
pass `--allow-user-config` (a clear note is printed in the report). When enabled, its runner copies the original
bytes aside, runs `memdusa install hermes --scope user --database-url …`, launches Hermes with `TERMINAL_CWD` and
cwd set to the scratch repo, then restores the original byte for byte (hash-verified) from a `try/finally` **and**
a SIGINT/SIGTERM handler, and removes `config.yaml.memdusa-backup` if it is identical. It also restores
`~/.hermes/shell-hooks-allowlist.json` (Hermes auto-approves the trial's hooks there) and removes any backups
Hermes creates under `~/.hermes/backups/config/` during the run, so no global Hermes state is left modified. Dry
runs always use a synthetic config in the trial directory (`--hermes-config` overrides the path), never the real
file. Hash evidence (config and allowlist, before and after) is written to `hermes-config-hashes.json` in the trial
directory.

**Cursor double-recording.** Cursor runs Claude Code hooks by default ("Third-Party Imports"). When a trial installs
both Cursor and Claude Code hooks, Cursor's edits can be journaled twice — once with source `cursor`, once with
`claude-code`. The eval installs only the harnesses used in that trial, and the `attributed` metric only requires
the sources that actually played a role, so an extra `claude-code` line in "Who did what" is tolerated and
documented here rather than counted as a failure.

### How a trial works

The `shop-team` scenario starts from a tiny repo with `ORDERS = {1: 5000, 2: 1250}`.

1. The coordinator (the eval script itself) runs `memdusa delegate "release refunds invoices" refunds invoices` and
   parses the `MEMDUSA_RUN` / `MEMDUSA_TASK` launch lines.
2. **Worker 1** (default OpenCode) gets the full `refunds` assignment in its prompt. The orchestrator polls
   `memdusa export` and kills the process group as soon as its first `file_change` event appears. Its step goes stale
   when the 45 s lease expires.
3. **Worker 2** (default Codex) gets the `invoices` assignment in a different harness, runs to completion and calls
   `task_complete` itself.
4. After the stale lease is observed, the replacement's brief is captured with `memdusa resume` and its env, then
   the replacement (default Claude Code) is launched with only "Continue your delegated task." It sees the full
   assignment through the worker-scoped brief ("## Your assignment") and claims the stale step.

### Matrix mode

`--matrix` rotates the selected harnesses H so trial *i* uses `worker1=H[i]`, `worker2=H[i+1]`,
`replacement=H[i+2]` (modulo the set size). That is `len(H)` trials, with every harness appearing once in every
role — e.g. the default six produce `(opencode, codex, claude)`, `(codex, claude, cursor)`, `(claude, cursor, pi)`,
`(cursor, pi, antigravity)`, `(pi, antigravity, opencode)`, `(antigravity, opencode, codex)`. `--harnesses-only`
restricts H; `--allow-user-config` adds hermes as a seventh. The report has one row per trial with the three roles
and every metric above, plus a per-harness summary table with pass/fail and the failing metric(s) for each harness
in each role. `--trials` is ignored in matrix mode.

### Metrics

Results go to `eval/results/<timestamp>-multiagent/`: `report.md`, `results.json`, each agent's log, both versions
of the coordinator brief, the replacement's brief, the final `memdusa export` and the hidden-test junit files.

| Metric | Meaning |
|---|---|
| hidden tests | Per-task acceptance tests the workers never see (refunds behaviour, lowercased reason, over-refund `ValueError`, own tests file; invoice format). The headline number |
| steps done | Both `refunds` and `invoices` steps are `completed` in the run |
| assignment | The replacement's brief contains "## Your assignment" and the assignment's distinctive tail: proof the handoff carried the full task, not just "continue" |
| lines kept | Share of worker 1's `shop/refunds.py` lines the replacement preserved (1 = built on its work, 0 = started over) |
| attributed | "Who did what" names the three sources of the harnesses used (`opencode`, `codex`, `claude-code` by default). An extra `claude-code` source in a Cursor session is tolerated (see above) |
| turns, cost | What the harness reports about its own effort (Codex/Claude Code JSON; best-effort from the new harnesses' output) |

Use `--keep` to retain each trial's scratch repo, venv and database for inspection. One trial is an anecdote; use
`--trials 3` or more (or `--matrix`) before drawing conclusions.
