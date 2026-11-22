#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# ///
"""Handoff eval: does memdusa let agent B continue agent A's interrupted work?

For each trial:
  1. Agent A starts the scenario's task (with memdusa hooks + MCP) and is
     interrupted after --interrupt-after seconds, like an abandoned or crashed
     session.
  2. The repo (and memdusa's database) are snapshotted.
  3. Agent B gets only "continue where the previous session left off", once
     per condition, from the same snapshot:
       memdusa   - hooks + MCP (the resume brief is injected at session start)
       baseline  - no memdusa; B only sees the code A left behind
  4. B's result is graded deterministically: hidden acceptance tests (never
     shown to the agents), forbidden patterns (the user's stated constraints),
     files that must not change, how much of A's finished work B rewrote, and
     B's effort (turns, cost, time to first edit).

Agents run fully isolated from your own setup: their own settings file, only
the MCP servers we pass, and a per-trial MEMDUSA_HOME with a local database.

Usage:
  uv run eval/handoff/run.py --trials 3
  uv run eval/handoff/run.py --scenario refunds --interrupt-after 90 --model sonnet
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime as dt
import difflib
import json
import os
import re
import shutil
import signal
import statistics
import subprocess
import sys
import threading
import time
import xml.etree.ElementTree as ET
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
EDIT_TOOLS = {"Write", "Edit", "MultiEdit", "NotebookEdit"}
# What agents may run without a human: tests, the CLI, read-only inspection.
ALLOWED_BASH = [
    "Bash(python3 -m pytest:*)", "Bash(python -m pytest:*)", "Bash(pytest:*)",
    "Bash(python3 -m shop:*)", "Bash(python -m shop:*)", "Bash(python3 -c:*)",
    "Bash(ls:*)", "Bash(cat:*)", "Bash(git status:*)", "Bash(git diff:*)", "Bash(git log:*)",
]


def sh(cmd: list[str], cwd: Path | None = None, env: dict | None = None, check: bool = True) -> str:
    r = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True, errors="replace")
    if check and r.returncode != 0:
        raise RuntimeError(f"{' '.join(map(str, cmd))} failed:\n{r.stdout}\n{r.stderr}")
    return r.stdout


# ---------------------------------------------------------------- setup

def build_memdusa() -> Path:
    sh(["cargo", "build", "--release", "-q", "-p", "memdusa"], cwd=ROOT)
    return ROOT / "target" / "release" / "memdusa"


def prepare(scenario_dir: Path, trial: Path) -> None:
    repo = trial / "repo"
    shutil.copytree(scenario_dir / "repo", repo)
    for cmd in (["git", "init", "-q", "-b", "main"], ["git", "add", "-A"],
                ["git", "-c", "user.email=eval@memdusa", "-c", "user.name=eval", "commit", "-qm", "initial"],
                ["git", "tag", "initial"]):
        sh(cmd, cwd=repo)
    # agents run `python3 -m pytest`: give them a venv with pytest, first on PATH
    sh(["uv", "venv", "-q", str(trial / "venv")])
    sh(["uv", "pip", "install", "-q", "--python", str(trial / "venv"), "pytest"])


def agent_env(trial: Path, scenario: str, agent: str) -> dict:
    env = {k: v for k, v in os.environ.items() if not k.startswith("MEMDUSA_")}
    venv_bin = trial / "venv" / ("Scripts" if os.name == "nt" else "bin")
    env.update(
        PATH=f"{venv_bin}{os.pathsep}{env.get('PATH', '')}",
        MEMDUSA_HOME=str(trial / "home"),
        MEMDUSA_DATABASE_URL=f"sqlite:{(trial / 'home' / 'memdusa.db').as_posix()}",
        MEMDUSA_PROJECT=f"eval/{scenario}",
        MEMDUSA_AGENT=agent,
    )
    (trial / "home").mkdir(exist_ok=True)
    return env


def claude_config(trial: Path, memdusa: Path | None, env: dict, agent: str) -> tuple[Path, Path]:
    """Settings + MCP config for one agent. Without memdusa both are empty."""
    settings, mcp = trial / f"settings-{agent}.json", trial / f"mcp-{agent}.json"
    settings.write_text("{}\n")
    servers = {}
    if memdusa:
        sh([str(memdusa), "install", "claude-code", "--settings", str(settings), "--no-mcp", "--bin", str(memdusa)],
           env=env)
        servers["memdusa"] = {
            "command": str(memdusa),
            "args": ["mcp", "--agent", agent],
            "env": {k: env[k] for k in env if k.startswith("MEMDUSA_")},
        }
    mcp.write_text(json.dumps({"mcpServers": servers}, indent=2))
    return settings, mcp


# ---------------------------------------------------------------- running an agent

def run_claude(prompt: str, repo: Path, env: dict, settings: Path, mcp: Path, model: str,
               timeout: float | None, log: Path, memdusa: bool, stop_after_edits: int | None = None) -> dict:
    """Run one headless Claude Code session. With `stop_after_edits`, the
    session is interrupted right after its Nth file edit completes: a
    reproducible "mid-task" point, unlike a timer that depends on model speed."""
    allowed = ALLOWED_BASH + (["mcp__memdusa"] if memdusa else [])
    cmd = [
        "claude", "-p", prompt,
        "--output-format", "stream-json", "--verbose",
        "--model", model,
        "--permission-mode", "acceptEdits",
        "--allowedTools", *allowed,
        "--settings", str(settings),
        "--setting-sources", "project",  # never the user's own settings or hooks
        "--mcp-config", str(mcp), "--strict-mcp-config",
        "--no-session-persistence",
    ]
    start = time.monotonic()
    proc = subprocess.Popen(cmd, cwd=repo, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                            start_new_session=True)
    killed = False

    edit_ids: set[str] = set()
    edits_done = 0
    interrupt = threading.Event()

    def reader() -> None:
        nonlocal edits_done
        assert proc.stdout is not None
        with log.open("w") as f:
            for line in proc.stdout:
                f.write(line)
                f.flush()
                if stop_after_edits is None:
                    continue
                try:
                    ev = json.loads(line)
                except json.JSONDecodeError:
                    continue
                for block in ev.get("message", {}).get("content", []) if isinstance(ev.get("message"), dict) else []:
                    if block.get("type") == "tool_use" and block.get("name") in EDIT_TOOLS:
                        edit_ids.add(block.get("id"))
                    elif block.get("type") == "tool_result" and block.get("tool_use_id") in edit_ids:
                        edits_done += 1
                        if edits_done >= stop_after_edits:
                            interrupt.set()

    thread = threading.Thread(target=reader, daemon=True)
    thread.start()
    deadline = None if timeout is None else start + timeout
    while proc.poll() is None and not interrupt.is_set() and (deadline is None or time.monotonic() < deadline):
        time.sleep(0.1)
    if proc.poll() is None:
        killed = True
        os.killpg(proc.pid, signal.SIGTERM)  # like closing the terminal mid-task
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
    thread.join(timeout=10)
    stats = log_stats(log)
    stats.update(killed=killed, duration_s=round(time.monotonic() - start, 1), exit_code=proc.returncode)
    return stats


def log_stats(log: Path) -> dict:
    """Effort numbers from an agent's stream-json transcript."""
    stats = {"tool_calls": 0, "memdusa_calls": 0, "first_edit_call": None, "num_turns": None, "cost_usd": None,
             "result": None}
    for line in log.read_text(errors="replace").splitlines() if log.exists() else []:
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "assistant":
            for block in ev.get("message", {}).get("content", []):
                if block.get("type") != "tool_use":
                    continue
                stats["tool_calls"] += 1
                name = block.get("name", "")
                if name.startswith("mcp__memdusa"):
                    stats["memdusa_calls"] += 1
                if name in EDIT_TOOLS and stats["first_edit_call"] is None:
                    stats["first_edit_call"] = stats["tool_calls"]
        elif ev.get("type") == "result":
            stats.update(num_turns=ev.get("num_turns"), cost_usd=ev.get("total_cost_usd"), result=ev.get("subtype"))
    return stats


