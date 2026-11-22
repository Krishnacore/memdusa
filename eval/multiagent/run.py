#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# ///
"""Repeatable multi-agent eval: delegate -> crash -> cross-harness takeover.

One trial:
  1. The orchestrator (this script, not an agent) creates a durable run with the
     scenario's steps via `memdusa delegate`.
  2. Worker 1 (default: OpenCode) gets its full assignment as the prompt and is
     killed as soon as its first `file_change` event reaches the database.
  3. Worker 2 (default: Codex) gets a different assignment and runs to
     completion, calling `task_complete` itself.
  4. Once worker 1's step lease has gone stale, a replacement (default: Claude
     Code) is launched with ONLY "Continue your delegated task." Its brief is
     captured just before launch.
  5. Grading is deterministic: hidden tests per task, both steps `done`, the
     brief carrying the full assignment, how much of worker 1's file the
     replacement kept, "Who did what" attribution and reported cost/turns.

Harness configs are installed at `--scope project` into a per-trial scratch
repo (never the user's global harness config). The database URL is baked into
those installs with `memdusa install --database-url`; worker environments are
stripped of every MEMDUSA_* variable except the four coordination variables.

Usage:
  uv run eval/multiagent/run.py --dry-run                    # free, stub harnesses
  uv run eval/multiagent/run.py --self-test                  # grading unit checks
  uv run eval/multiagent/run.py                              # one real trial
  uv run eval/multiagent/run.py --trials 3 --lease-secs 45
  uv run eval/multiagent/run.py --harnesses worker1=opencode,worker2=codex,replacement=claude
"""

from __future__ import annotations

import argparse
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
SCENARIOS = HERE / "scenarios"
RESULTS = ROOT / "eval" / "results"
PROJECT = "github.com/memdusa-eval/shop-team"

# The only MEMDUSA_* variables a worker's environment is allowed to keep.
COORD_VARS = ("MEMDUSA_RUN", "MEMDUSA_TASK", "MEMDUSA_PARENT", "MEMDUSA_STEP_LEASE_SECS")
# Memdusa's `source` on journal events for each harness (used by "Who did what").
SOURCE = {"claude": "claude-code", "codex": "codex", "opencode": "opencode"}
# `memdusa install` uses the full harness id, while the runners use short names.
INSTALL_NAME = {"claude": "claude-code", "codex": "codex", "opencode": "opencode"}
REAL_BIN = {
    "claude": "claude",
    "codex": "/opt/homebrew/bin/codex",
    "opencode": str(Path.home() / ".opencode" / "bin" / "opencode"),
}


# ---------------------------------------------------------------- helpers

def sh(cmd, cwd: Path | None = None, env: dict | None = None, check: bool = True,
       stdin: int | None = None) -> str:
    r = subprocess.run([str(c) for c in cmd], cwd=cwd, env=env, stdin=stdin,
                       capture_output=True, text=True, errors="replace")
    if check and r.returncode != 0:
        raise RuntimeError(f"{' '.join(map(str, cmd))} failed ({r.returncode}):\n{r.stdout}\n{r.stderr}")
    return r.stdout


def build_memdusa() -> Path:
    sh(["cargo", "build", "--release", "-q", "-p", "memdusa"], cwd=ROOT)
    return ROOT / "target" / "release" / "memdusa"


