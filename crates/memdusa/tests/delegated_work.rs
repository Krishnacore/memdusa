//! A coordinator delegates tasks to workers via `MEMDUSA_TASK`: workers' sessions are recorded, their
//! checkpoints are task-scoped, a replacement worker resumes its task, and the coordinator sees the work.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

struct Env<'a> {
    dir: &'a Path,
    task: Option<&'a str>,
    parent: Option<&'a str>,
    agent: &'a str,
}

fn memdusa(env: &Env, args: &[&str], stdin: Option<Value>) -> String {
    // sqlite_url, not "sqlite://{path}": a Windows path's "C:" parses as host:port
    let db = memdusa_core::store::sqlite_url(&env.dir.join("t.db"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_memdusa"));
    cmd.args(["--database-url", &db, "--project", "demo"])
        .args(args)
        .env("MEMDUSA_HOME", env.dir)
        .env("HOME", env.dir)
        .env("MEMDUSA_AGENT", env.agent)
        .env_remove("MEMDUSA_URL")
        .env_remove("MEMDUSA_TOKEN")
        .env_remove("MEMDUSA_TASK")
        .env_remove("MEMDUSA_PARENT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(t) = env.task {
        cmd.env("MEMDUSA_TASK", t);
    }
    if let Some(p) = env.parent {
        cmd.env("MEMDUSA_PARENT", p);
    }
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin.unwrap_or(json!({})).to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn hook(env: &Env, event: &str, session: &str, extra: Value) -> String {
    let mut payload = json!({ "session_id": session, "cwd": env.dir });
    payload.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    memdusa(env, &["hook", event], Some(payload))
}

/// A worker session that edits a file, says something and ends (so an automatic checkpoint is written).
fn work_session(env: &Env, session: &str, prompt: &str, last_message: &str) -> String {
    let start = hook(env, "session-start", session, json!({ "source": "startup" }));
    hook(env, "user-prompt-submit", session, json!({ "prompt": prompt }));
    let edit = json!({ "tool_name": "Edit", "tool_input": { "file_path": env.dir.join("a.rs") } });
    hook(env, "post-tool-use", session, edit);
    hook(env, "stop", session, json!({ "last_assistant_message": last_message }));
    hook(env, "session-end", session, json!({ "reason": "other" }));
    start
}

fn resume_json(env: &Env) -> Value {
    serde_json::from_str(&memdusa(env, &["resume", "--json"], None)).unwrap()
}

#[test]
fn delegated_workers_replacement_and_coordinator_view() {
    let dir = std::env::temp_dir().join(format!("memdusa-delegated-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let worker = |task, agent| Env { dir: &dir, task: Some(task), parent: Some("lead"), agent };

    // worker A on `install-scripts`, worker B on `docs`; both end their sessions
    let a = worker("install-scripts", "claude-code");
    work_session(&a, "a1", "build the install scripts", "A: stopped at the windows installer");
    let b = worker("docs", "codex");
    work_session(&b, "b1", "write the docs", "B: docs are half done");

    // the sessions were recorded as delegated; auto checkpoints carry the env task
    let r = resume_json(&a);
    let delegated: Vec<&Value> =
        r["recent_events"].as_array().unwrap().iter().filter(|e| e["kind"] == "delegated_session").collect();
    assert_eq!(delegated.len(), 1, "a worker's activity is its own task's: {delegated:?}");
    let other = resume_json(&b);
    assert!(
        other["recent_events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "delegated_session" && e["session"] == "b1")
    );
    let first = delegated[0];
    assert_eq!(first["session"], "a1");
    assert_eq!(first["content"], "install-scripts");
    assert_eq!(
        first["data"],
        json!({ "task": "install-scripts", "parent": "lead", "harness": "claude-code", "source": "startup" })
    );
    let cps = r["checkpoints"].as_array().unwrap();
    assert_eq!(cps.len(), 1, "worker brief shows its task only: {cps:?}");
    assert_eq!(cps[0]["task"], "install-scripts");
    assert_eq!(cps[0]["goal"], "build the install scripts");

    // the replacement worker (new session, other harness) picks up exactly where A stopped
    let a2 = worker("install-scripts", "opencode");
    let start = hook(&a2, "session-start", "a2", json!({ "source": "startup" }));
    assert!(start.contains("A: stopped at the windows installer"), "{start}");
    assert!(start.contains("build the install scripts"), "{start}");
    assert!(!start.contains("B: docs are half done") && !start.contains("write the docs"), "{start}");

    // the coordinator sees both tasks, one still active (a2 started, not ended), one ended
    let lead = Env { dir: &dir, task: None, parent: None, agent: "lead" };
    let brief = hook(&lead, "session-start", "lead1", json!({ "source": "startup" }));
    assert!(brief.contains("## Delegated work"), "{brief}");
    assert!(brief.contains("**install-scripts** by opencode (for lead): active"), "{brief}");
    assert!(brief.contains("**docs** by codex (for lead): ended"), "{brief}");
    assert!(brief.contains("A: stopped at the windows installer"), "{brief}");
    assert!(!brief.contains("Last words"), "workers' messages are not the coordinator's: {brief}");
    assert!(!brief.contains("### Goal [docs]"), "workers' checkpoints are listed under Delegated work: {brief}");
    let md = resume_json(&lead)["markdown"].as_str().unwrap().to_owned();
    assert!(!md.contains("delegated_session"), "bookkeeping is hidden: {md}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_a_task_nothing_changes() {
    let dir = std::env::temp_dir().join(format!("memdusa-delegated-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env = Env { dir: &dir, task: None, parent: None, agent: "claude-code" };

    work_session(&env, "s1", "add refunds", "Endpoint done, tests next.");
    let r = resume_json(&env);
    assert!(r["recent_events"].as_array().unwrap().iter().all(|e| e["kind"] != "delegated_session"));
    let cp = &r["checkpoints"][0];
    assert!(cp["task"].is_null(), "{cp}");
    let md = r["markdown"].as_str().unwrap();
    assert!(
        md.contains("## Last words from the previous session") && md.contains("> Endpoint done, tests next."),
        "{md}"
    );
    assert!(!md.contains("Delegated work") && !md.contains("delegated task"), "{md}");

    let _ = std::fs::remove_dir_all(&dir);
}