# ---------------------------------------------------------------- grading

def hidden_tests(trial: Path, repo: Path, scenario_dir: Path, name: str) -> dict:
    junit = trial / f"junit-{name}.xml"
    test = trial / f"hidden-{name}.py"
    shutil.copy(scenario_dir / "hidden_test.py", test)
    python = trial / "venv" / ("Scripts" if os.name == "nt" else "bin") / "python"
    subprocess.run([str(python), "-m", "pytest", "-q", "--tb=no", "-p", "no:cacheprovider", f"--junitxml={junit}",
                    str(test)], cwd=repo, env={**os.environ, "PYTHONPATH": str(repo)}, capture_output=True, text=True)
    if not junit.exists():
        return {"passed": 0, "total": 0}
    suite = ET.parse(junit).getroot()
    suite = suite if suite.tag == "testsuite" else suite[0]
    total = int(suite.get("tests", 0))
    bad = sum(int(suite.get(k, 0)) for k in ("failures", "errors", "skipped"))
    return {"passed": total - bad, "total": total}


def changed_files(repo: Path, a: str, b: str) -> list[str]:
    return [l for l in sh(["git", "diff", "--name-only", a, b], cwd=repo).splitlines() if l]


def show(repo: Path, rev: str, path: str) -> str | None:
    """A file's text at `rev`; None if missing or binary."""
    r = subprocess.run(["git", "show", f"{rev}:{path}"], cwd=repo, capture_output=True)
    if r.returncode != 0 or b"\0" in r.stdout:
        return None
    return r.stdout.decode(errors="replace")