def base_environment(trial: Path) -> dict:
    """Strip every MEMDUSA_* var; put the trial's pytest venv first on PATH."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("MEMDUSA_")}
    venv_bin = trial / "venv" / ("Scripts" if os.name == "nt" else "bin")
    env["PATH"] = f"{venv_bin}{os.pathsep}{env.get('PATH', '')}"
    # OpenCode resolves its project root from $PWD, not from the process cwd. Left
    # untouched, a worker launched from this repo would load no project plugin and
    # file every hook event under the outer project. Pin both to the scratch repo.
    env["PWD"] = str(trial / "repo")
    # Only for isolation of hook logs; the database URL stays baked into the
    # project-scope installs (and is deliberately not a worker coordination var).
    env["MEMDUSA_HOME"] = str(trial / "home")
    return env


def worker_env(base: dict, run_id: str, task: str, lease_secs: int) -> dict:
    env = dict(base)
    env.update(
        MEMDUSA_RUN=run_id,
        MEMDUSA_TASK=task,
        MEMDUSA_PARENT="coordinator",
        MEMDUSA_STEP_LEASE_SECS=str(lease_secs),
    )
    return env


def memcli(memdusa: Path, db_url: str, *args: str) -> list[str]:
    return [str(memdusa), "--database-url", db_url, *args]


def prepare(scenario_dir: Path, trial: Path) -> None:
    repo = trial / "repo"
    shutil.copytree(scenario_dir / "repo", repo)
    for cmd in (
        ["git", "init", "-q", "-b", "main"],
        ["git", "remote", "add", "origin", f"https://{PROJECT}.git"],
        ["git", "add", "-A"],
        ["git", "-c", "user.email=eval@memdusa", "-c", "user.name=eval", "commit", "-qm", "initial"],
        ["git", "tag", "initial"],
    ):
        sh(cmd, cwd=repo)
    sh(["uv", "venv", "-q", str(trial / "venv")])
    sh(["uv", "pip", "install", "-q", "--python", str(trial / "venv"), "pytest"])
    for name in ("home", "xdg"):
        (trial / name).mkdir(exist_ok=True)


def install_harnesses(memdusa: Path, db_url: str, repo: Path, harnesses: dict,
                      base: dict, trial: Path) -> None:
    """Project-scope installs only: every config file lands inside the scratch repo."""
    for harness in sorted(set(harnesses.values())):
        env = dict(base)
        if harness == "opencode":
            # `opencode mcp add` must not write the user's global config.
            env["XDG_CONFIG_HOME"] = str(trial / "xdg")
        sh(memcli(memdusa, db_url, "install", INSTALL_NAME[harness], "--scope", "project"), cwd=repo, env=env)


def parse_delegate(output: str) -> tuple[str, list[str]]:
    pairs = re.findall(r"MEMDUSA_RUN=(\S+)\s+MEMDUSA_TASK=(\S+)", output)
    if not pairs:
        raise RuntimeError(f"could not parse delegate output:\n{output}")
    return pairs[0][0], [task for _, task in pairs]


# ---------------------------------------------------------------- database reads

def export_events(memdusa: Path, db_url: str) -> list[dict]:
    text = sh(memcli(memdusa, db_url, "export", "--only-project", PROJECT), check=False)
    out = []
    for line in text.splitlines():
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("type") == "event":
            out.append(rec)
    return out


def export_runs(memdusa: Path, db_url: str) -> list[dict]:
    text = sh(memcli(memdusa, db_url, "export", "--only-project", PROJECT), check=False)
    out = []
    for line in text.splitlines():
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("type") == "run":
            out.append(rec)
    return out


def step_of(runs: list[dict], run_id: str, task: str) -> dict | None:
    for run in runs:
        if run.get("id") == run_id:
            for step in run.get("steps", []):
                if step.get("key") == task:
                    return step
    return None


def steps_done(runs: list[dict], run_id: str, tasks: list[str]) -> dict[str, bool]:
    return {t: (step_of(runs, run_id, t) or {}).get("status") == "completed" for t in tasks}


def is_stale(step: dict | None, now_ms: int | None = None) -> bool:
    if not step:
        return False
    if step.get("stale"):
        return True
    lease = step.get("lease_until")
    return bool(step.get("status") == "running" and lease and lease < (now_ms or int(time.time() * 1000)))


def wait_for_stale(memdusa: Path, db_url: str, run_id: str, task: str,
                   timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if is_stale(step_of(export_runs(memdusa, db_url), run_id, task)):
            return True
        time.sleep(0.4)
    return False


def wait_for_file_change(memdusa: Path, db_url: str, source: str, seen: set[str],
                         proc: subprocess.Popen, timeout: float) -> dict | None:
    """Poll `memdusa export` until a NEW file_change event from `source` appears."""
    deadline = time.monotonic() + timeout
    while proc.poll() is None and time.monotonic() < deadline:
        for ev in export_events(memdusa, db_url):
            if ev.get("kind") != "file_change" or ev.get("source") != source:
                continue
            if ev.get("id") in seen:
                continue
            seen.add(ev["id"])
            return ev
        time.sleep(0.25)
    return None


# ---------------------------------------------------------------- process helpers

def kill_group(proc: subprocess.Popen) -> None:
    try:
        os.killpg(proc.pid, signal.SIGTERM)
        proc.wait(timeout=8)
    except (ProcessLookupError, subprocess.TimeoutExpired):
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


def read_log(proc: subprocess.Popen, log: Path) -> None:
    assert proc.stdout is not None
    with log.open("w") as f:
        for line in proc.stdout:
            f.write(line)
            f.flush()


# ---------------------------------------------------------------- harness runners

def run_opencode(binary: str, prompt: str, repo: Path, env: dict, log: Path, timeout: float, *,
                 memdusa: Path | None = None, db_url: str | None = None, seen: set[str] | None = None,
                 kill_on_source: str = "opencode") -> dict:
    """OpenCode worker: crash the process group on its first file_change event."""
    cmd = [binary, "run", "--standalone", "--auto", "--model", "opencode/hy4-flash-free", prompt]
    start = time.monotonic()
    proc = subprocess.Popen(cmd, cwd=repo, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, start_new_session=True)
    thread = threading.Thread(target=read_log, args=(proc, log), daemon=True)
    thread.start()
    first_change = None
    if kill_on_source and memdusa and db_url:
        ev = wait_for_file_change(memdusa, db_url, kill_on_source, seen or set(), proc, timeout)
        first_change = ev.get("content") if ev else None
    else:
        deadline = start + timeout
        while proc.poll() is None and time.monotonic() < deadline:
            time.sleep(0.2)
    killed = False
    if proc.poll() is None:
        killed = True
        kill_group(proc)
    thread.join(timeout=8)
    return {
        "killed": killed,
        "first_file_change": first_change,
        "duration_s": round(time.monotonic() - start, 1),
        "exit_code": proc.returncode,
        "command": cmd[:-1] + ["<assignment>"],
    }


def run_codex(binary: str, prompt: str, repo: Path, env: dict, log: Path, timeout: float, *,
              memdusa: Path | None = None, db_url: str | None = None, seen: set[str] | None = None,
              kill_on_source: str | None = None) -> dict:
    """Codex worker: runs to completion and is expected to call task_complete itself."""
    cmd = [binary, "exec", "-C", str(repo), "--trust", "--dangerously-bypass-hook-trust",
           "--sandbox", "workspace-write", "--json", prompt]
    start = time.monotonic()
    proc = subprocess.Popen(cmd, cwd=repo, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, start_new_session=True)
    thread = threading.Thread(target=read_log, args=(proc, log), daemon=True)
    thread.start()
    killed = False
    first_change = None
    if kill_on_source and memdusa and db_url:
        ev = wait_for_file_change(memdusa, db_url, kill_on_source, seen or set(), proc, timeout)
        first_change = ev.get("content") if ev else None
    else:
        deadline = start + timeout
        while proc.poll() is None and time.monotonic() < deadline:
            time.sleep(0.2)
    if proc.poll() is None:
        killed = True
        kill_group(proc)
    thread.join(timeout=8)
    stats = {"killed": killed, "first_file_change": first_change,
             "duration_s": round(time.monotonic() - start, 1),
             "exit_code": proc.returncode, "command": cmd[:-1] + ["<assignment>"]}
    stats.update(codex_stats(log))
    return stats


def codex_stats(log: Path) -> dict:
    stats = {"tokens": None, "num_turns": None, "cost_usd": None}
    for line in log.read_text(errors="replace").splitlines() if log.exists() else []:
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "turn.completed":
            usage = ev.get("usage", {})
            stats["tokens"] = usage.get("input_tokens", 0) + usage.get("cached_input_tokens", 0) + usage.get(
                "output_tokens", 0)
            stats["num_turns"] = (stats["num_turns"] or 0) + 1
    return stats


def run_claude(binary: str, prompt: str, repo: Path, env: dict, log: Path, timeout: float, *,
               memdusa: Path | None = None, db_url: str | None = None, seen: set[str] | None = None,
               kill_on_source: str | None = None) -> dict:
    """Replacement: only 'Continue your delegated task.'; brief comes from memdusa."""
    cmd = [binary, "-p", prompt,
           "--output-format", "stream-json", "--verbose", "--model", "haiku",
           "--setting-sources", "project,local", "--mcp-config", ".mcp.json", "--strict-mcp-config",
           "--permission-mode", "acceptEdits",
           "--allowedTools", "Bash(python3:*)", "mcp__memdusa__*", "Read", "Edit", "Write"]
    start = time.monotonic()
    proc = subprocess.Popen(cmd, cwd=repo, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, start_new_session=True)
    thread = threading.Thread(target=read_log, args=(proc, log), daemon=True)
    thread.start()
    killed = False
    first_change = None
    if kill_on_source and memdusa and db_url:
        ev = wait_for_file_change(memdusa, db_url, kill_on_source, seen or set(), proc, timeout)
        first_change = ev.get("content") if ev else None
    else:
        deadline = start + timeout
        while proc.poll() is None and time.monotonic() < deadline:
            time.sleep(0.2)
    if proc.poll() is None:
        killed = True
        kill_group(proc)
    thread.join(timeout=8)
    stats = {"killed": killed, "first_file_change": first_change,
             "duration_s": round(time.monotonic() - start, 1),
             "exit_code": proc.returncode, "command": cmd}
    stats.update(claude_stats(log))
    return stats


def claude_stats(log: Path) -> dict:
    stats = {"num_turns": None, "cost_usd": None, "tool_calls": 0, "memdusa_calls": 0, "result": None}
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
                if block.get("name", "").startswith("mcp__memdusa"):
                    stats["memdusa_calls"] += 1
        elif ev.get("type") == "result":
            stats.update(num_turns=ev.get("num_turns"), cost_usd=ev.get("total_cost_usd"),
                         result=ev.get("subtype"))
    return stats


# ---------------------------------------------------------------- stub harnesses

STUB_REFUNDS_PARTIAL = '''"""Refunds for the shop. Amounts are integer cents."""
from shop import ledger, orders


class RefundError(ValueError):
    """Raised when a refund is invalid."""


def refund(order_id, amount_cents, reason):
    if not isinstance(amount_cents, int) or amount_cents <= 0:
        raise RefundError("amount must be a positive integer of cents")
    if not isinstance(reason, str) or not reason.strip():
        raise RefundError("a non-empty reason is required")
    try:
        total = orders.get_order(order_id)
    except KeyError:
        raise RefundError("unknown order") from None
    remaining = total - sum(e.amount_cents for e in ledger.entries(order_id) if e.kind == "refund")
    if amount_cents > remaining:
        raise RefundError("refund exceeds the remaining refundable amount")
    return ledger.record("refund", order_id, amount_cents, reason.strip().lower())
'''

STUB_REFUNDS_APPEND = '''


def refunded_cents(order_id):
    return sum(e.amount_cents for e in ledger.entries(order_id) if e.kind == "refund")


def refundable_cents(order_id):
    return orders.get_order(order_id) - refunded_cents(order_id)
'''

STUB_REFUNDS_TESTS = '''from shop import ledger, orders
from shop.orders import ORDERS
from shop.refunds import RefundError, refund, refunded_cents, refundable_cents


def setup_function():
    ledger.reset()
    ORDERS.clear()
    ORDERS.update({1: 5000, 2: 1250})


def test_refund_lowercases_reason():
    e = refund(1, 100, "Damaged BOX")
    assert e.note == "damaged box"
    assert refunded_cents(1) == 100 and refundable_cents(1) == 4900


def test_over_refund():
    refund(2, 1250, "all")
    try:
        refund(2, 1, "one too many")
    except RefundError:
        pass
    else:
        raise AssertionError("over-refund did not raise")
'''

STUB_INVOICES = '''"""Invoices for the shop."""


def render_invoice(order_id):
    from shop.orders import get_order
    return f"INVOICE #{order_id} TOTAL {get_order(order_id)} cents"


def invoice_lines(order_id):
    from shop.orders import get_order
    return [f"INVOICE #{order_id}", f"TOTAL {get_order(order_id)} cents"]
'''

STUB_INVOICES_TESTS = '''from shop import invoices
from shop.orders import ORDERS


def setup_function():
    ORDERS.clear()
    ORDERS.update({1: 5000, 2: 1250})


def test_invoice_format():
    assert invoices.render_invoice(1) == "INVOICE #1 TOTAL 5000 cents"
    assert invoices.invoice_lines(2) == ["INVOICE #2", "TOTAL 1250 cents"]
'''


def make_stub_scripts(trial: Path, memdusa: Path, db_url: str, scenario: dict,
                      task_by_key: dict, crash_task: str) -> dict[str, str]:
    """Generate fake harness executables: real hooks/DB, no model calls, no cost."""
    prelude = '''#!/usr/bin/env python3
import json, os, pathlib, subprocess, time
MEMDUSA = {memdusa}
DB = {db_url}
PROJECT = {project}
REPO = pathlib.Path({repo})
COORD = {{"MEMDUSA_RUN", "MEMDUSA_TASK", "MEMDUSA_PARENT", "MEMDUSA_STEP_LEASE_SECS"}}
HE = {{k: v for k, v in os.environ.items() if not k.startswith("MEMDUSA_") or k in COORD}}
TASK = os.environ["MEMDUSA_TASK"]
SID = "stub-" + TASK


def hook(event, payload, agent):
    subprocess.run([MEMDUSA, "--database-url", DB, "--project", PROJECT, "hook", event,
                    "--agent", agent], input=json.dumps(payload), text=True, capture_output=True, env=HE)


def start(prompt, source="startup"):
    hook("session-start", {{"session_id": SID, "cwd": str(REPO), "source": source}}, AGENT)
    hook("user-prompt-submit", {{"session_id": SID, "prompt": prompt}}, AGENT)


def changed(path):
    hook("post-tool-use", {{"session_id": SID, "tool_name": "Write",
                           "tool_input": {{"file_path": str(REPO / path)}}}}, AGENT)


def done(summary):
    subprocess.run([MEMDUSA, "--database-url", DB, "--project", PROJECT, "task", "done",
                    "--summary", summary], text=True, capture_output=True, env=HE, check=True)


def write(rel, text):
    p = REPO / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text)
    changed(rel)
'''.format(memdusa=json.dumps(str(memdusa)), db_url=json.dumps(db_url),
           project=json.dumps(PROJECT), repo=json.dumps(str(trial / "repo")))

    bodies = {
        "opencode": f'''
AGENT = "opencode"
assignment = {json.dumps(task_by_key[crash_task]["assignment"])}
start(assignment)
write("shop/refunds.py", {json.dumps(STUB_REFUNDS_PARTIAL)})
print("stub opencode: wrote shop/refunds.py; waiting to be killed", flush=True)
if os.environ.get("EVAL_STUB_SLEEP") == "1":
    time.sleep(300)
''',
        "codex": f'''
AGENT = "codex"
assignment = {json.dumps(next(t["assignment"] for t in scenario["tasks"] if t["key"] != crash_task))}
start(assignment)
write("shop/invoices.py", {json.dumps(STUB_INVOICES)})
write("tests/test_invoices.py", {json.dumps(STUB_INVOICES_TESTS)})
done("invoices implemented by stub codex")
print(json.dumps({{"type": "turn.completed", "usage": {{"input_tokens": 100, "output_tokens": 50}}}}))
''',
        "claude": f'''
AGENT = "claude-code"
start({json.dumps(scenario["replacement_prompt"])})
existing = (REPO / "shop/refunds.py").read_text()
write("shop/refunds.py", existing + {json.dumps(STUB_REFUNDS_APPEND)})
write("tests/test_refunds.py", {json.dumps(STUB_REFUNDS_TESTS)})
done("refunds finished by replacement from brief")
print(json.dumps({{"type": "result", "subtype": "success", "num_turns": 2, "total_cost_usd": 0.02}}))
''',
    }
    paths = {}
    for harness, body in bodies.items():
        path = trial / f"stub-{harness}.py"
        path.write_text(prelude + body)
        path.chmod(0o755)
        paths[harness] = str(path)
    return paths


# ---------------------------------------------------------------- grading

def hidden_tests(trial: Path, repo: Path, scenario_dir: Path, name: str, test_file: str) -> dict:
    junit = trial / f"junit-{name}.xml"
    test = trial / f"hidden-{name}.py"
    shutil.copy(scenario_dir / test_file, test)
    python = trial / "venv" / ("Scripts" if os.name == "nt" else "bin") / "python"
    subprocess.run([str(python), "-m", "pytest", "-q", "--tb=no", "-p", "no:cacheprovider",
                    f"--junitxml={junit}", str(test)], cwd=repo,
                   env={**os.environ, "PYTHONPATH": str(repo)}, capture_output=True, text=True)
    if not junit.exists():
        return {"passed": 0, "total": 0}
    suite = ET.parse(junit).getroot()
    suite = suite if suite.tag == "testsuite" else suite[0]
    total = int(suite.get("tests", 0))
    bad = sum(int(suite.get(k, 0)) for k in ("failures", "errors", "skipped"))
    return {"passed": total - bad, "total": total}


def show(repo: Path, rev: str, path: str) -> str | None:
    r = subprocess.run(["git", "show", f"{rev}:{path}"], cwd=repo, capture_output=True)
    if r.returncode != 0 or b"\0" in r.stdout:
        return None
    return r.stdout.decode(errors="replace")


def changed_files(repo: Path, a: str, b: str) -> list[str]:
    return [l for l in sh(["git", "diff", "--name-only", a, b], cwd=repo).splitlines() if l]


def lines_kept(a_text: str | None, b_text: str | None) -> float | None:
    """Share of A's lines preserved in B (1 = built on A, 0 = started over)."""
    if a_text is None:
        return None
    a_lines = a_text.splitlines()
    if not a_lines:
        return None
    kept = sum(block.size for block in
               difflib.SequenceMatcher(None, a_lines, (b_text or "").splitlines()).get_matching_blocks())
    return round(kept / len(a_lines), 2)


