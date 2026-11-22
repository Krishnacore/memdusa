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

### Adding a scenario

`scenarios/<name>/` with:
- `repo/`: the starting project
- `scenario.json`: `task_prompt` (state constraints that leave no trace in the code until they're implemented: that's
  what memory has to carry), `resume_prompt`, and `checks` (`hidden_tests`, `forbidden_patterns`, `unchanged_files`)
- `hidden_test.py`: acceptance tests run against B's final repo (with the repo on `PYTHONPATH`)

Check the fixture first: its own tests pass and the hidden tests fail.