def grade(trial: Path, repo: Path, scenario_dir: Path, scenario: dict, name: str) -> dict:
    sh(["git", "add", "-A"], cwd=repo)
    if sh(["git", "status", "--porcelain"], cwd=repo).strip():
        sh(["git", "-c", "user.email=eval@memdusa", "-c", "user.name=eval", "commit", "-qm", f"B-{name}"], cwd=repo)
    out = {"hidden_tests": hidden_tests(trial, repo, scenario_dir, name), "violations": []}
    for rule in scenario["checks"].get("forbidden_patterns", []):
        text = (repo / rule["path"]).read_text() if (repo / rule["path"]).exists() else ""
        code = "\n".join(l for l in text.splitlines() if not l.lstrip().startswith("#"))
        if re.search(rule["regex"], code):
            out["violations"].append(f"{rule['path']}: {rule['why']}")
    for path in scenario["checks"].get("unchanged_files", []):
        if show(repo, "HEAD", path) != (scenario_dir / "repo" / path).read_text():
            out["violations"].append(f"{path} was changed")
    # Rework: how much of what A wrote did B throw away? 0 = kept all of it, 1 = rewrote everything.
    a_lines = rewritten = 0
    for path in changed_files(repo, "initial", "A-state"):
        a_text, b_text = show(repo, "A-state", path), show(repo, "HEAD", path)
        if a_text is None:
            continue
        a_list = a_text.splitlines()
        kept = sum(block.size for block in
                   difflib.SequenceMatcher(None, a_list, (b_text or "").splitlines()).get_matching_blocks())
        a_lines += len(a_list)
        rewritten += len(a_list) - kept
    out["rework"] = round(rewritten / a_lines, 2) if a_lines else None
    return out


# ---------------------------------------------------------------- one trial

def trial_run(i: int, args, scenario_dir: Path, scenario: dict, memdusa: Path, out: Path) -> dict:
    trial = out / f"trial-{i}"
    trial.mkdir(parents=True)
    prepare(scenario_dir, trial)
    name = scenario["name"]

    # --- A: starts the task, gets interrupted
    env_a = agent_env(trial, name, "agent-a")
    settings, mcp = claude_config(trial, memdusa, env_a, "agent-a")
    a = run_claude(scenario["task_prompt"], trial / "repo", env_a, settings, mcp, args.model, args.interrupt_after,
                   trial / "agent-a.jsonl", memdusa=True, stop_after_edits=args.interrupt_after_edits)
    repo = trial / "repo"
    sh(["git", "add", "-A"], cwd=repo)
    sh(["git", "-c", "user.email=eval@memdusa", "-c", "user.name=eval", "commit", "-qm", "A-state", "--allow-empty"],
       cwd=repo)
    sh(["git", "tag", "A-state"], cwd=repo)
    a["files_changed"] = changed_files(repo, "initial", "A-state")
    a["hidden_tests"] = hidden_tests(trial, repo, scenario_dir, "after-a")
    (trial / "brief-after-a.md").write_text(sh([str(memdusa), "resume"], cwd=repo, env=env_a, check=False))

    # --- B: continues from the same snapshot, once per condition, in parallel
    def run_b(cond: str) -> tuple[str, dict]:
        b_dir = trial / f"b-{cond}"
        shutil.copytree(repo, b_dir / "repo")
        shutil.copytree(trial / "home", b_dir / "home")
        os.symlink(trial / "venv", b_dir / "venv")
        env_b = agent_env(b_dir, name, "agent-b")
        use = memdusa if cond == "memdusa" else None
        s, m = claude_config(b_dir, use, env_b, "agent-b")
        b = run_claude(scenario["resume_prompt"], b_dir / "repo", env_b, s, m, args.model, args.b_timeout,
                       b_dir / "agent-b.jsonl", memdusa=use is not None)
        b.update(grade(b_dir, b_dir / "repo", scenario_dir, scenario, cond))
        return cond, b

    with concurrent.futures.ThreadPoolExecutor() as pool:
        results = dict(pool.map(run_b, args.conditions))
    return {"trial": i, "a": a, "b": results}


# ---------------------------------------------------------------- report