def brief_check(brief: str, tail: str) -> dict:
    return {"has_assignment_section": "## Your assignment" in brief, "has_distinctive_tail": tail in brief}


def who_did_what(coordinator_brief: str, sources: list[str]) -> dict[str, bool]:
    match = re.search(r"\*\*Who did what:\*\*\n(.*?)(?:\n\n|\Z)", coordinator_brief, re.S)
    section = match.group(1) if match else ""
    named = set(re.findall(r"^- \*\*([^*]+)\*\*:", section, re.M))
    return {source: source in named for source in sources}


def grade(trial: Path, repo: Path, scenario_dir: Path, scenario: dict, task_by_key: dict,
          crash_task: str, runs: list[dict], run_id: str, stats: dict, harnesses: dict) -> dict:
    sh(["git", "add", "-A"], cwd=repo)
    if sh(["git", "status", "--porcelain"], cwd=repo).strip():
        sh(["git", "-c", "user.email=eval@memdusa", "-c", "user.name=eval",
            "commit", "-qm", "final"], cwd=repo)

    tasks = [t["key"] for t in scenario["tasks"]]
    result = {
        "hidden_tests": {t: hidden_tests(trial, repo, scenario_dir, t, task_by_key[t]["hidden_test"]) for t in tasks},
        "steps_done": steps_done(runs, run_id, tasks),
    }
    brief = (trial / "brief-before-replacement.md").read_text()
    result["brief"] = brief_check(brief, task_by_key[crash_task]["assignment_tail"])
    result["worker1_lines_kept"] = lines_kept(
        show(repo, "worker1-state", scenario["rework_path"]),
        show(repo, "HEAD", scenario["rework_path"]))
    coord = (trial / "coordinator-brief.md").read_text()
    sources = [SOURCE[h] for h in (harnesses["worker1"], harnesses["worker2"], harnesses["replacement"])]
    result["who_did_what"] = who_did_what(coord, sources)
    result["stats"] = stats
    result["all_steps_done"] = all(result["steps_done"].values())
    result["brief_carried_assignment"] = all(result["brief"].values())
    result["all_sources_attributed"] = all(result["who_did_what"].values())
    result["passed"] = (
        result["all_steps_done"]
        and result["brief_carried_assignment"]
        and result["all_sources_attributed"]
        and all(v["passed"] == v["total"] for v in result["hidden_tests"].values())
    )
    return result


