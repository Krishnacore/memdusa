//! End-to-end sync mode (H3b): hooks and CLI write locally with the server
//! down, sync delivers, a second machine pulls; auto opt-in; delegated work
//! goes to the server; watcher lock and service rendering.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

use memdusa_core::model::MemoryQuery;
use memdusa_core::store::Store;

fn cmd(dir: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_memdusa"));
    c.args(args).env("MEMDUSA_HOME", dir).env("HOME", dir);
    c.env_remove("MEMDUSA_URL").env_remove("MEMDUSA_TOKEN").env_remove("MEMDUSA_DATABASE_URL");
    c
}

fn ok(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> String {
    let mut c = cmd(dir, args);
    for (k, v) in envs {
        c.env(k, v);
    }
    let out = c.output().unwrap();
    assert!(out.status.success(), "cli {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn hook(dir: &Path, project: &str, event: &str, payload: &str, envs: &[(&str, &str)]) -> String {
    let mut c = cmd(dir, &["--project", project, "hook", event]);
    for (k, v) in envs {
        c.env(k, v);
    }
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "hook {event} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn spawn_server(dir: &Path, db: &str, port: u16) -> Child {
    let mut c = Command::new(env!("CARGO_BIN_EXE_memdusa"));
    c.args([
        "--database-url",
        db,
        "serve",
        "--addr",
        &format!("127.0.0.1:{port}"),
        "--token",
        "root-token",
        "--no-auto-prune",
        "--no-metrics",
    ])
    .env("MEMDUSA_HOME", dir)
    .env("HOME", dir)
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    c.spawn().unwrap()
}

/// Wait until `port` answers AND the answer comes from our own `server`. `free_port` releases the
/// port before the server binds it; under a parallel test run another test's server can grab it, our
/// server then exits on the bind error, and the test would talk to the wrong server.
fn ready_and_ours(server: &mut Child, port: u16) -> bool {
    for _ in 0..200 {
        if server.try_wait().unwrap().is_some() {
            return false; // ours died (port taken)
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            sleep(Duration::from_millis(150));
            return server.try_wait().unwrap().is_none();
        }
        sleep(Duration::from_millis(25));
    }
    false
}

/// Start a server on a free port, retrying on another port if one was taken in between.
fn start_server(dir: &Path, db: &str) -> (Child, u16) {
    for _ in 0..5 {
        let port = free_port();
        let mut server = spawn_server(dir, db, port);
        if ready_and_ours(&mut server, port) {
            return (server, port);
        }
        let _ = server.kill();
        let _ = server.wait();
    }
    panic!("could not start a test server");
}

/// Restart on a known port (the login points at it).
fn restart_server(dir: &Path, db: &str, port: u16) -> Child {
    let mut server = spawn_server(dir, db, port);
    assert!(ready_and_ours(&mut server, port), "server on {port} didn't come back (port taken?)");
    server
}

fn stop(mut server: Child) {
    let _ = server.kill();
    let _ = server.wait();
}

fn scratch(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf, String) {
    let root = std::env::temp_dir().join(format!("memdusa-syncmode-{tag}-{}", memdusa_core::model::new_id()));
    let server_dir = root.join("server");
    let one = root.join("machine-one");
    let two = root.join("machine-two");
    for d in [&server_dir, &one, &two] {
        std::fs::create_dir_all(d).unwrap();
    }
    let db = memdusa_core::store::sqlite_url(&server_dir.join("server.db"));
    (root, server_dir, one, db)
}

fn key(server_dir: &Path, server_db: &str) -> String {
    let out = ok(server_dir, &["--database-url", server_db, "user", "add", "alice", "--key-name", "laptop"], &[]);
    out.lines().map(str::trim).find(|l| l.starts_with("mdk_")).expect("the key is printed once").to_owned()
}

fn login_sync(dir: &Path, url: &str, key: &str) {
    let out = ok(dir, &["login", url, "--token", key, "--sync"], &[]);
    assert!(out.contains("sync mode"), "{out}");
}

fn context(out: &str) -> String {
    let text = out.trim();
    if text.is_empty() {
        return String::new();
    }
    let v: serde_json::Value = serde_json::from_str(text).unwrap();
    v["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or("").to_owned()
}

#[tokio::test]
async fn hooks_write_locally_with_the_server_down_then_sync_delivers_to_a_second_machine() {
    let (root, server_dir, one, server_db) = scratch("offline");
    let (server, port) = start_server(&server_dir, &server_db);
    let url = format!("http://127.0.0.1:{port}");
    let key = key(&server_dir, &server_db);
    login_sync(&one, &url, &key);

    let project = "github.com/acme/offline-syncmode";
    // The server goes away before any memory or hook write.
    stop(server);

    let saved = ok(&one, &["--project", project, "remember", "an offline fact from machine one"], &[]);
    assert!(saved.contains("saved "), "{saved}");
    let out = hook(&one, project, "user-prompt-submit", r#"{"session_id":"s1","prompt":"offline prompt"}"#, &[]);
    assert!(context(&out).is_empty(), "a prompt hook injects nothing on this payload");

    // Auto opt-in happened locally even though the server was unreachable.
    let list = ok(&one, &["sync", "list"], &[]);
    assert!(list.contains(project), "{list}");

    let local = Store::connect(&memdusa_core::store::sqlite_url(&one.join("memdusa.db"))).await.unwrap();
    let events = local.events_recent(project, Some("s1"), 10).await.unwrap();
    assert!(events.iter().any(|e| e.content == "offline prompt"), "{events:?}");
    let memories = local
        .memory_search(MemoryQuery {
            project: Some(project.into()),
            query: Some("offline fact".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(memories.len(), 1, "{memories:?}");

    // Bring the server back and sync. Memory and event both cross the wire.
    let server = restart_server(&server_dir, &server_db, port);
    let out = ok(&one, &["sync"], &[]);
    assert!(out.contains("pushed 2"), "{out}");
    let remote = Store::connect(&server_db).await.unwrap();
    let remote_events = remote.events_recent(project, Some("s1"), 10).await.unwrap();
    assert!(remote_events.iter().any(|e| e.content == "offline prompt"), "{remote_events:?}");
    let remote_memories = remote
        .memory_search(MemoryQuery {
            project: Some(project.into()),
            query: Some("offline fact".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(remote_memories.len(), 1, "{remote_memories:?}");

    // A second machine pulls and sees the fact in its brief.
    let two = root.join("machine-two");
    login_sync(&two, &url, &key);
    let pulled = ok(&two, &["sync", "add", project], &[]);
    assert!(pulled.contains("pulled 2"), "{pulled}");
    let brief = ok(&two, &["--project", project, "resume", "--json"], &[]);
    let brief: serde_json::Value = serde_json::from_str(&brief).unwrap();
    assert!(brief["markdown"].as_str().unwrap().contains("an offline fact from machine one"), "{brief}");

    stop(server);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn auto_opt_in_can_be_disabled() {
    let (root, server_dir, one, server_db) = scratch("auto-add");
    let (server, port) = start_server(&server_dir, &server_db);
    let url = format!("http://127.0.0.1:{port}");
    let key = key(&server_dir, &server_db);
    login_sync(&one, &url, &key);

    let opted = "github.com/acme/auto-opted";
    let skipped = "github.com/acme/auto-skipped";
    hook(&one, opted, "user-prompt-submit", r#"{"session_id":"s1","prompt":"first"}"#, &[]);
    hook(
        &one,
        skipped,
        "user-prompt-submit",
        r#"{"session_id":"s2","prompt":"first"}"#,
        &[("MEMDUSA_SYNC_AUTO_ADD", "0")],
    );

    let list = ok(&one, &["sync", "list"], &[]);
    assert!(list.contains(opted), "{list}");
    assert!(!list.contains(skipped), "{list}");

    stop(server);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn delegate_and_claim_are_server_authoritative_in_sync_mode() {
    let (root, server_dir, one, server_db) = scratch("delegated");
    let (server, port) = start_server(&server_dir, &server_db);
    let url = format!("http://127.0.0.1:{port}");
    let key = key(&server_dir, &server_db);
    login_sync(&one, &url, &key);

    let project = "github.com/acme/delegated-syncmode";
    // `delegate` from a sync-mode client creates the run on the server, not locally.
    let report = ok(&one, &["--project", project, "delegate", "release", "docs"], &[]);
    assert!(report.contains("docs"), "{report}");
    let remote = Store::connect(&server_db).await.unwrap();
    let runs = remote.run_list_visible(Some(project), None, None, 10).await.unwrap();
    assert_eq!(runs.len(), 1, "the run lives on the server: {runs:?}");
    let run_id = runs[0].id.clone();
    let local = Store::connect(&memdusa_core::store::sqlite_url(&one.join("memdusa.db"))).await.unwrap();
    assert!(local.run_get(&run_id).await.is_err(), "no local lease/run in sync mode");

    // SessionStart claims the step on the server and shows the result in the brief.
    let out = hook(
        &one,
        project,
        "session-start",
        r#"{"session_id":"worker-1","source":"startup"}"#,
        &[("MEMDUSA_RUN", &run_id), ("MEMDUSA_TASK", "docs")],
    );
    let text = context(&out);
    assert!(text.contains("You claimed the delegated task `docs`"), "{text}");
    let steps = remote.run_get(&run_id).await.unwrap().steps;
    let step = steps.iter().find(|s| s.key == "docs").unwrap();
    assert!(step.owner.as_deref().is_some_and(|o| o.starts_with("alice/")), "{step:?}");

    // With the server down the claim fails visibly; no local lease is created.
    stop(server);
    let out = hook(
        &one,
        project,
        "session-start",
        r#"{"session_id":"worker-2","source":"startup"}"#,
        &[("MEMDUSA_RUN", &run_id), ("MEMDUSA_TASK", "docs")],
    );
    let text = context(&out);
    assert!(text.contains("could not claim the delegated task `docs`"), "{text}");
    assert!(text.contains("team server"), "{text}");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn watcher_is_single_instance_picks_up_changes_and_exits_on_sigterm() {
    let (root, server_dir, one, server_db) = scratch("watch");
    let (server, port) = start_server(&server_dir, &server_db);
    let url = format!("http://127.0.0.1:{port}");
    let key = key(&server_dir, &server_db);
    login_sync(&one, &url, &key);
    let project = "github.com/acme/watched";
    ok(&one, &["sync", "add", project], &[]);

    // The watcher's output goes to a file so a failure below can show what it was doing.
    let log_path = one.join("watcher.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut watcher = cmd(&one, &["sync", "--watch", "--interval", "100ms"])
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let watcher_log = || std::fs::read_to_string(&log_path).unwrap_or_default();
    let pid = watcher.id();
    // Wait until this watcher holds the lock: starting the second one earlier races for it (the
    // product keeps exactly one watcher either way, but the test would then wait on the wrong one).
    let mut locked = false;
    for _ in 0..100 {
        if ok(&one, &["sync", "status"], &[]).contains(&format!("running (pid {pid})")) {
            locked = true;
            break;
        }
        sleep(Duration::from_millis(100));
    }
    assert!(locked, "the watcher never took the lock");
    // A second watcher refuses to start while the lock is held.
    let second = ok(&one, &["sync", "--watch", "--interval", "100ms"], &[]);
    assert!(second.contains("already running"), "{second}");
    let status = ok(&one, &["sync", "status"], &[]);
    assert!(status.contains(&format!("running (pid {pid})")), "{status}");

    ok(&one, &["--project", project, "remember", "picked up by the watcher"], &[]);
    let remote = Store::connect(&server_db).await.unwrap();
    let mut found = false;
    // CI runners can be slow: allow up to 30 s.
    for _ in 0..300 {
        let memories = remote
            .memory_search(MemoryQuery {
                project: Some(project.into()),
                query: Some("picked up".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        if !memories.is_empty() {
            found = true;
            break;
        }
        sleep(Duration::from_millis(100));
    }
    assert!(found, "the watcher never pushed the local change; watcher log:\n{}", watcher_log());

    // SIGTERM shuts it down cleanly and releases the lock.
    // SIGTERM on Unix; Windows has no signals here, so the watcher is killed and its lock goes stale
    // (a dead pid reads as "not running").
    #[cfg(unix)]
    let _ = Command::new("kill").arg("-TERM").arg(pid.to_string()).status();
    #[cfg(not(unix))]
    let _ = watcher.kill();
    // Never hang the suite: give the watcher 10 s to exit, then kill it and fail.
    let mut exited = false;
    for _ in 0..100 {
        if watcher.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
        sleep(Duration::from_millis(100));
    }
    if !exited {
        let _ = watcher.kill();
        let _ = watcher.wait();
    }
    assert!(exited, "the watcher did not exit within 10 s of SIGTERM; watcher log:\n{}", watcher_log());
    let status = ok(&one, &["sync", "status"], &[]);
    assert!(status.contains("not running"), "{status}");

    stop(server);
    let _ = std::fs::remove_dir_all(&root);
}
