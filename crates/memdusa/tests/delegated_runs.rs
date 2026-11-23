//! A coordinator delegates tasks as one durable run; workers claim their step with a lease. A crashed worker's
//! step goes stale and a replacement takes it over, finished steps are never redone, and two workers never
//! silently work on the same task.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

struct Env<'a> {
    dir: &'a Path,
    run: Option<&'a str>,
    task: Option<&'a str>,
    agent: &'a str,
    /// `MEMDUSA_STEP_LEASE_SECS` of this process.
    lease: Option<&'a str>,
}

fn memdusa(env: &Env, args: &[&str], stdin: Option<Value>) -> String {
    let db = memdusa_core::store::sqlite_url(&env.dir.join("t.db"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_memdusa"));
    cmd.args(["--database-url", &db, "--project", "demo"])
        .args(args)
        .env("MEMDUSA_HOME", env.dir)
        .env("HOME", env.dir)
        .env("MEMDUSA_AGENT", env.agent)
        .env_remove("MEMDUSA_URL")
        .env_remove("MEMDUSA_TOKEN")
        .env_remove("MEMDUSA_RUN")
        .env_remove("MEMDUSA_TASK")
        .env_remove("MEMDUSA_PARENT")
        .env_remove("MEMDUSA_STEP_LEASE_SECS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (var, value) in [("MEMDUSA_RUN", env.run), ("MEMDUSA_TASK", env.task), ("MEMDUSA_STEP_LEASE_SECS", env.lease)] {
        if let Some(v) = value {
            cmd.env(var, v);
        }
    }
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin.unwrap_or(json!({})).to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn session_start(env: &Env, session: &str) -> String {
    let payload = json!({ "session_id": session, "cwd": env.dir, "source": "startup" });
    let out = memdusa(env, &["hook", "session-start"], Some(payload));
    // the hook prints JSON; the brief is its additionalContext
    serde_json::from_str::<Value>(&out)
        .map_or(out, |v| v["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or_default().to_owned())
}

fn resume(env: &Env) -> String {
    memdusa(env, &["resume"], None)
}

#[test]
fn workers_claim_steps_stale_takeover_and_done_is_final() {
    let dir = std::env::temp_dir().join(format!("memdusa-runs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let lead = Env { dir: &dir, run: None, task: None, agent: "lead", lease: None };

    // the coordinator delegates two tasks and gets one env line per task
    let out = memdusa(&lead, &["delegate", "release", "install-scripts", "docs"], None);
    let lines: Vec<&str> = out.lines().filter(|l| l.starts_with("- MEMDUSA_RUN=")).collect();
    assert_eq!(lines.len(), 2, "{out}");
    let run = lines[0].trim_start_matches("- MEMDUSA_RUN=").split_whitespace().next().unwrap().to_owned();
    assert!(lines[0].ends_with("MEMDUSA_TASK=install-scripts"), "{out}");
    assert!(lines[1].ends_with("MEMDUSA_TASK=docs"), "{out}");
    let brief = resume(&lead);
    assert!(brief.contains("- **install-scripts**: pending") && brief.contains("- **docs**: pending"), "{brief}");

    // worker 1 claims install-scripts (short lease, so it can "crash" below)
    let w = |agent, task, lease| Env { dir: &dir, run: Some(&run), task: Some(task), agent, lease };
    let w1 = w("claude-code", "install-scripts", Some("2"));
    let start = session_start(&w1, "w1");
    assert!(start.contains("You claimed the delegated task `install-scripts`"), "{start}");

    // worker 2 on the same task is told it's held, and nothing changes
    let w2 = w("codex", "install-scripts", Some("600"));
    let start = session_start(&w2, "w2");
    assert!(start.contains("held by another live worker: claude-code:w1"), "{start}");
    assert!(!start.contains("You claimed"), "{start}");
    assert!(resume(&lead).contains("- **install-scripts**: running by claude-code:w1"));

    // worker 1 crashes: its lease expires and the task is visibly stale
    std::thread::sleep(Duration::from_millis(2600));
    let brief = resume(&lead);
    assert!(brief.contains("- **install-scripts**: stale (owner claude-code:w1, lease expired "), "{brief}");

    // a replacement in another harness takes it over and is told so
    let w3 = w("opencode", "install-scripts", Some("600"));
    let start = session_start(&w3, "w3");
    assert!(start.contains("You claimed the delegated task `install-scripts`"), "{start}");
    assert!(start.contains("previous attempt by claude-code:w1 went stale"), "{start}");
    assert!(resume(&lead).contains("- **install-scripts**: running by opencode:w3"));

    // its activity renews the lease; finishing is explicit
    let prompt = json!({ "session_id": "w3", "cwd": &dir, "prompt": "keep going" });
    memdusa(&w3, &["hook", "user-prompt-submit"], Some(prompt));
    memdusa(&w3, &["task", "done", "--summary", "installers built and signed"], None);

    // the coordinator sees A done (with its result) and B still pending
    let brief = resume(&lead);
    assert!(brief.contains("- **install-scripts**: done\n  - result: installers built and signed"), "{brief}");
    assert!(brief.contains("- **docs**: pending"), "{brief}");

    // a new worker started on the finished task is told not to redo it
    let w4 = w("claude-code", "install-scripts", Some("600"));
    let start = session_start(&w4, "w4");
    assert!(start.contains("`install-scripts` is already done"), "{start}");
    assert!(start.contains("installers built and signed") && start.contains("Do NOT redo it"), "{start}");

    // a worker can give a task up; a replacement then claims it and hears why
    let w5 = w("codex", "docs", Some("600"));
    assert!(session_start(&w5, "w5").contains("You claimed the delegated task `docs`"));
    memdusa(&w5, &["task", "fail", "no style guide"], None);
    assert!(resume(&lead).contains("- **docs**: failed: no style guide"));
    let w6 = w("opencode", "docs", Some("600"));
    let start = session_start(&w6, "w6");
    assert!(start.contains("An earlier attempt failed: no style guide"), "{start}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn task_commands_need_a_delegated_worker_env() {
    let dir = std::env::temp_dir().join(format!("memdusa-runs-noenv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = memdusa_core::store::sqlite_url(&dir.join("t.db"));
    let out = Command::new(env!("CARGO_BIN_EXE_memdusa"))
        .args(["--database-url", &db, "--project", "demo", "task", "done"])
        .env("MEMDUSA_HOME", &dir)
        .env("HOME", &dir)
        .env_remove("MEMDUSA_RUN")
        .env_remove("MEMDUSA_TASK")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("MEMDUSA_RUN is not set"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn delegate_stores_assignments_from_file_and_inline() {
    let dir = std::env::temp_dir().join(format!("memdusa-runs-assign-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let lead = Env { dir: &dir, run: None, task: None, agent: "lead", lease: None };

    let file = dir.join("tasks.json");
    std::fs::write(&file, json!([{"task": "refunds", "assignment": "Build refunds. TailFromFileXYZ."}]).to_string())
        .unwrap();
    let out = memdusa(&lead, &["delegate", "release", "--from-file", file.to_str().unwrap()], None);
    assert!(out.contains("1 assignment stored"), "{out}");
    assert!(!out.contains("TailFromFileXYZ"), "{out}: env lines stay clean");
    let run = out
        .lines()
        .find(|l| l.starts_with("- MEMDUSA_RUN="))
        .unwrap()
        .trim_start_matches("- MEMDUSA_RUN=")
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();

    // the worker's brief carries the stored text even with no prompt recorded
    let worker = Env { dir: &dir, run: Some(&run), task: Some("refunds"), agent: "opencode", lease: None };
    let brief = resume(&worker);
    assert!(brief.contains("## Your assignment"), "{brief}");
    assert!(brief.contains("_stored by the coordinator_"), "{brief}");
    assert!(brief.contains("TailFromFileXYZ."), "{brief}");
    // the coordinator sees the first line too
    assert!(resume(&lead).contains("  - assignment: Build refunds."), "{out}");

    // inline `task=assignment` form on a second run
    let out2 = memdusa(&lead, &["delegate", "docs run", "docs=Write the docs. TailInlineXYZ."], None);
    assert!(out2.contains("1 assignment stored"), "{out2}");
    let run2 = out2
        .lines()
        .find(|l| l.starts_with("- MEMDUSA_RUN="))
        .unwrap()
        .trim_start_matches("- MEMDUSA_RUN=")
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let worker2 = Env { dir: &dir, run: Some(&run2), task: Some("docs"), agent: "opencode", lease: None };
    assert!(resume(&worker2).contains("TailInlineXYZ."), "{}", resume(&worker2));
    assert!(!resume(&worker2).contains("TailFromFileXYZ"), "assignments are per task");

    let _ = std::fs::remove_dir_all(&dir);
}