# ---------------------------------------------------------------- one trial

def trial_run(i: int, args, scenario_dir: Path, scenario: dict, memdusa: Path, out: Path) -> dict:
    trial = out / "scratch" / f"trial-{i}"
    trial.mkdir(parents=True)
    prepare(scenario_dir, trial)
    repo = trial / "repo"
    db_url = f"sqlite:{(trial / 'home' / 'memdusa.db').as_posix()}"

    task_by_key = {t["key"]: t for t in scenario["tasks"]}
    harnesses = dict(scenario["harnesses"])
    harnesses.update(args.harnesses)
    crash_task = scenario["crash_task"]
    worker2_task = next(k for k in task_by_key if k != crash_task)
    base = base_environment(trial)

    # --- coordinator: one durable run with one step per task
    delegate_out = sh(memcli(memdusa, db_url, "--project", PROJECT, "delegate",
                             scenario["run_name"], *task_by_key), cwd=repo, env=base)
    run_id, _ = parse_delegate(delegate_out)
    (trial / "delegate.txt").write_text(delegate_out)
    install_harnesses(memdusa, db_url, repo, harnesses, base, trial)

    stubs = None
    if args.dry_run:
        stubs = make_stub_scripts(trial, memdusa, db_url, scenario, task_by_key, crash_task)

    def binary(harness: str) -> str:
        return stubs[harness] if stubs else REAL_BIN[harness]

    runners = {"opencode": run_opencode, "codex": run_codex, "claude": run_claude}

    # --- worker 1: full assignment, killed on its first file_change event
    seen = {ev["id"] for ev in export_events(memdusa, db_url)}
    env1 = worker_env(base, run_id, crash_task, args.lease_secs)
    # Same isolated config dir as the install, so project MCP registration is visible
    # and no global OpenCode config is read or written.
    env1["XDG_CONFIG_HOME"] = str(trial / "xdg")
    if stubs:
        env1["EVAL_STUB_SLEEP"] = "1"  # only the crashing worker's stub waits to be killed
    h1 = harnesses["worker1"]
    stats1 = runners[h1](binary(h1), task_by_key[crash_task]["assignment"], repo, env1,
                         trial / f"worker1-{h1}.log", args.worker_timeout,
                         memdusa=memdusa, db_url=db_url, seen=seen, kill_on_source=SOURCE[h1])
    sh(["git", "add", "-A"], cwd=repo)
    sh(["git", "-c", "user.email=eval@memdusa", "-c", "user.name=eval", "commit", "-qm", "worker1-state",
        "--allow-empty"], cwd=repo)
    sh(["git", "tag", "worker1-state"], cwd=repo)
    stats1["files_changed"] = changed_files(repo, "initial", "worker1-state")

    # --- worker 2: different harness, different task, runs to `task_complete`
    env2 = worker_env(base, run_id, worker2_task, args.lease_secs)
    h2 = harnesses["worker2"]
    stats2 = runners[h2](binary(h2), task_by_key[worker2_task]["assignment"], repo, env2,
                         trial / f"worker2-{h2}.log", args.worker_timeout)

    # --- wait for the crashed worker's lease to go stale, then capture the handoff
    stale = wait_for_stale(memdusa, db_url, run_id, crash_task, args.stale_timeout)
    (trial / "coordinator-brief-before-replacement.md").write_text(
        sh(memcli(memdusa, db_url, "--project", PROJECT, "resume"), cwd=repo, env=base))
    env3 = worker_env(base, run_id, crash_task, args.lease_secs)
    brief = sh(memcli(memdusa, db_url, "--project", PROJECT, "resume"), cwd=repo, env=env3)
    (trial / "brief-before-replacement.md").write_text(brief)

    # --- replacement: only "Continue your delegated task." (brief via memdusa)
    h3 = harnesses["replacement"]
    stats3 = runners[h3](binary(h3), scenario["replacement_prompt"], repo, env3,
                         trial / f"replacement-{h3}.log", args.worker_timeout)
    # "Who did what" needs all three sources, so read the coordinator brief again
    # after the replacement has journaled its events.
    (trial / "coordinator-brief.md").write_text(
        sh(memcli(memdusa, db_url, "--project", PROJECT, "resume"), cwd=repo, env=base))

    runs = export_runs(memdusa, db_url)
    (trial / "runs.json").write_text(json.dumps(runs, indent=2))
    result = grade(trial, repo, scenario_dir, scenario, task_by_key, crash_task, runs, run_id,
                   {"worker1": stats1, "worker2": stats2, "replacement": stats3}, harnesses)
    result.update(trial=i, run_id=run_id, lease_went_stale=stale, harnesses=dict(harnesses))

    # Keep the small artifacts, drop the heavy scratch in non---keep runs.
    if not args.keep:
        for name in ("repo", "venv", "home", "xdg"):
            shutil.rmtree(trial / name, ignore_errors=True)
        if stubs:
            for h in stubs:
                (trial / f"stub-{h}.py").unlink(missing_ok=True)
    return result