def summarize(trials: list[dict], conditions: list[str]) -> str:
    rows = []
    for cond in conditions:
        bs = [t["b"][cond] for t in trials]
        def mean(key, f=lambda v: v):
            vals = [f(b[key]) for b in bs if b.get(key) is not None]
            return statistics.mean(vals) if vals else None
        tests = [b["hidden_tests"]["passed"] / max(b["hidden_tests"]["total"], 1) for b in bs]
        fmt = lambda v, p="{:.2f}": "–" if v is None else p.format(v)
        rows.append(
            f"| {cond} | {statistics.mean(tests):.0%} | {sum(1 for b in bs if b['violations'])}/{len(bs)} "
            f"| {fmt(mean('rework'))} | {fmt(mean('num_turns'), '{:.1f}')} | {fmt(mean('first_edit_call'), '{:.1f}')} "
            f"| {fmt(mean('cost_usd'), '${:.2f}')} | {fmt(mean('memdusa_calls'), '{:.1f}')} |"
        )
    a_tests = statistics.mean(t["a"]["hidden_tests"]["passed"] / max(t["a"]["hidden_tests"]["total"], 1) for t in trials)
    return "\n".join([
        f"Agent A (interrupted) left hidden tests at {a_tests:.0%} on average.\n",
        "| B condition | hidden tests | runs with constraint violations | rework | turns | tool calls before first edit | cost | memdusa calls |",
        "|---|---|---|---|---|---|---|---|",
        *rows,
    ])


def regrade(out: Path, scenario_dir: Path, scenario: dict, conditions: list[str]) -> int:
    out = out.resolve()
    trials = []
    for trial in sorted(out.glob("trial-*")):
        repo = trial / "repo"
        a = log_stats(trial / "agent-a.jsonl")
        a.update(killed=a["result"] is None, files_changed=changed_files(repo, "initial", "A-state"),
                 hidden_tests=hidden_tests(trial, repo, scenario_dir, "after-a"))
        b = {}
        for cond in conditions:
            b_dir = trial / f"b-{cond}"
            b[cond] = {**log_stats(b_dir / "agent-b.jsonl"), **grade(b_dir, b_dir / "repo", scenario_dir, scenario, cond)}
        trials.append({"trial": int(trial.name.split("-")[1]), "a": a, "b": b})
    (out / "results.json").write_text(json.dumps({"scenario": scenario["name"], "regraded": True, "trials": trials},
                                                 indent=2))
    report = summarize(trials, conditions)
    (out / "report.md").write_text(f"# Handoff eval: {scenario['name']} (regraded)\n\n{report}\n")
    print(report)
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--scenario", default="refunds")
    p.add_argument("--trials", type=int, default=1)
    p.add_argument("--interrupt-after-edits", type=int, default=1,
                   help="interrupt agent A right after its Nth completed file edit (0 = only use the timer)")
    p.add_argument("--interrupt-after", type=float, default=300,
                   help="safety timer for agent A, in seconds")
    p.add_argument("--b-timeout", type=float, default=600, help="safety limit for agent B, in seconds")
    p.add_argument("--model", default="sonnet")
    p.add_argument("--conditions", default="memdusa,baseline", type=lambda s: s.split(","))
    p.add_argument("--out", type=Path, default=None)
    p.add_argument("--regrade", type=Path, metavar="RESULTS_DIR",
                   help="recompute grades from an earlier run's saved repos and transcripts (no agents run)")
    args = p.parse_args()
    args.interrupt_after_edits = args.interrupt_after_edits or None

    scenario_dir = HERE / "scenarios" / args.scenario
    scenario = json.loads((scenario_dir / "scenario.json").read_text())
    if args.regrade:
        return regrade(args.regrade, scenario_dir, scenario, args.conditions)
    out = args.out or ROOT / "eval" / "results" / f"{dt.datetime.now():%Y%m%d-%H%M%S}-{args.scenario}"
    out = out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    print("building memdusa…", file=sys.stderr)
    memdusa = build_memdusa()

    trials = []
    for i in range(1, args.trials + 1):
        print(f"trial {i}/{args.trials}: agent A (stopped after {args.interrupt_after_edits} edit(s)) then B × "
              f"{args.conditions}", file=sys.stderr)
        trials.append(trial_run(i, args, scenario_dir, scenario, memdusa, out))
        (out / "results.json").write_text(json.dumps(
            {"scenario": args.scenario, "model": args.model, "interrupt_after_edits": args.interrupt_after_edits,
             "trials": trials}, indent=2))
    report = summarize(trials, args.conditions)
    (out / "report.md").write_text(f"# Handoff eval: {args.scenario}\n\nmodel `{args.model}`, {args.trials} trial(s), "
                                   f"A interrupted after {args.interrupt_after_edits} edit(s)\n\n{report}\n")
    print(report)
    print(f"\nfull results: {out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