# ---------------------------------------------------------------- report / CLI

def summarize(trials: list[dict]) -> str:
    harnesses = trials[0].get("harnesses", {}) if trials else {}
    w2_label = harnesses.get("worker2", "codex")
    rep_label = harnesses.get("replacement", "claude")
    rows = []
    for t in trials:
        h = t["hidden_tests"]
        fmt = lambda d: f"{d['passed']}/{d['total']}"
        b = lambda key: "yes" if t[key] else "NO"
        kept = t["worker1_lines_kept"]
        s1, s2, s3 = t["stats"]["worker1"], t["stats"]["worker2"], t["stats"]["replacement"]
        cost = s3.get("cost_usd")
        rows.append(
            f"| {t['trial']} | {fmt(h['refunds'])} | {fmt(h['invoices'])} | {b('all_steps_done')} "
            f"| {b('brief_carried_assignment')} | {'–' if kept is None else f'{kept:.0%}'} "
            f"| {b('all_sources_attributed')} | {s1.get('duration_s', '–')}s "
            f"| {s2.get('num_turns') or '–'} | {s3.get('num_turns') or '–'} "
            f"| {'–' if cost is None else f'${cost:.2f}'} | {b('passed')} |"
        )
    return "\n".join([
        "Metric: hidden tests are never shown to the workers; `assignment` means the replacement's brief contained",
        "\"## Your assignment\" and the assignment's distinctive tail; `lines kept` is how much of worker 1's",
        "`shop/refunds.py` survived the takeover; `attributed` means \"Who did what\" named all three sources.\n",
        f"| trial | refunds tests | invoices tests | steps done | assignment | lines kept | attributed | worker1 (s) | {w2_label} turns | {rep_label} turns | {rep_label} cost | passed |",
        "|---|---|---|---|---|---|---|---|---|---|---|---|",
        *rows,
    ])


def self_test() -> int:
    checks = 0

    def ok(cond, label):
        nonlocal checks
        assert cond, label
        checks += 1

    out = ("Delegated run `abc` (release): 2 task(s).\n"
           "- MEMDUSA_RUN=abc MEMDUSA_TASK=refunds\n"
           "- MEMDUSA_RUN=abc MEMDUSA_TASK=invoices\n")
    run_id, tasks = parse_delegate(out)
    ok(run_id == "abc" and tasks == ["refunds", "invoices"], "parse_delegate")

    runs = [{"id": "abc", "steps": [
        {"key": "refunds", "status": "completed"}, {"key": "invoices", "status": "completed"}]}]
    ok(steps_done(runs, "abc", ["refunds", "invoices"]) == {"refunds": True, "invoices": True}, "steps_done")
    ok(is_stale({"status": "running", "lease_until": 5}, now_ms=10), "is_stale expired")
    ok(not is_stale({"status": "running", "lease_until": 50}, now_ms=10), "is_stale live")
    ok(lines_kept("a\nb\nc\n", "a\nb\nX\n") == 0.67, "lines_kept")
    ok(lines_kept(None, "x") is None, "lines_kept missing")
    tail = "Finally, add tests in `tests/test_refunds.py` covering the lowercased reason."
    brief = "## Your assignment\n_first given to opencode 1m ago_\n\n> " + tail + "\n"
    ok(brief_check(brief, tail) == {"has_assignment_section": True, "has_distinctive_tail": True}, "brief_check")
    ok(not brief_check(brief, "no such tail")["has_distinctive_tail"], "brief_check negative")
    coord = ("**Who did what:**\n- **opencode**: changed shop/refunds.py\n"
             "- **codex**: changed shop/invoices.py\n- **claude-code**: last active 1m ago\n\n"
             "**Files changed recently:** shop/refunds.py\n")
    w = who_did_what(coord, ["opencode", "codex", "claude-code"])
    ok(all(w.values()), "who_did_what")
    ok(not who_did_what(coord, ["cursor"])["cursor"], "who_did_what negative")
    print(f"self-test: {checks} checks passed")
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--scenario", default="shop-team")
    p.add_argument("--trials", type=int, default=1)
    p.add_argument("--harnesses", default="", help="override, e.g. worker1=opencode,worker2=codex,replacement=claude")
    p.add_argument("--keep", action="store_true", help="keep scratch repos/venvs")
    p.add_argument("--lease-secs", type=int, default=45)
    p.add_argument("--worker-timeout", type=float, default=600, help="safety limit per harness")
    p.add_argument("--stale-timeout", type=float, default=180, help="how long to wait for the stale lease")
    p.add_argument("--dry-run", action="store_true", help="stub harnesses: no model calls, no cost")
    p.add_argument("--self-test", action="store_true", help="unit checks for the grading functions")
    p.add_argument("--out", type=Path, default=None)
    args = p.parse_args()

    if args.self_test:
        return self_test()

    override = {}
    if args.harnesses:
        for pair in args.harnesses.split(","):
            k, v = pair.split("=", 1)
            override[k.strip()] = v.strip()
    args.harnesses = override

    scenario_dir = SCENARIOS / args.scenario
    scenario = json.loads((scenario_dir / "scenario.json").read_text())
    out = (args.out or RESULTS / f"{dt.datetime.now():%Y%m%d-%H%M%S}-multiagent").resolve()
    out.mkdir(parents=True, exist_ok=True)
    print("building memdusa…", file=sys.stderr)
    memdusa = build_memdusa()

    trials = []
    for i in range(1, args.trials + 1):
        print(f"trial {i}/{args.trials}: delegate -> crash -> takeover"
              f"{' (stub harnesses)' if args.dry_run else ''}", file=sys.stderr)
        trials.append(trial_run(i, args, scenario_dir, scenario, memdusa, out))
    report = summarize(trials)
    (out / "report.md").write_text(
        f"# Multi-agent eval: {scenario['name']}\n\n"
        f"{args.trials} trial(s), lease {args.lease_secs}s, "
        f"harnesses {scenario['harnesses'] | args.harnesses}"
        f"{' (stub mode)' if args.dry_run else ''}\n\n{report}\n")
    (out / "results.json").write_text(json.dumps(
        {"scenario": scenario["name"], "dry_run": args.dry_run, "lease_secs": args.lease_secs,
         "harnesses": scenario["harnesses"] | args.harnesses, "trials": trials}, indent=2))
    print(report)
    print(f"\nfull results: {out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
