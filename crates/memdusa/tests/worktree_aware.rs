//! End-to-end: two worktrees of one repo share a project key. A checkpoint saved in
//! worktree A must not trigger a drift warning for an agent resuming in worktree B;
//! A's edits are summarized as "Other worktrees", not mixed into B's recent files.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

const PROJECT: &str = "github.com/demo/worktree-e2e";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct Env<'a> {
    home: &'a Path,
    cwd: &'a Path,
}

fn memdusa(env: &Env<'_>, args: &[&str], stdin: Option<Value>) -> String {
    // sqlite_url, not "sqlite://{path}": a Windows path's "C:" parses as host:port.
    let db = memdusa_core::store::sqlite_url(&env.home.join("t.db"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_memdusa"));
    cmd.args(["--database-url", &db, "--project", PROJECT])
        .args(args)
        .current_dir(env.cwd)
        .env("MEMDUSA_HOME", env.home)
        .env_remove("MEMDUSA_URL")
        .env_remove("MEMDUSA_TOKEN")
        .env_remove("MEMDUSA_PROJECT")
        .env_remove("MEMDUSA_TASK")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin.unwrap_or(json!({})).to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn hook(env: &Env<'_>, event: &str, session: &str, extra: Value) -> String {
    let mut payload = json!({ "session_id": session, "cwd": env.cwd });
    payload.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    memdusa(env, &["hook", event], Some(payload))
}

fn canonical(path: &str) -> std::path::PathBuf {
    fs::canonicalize(path).unwrap()
}

#[test]
fn checkpoints_and_files_are_scoped_to_their_worktree() {
    let root = std::env::temp_dir().join(format!("memdusa-wt-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    // One repo, one remote (so both worktrees resolve to the same project key), two worktrees.
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(&repo, &["remote", "add", "origin", "https://github.com/demo/worktree-e2e.git"]);
    fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "base.txt"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let wta = root.join("wta");
    let wtb = root.join("wtb");
    git(&repo, &["worktree", "add", "-q", "-b", "feat/a", wta.to_str().unwrap()]);
    git(&repo, &["worktree", "add", "-q", "-b", "feat/b", wtb.to_str().unwrap()]);

    let a = Env { home: &root, cwd: &wta };
    let b = Env { home: &root, cwd: &wtb };

    // Worker A: edit a file in its worktree, then end the session (auto checkpoint).
    hook(&a, "session-start", "sess-a", json!({ "source": "startup" }));
    hook(
        &a,
        "post-tool-use",
        "sess-a",
        json!({ "tool_name": "Edit", "tool_input": { "file_path": wta.join("a.rs") } }),
    );
    hook(&a, "session-end", "sess-a", json!({ "reason": "other" }));

    // The checkpoint carries the absolute worktree root, canonicalized across OSes.
    let state: Value = serde_json::from_str(&memdusa(&a, &["resume", "--json"], None)).unwrap();
    let saved = &state["checkpoints"][0]["git"]["worktree"];
    assert_eq!(canonical(saved.as_str().unwrap()), fs::canonicalize(&wta).unwrap(), "{saved}");
    assert_eq!(state["checkpoints"][0]["git"]["branch"], "feat/a");

    // Worker B starts in its own worktree: no scary drift, just the calm note.
    let brief = hook(&b, "session-start", "sess-b", json!({ "source": "startup" }));
    assert!(!brief.contains("⚠ Code state differs"), "{brief}");
    assert!(brief.contains("Latest checkpoint was saved in another worktree (../wta, branch feat/a)._"), "{brief}");
    assert!(brief.contains("Other worktrees: ../wta (1 files)"), "{brief}");
    assert!(!brief.contains("**Files changed recently:**"), "{brief}: nothing edited in B yet");

    // B's own edits are listed first; A's stay summarized under "Other worktrees".
    hook(
        &b,
        "post-tool-use",
        "sess-b",
        json!({ "tool_name": "Edit", "tool_input": { "file_path": wtb.join("b.rs") } }),
    );
    let brief = memdusa(&b, &["resume"], None);
    assert!(brief.contains("**Files changed recently:** b.rs"), "{brief}");
    assert!(brief.contains("Other worktrees: ../wta (1 files)"), "{brief}");
    assert!(!brief.contains("a.rs, b.rs"), "{brief}: files from both worktrees must not be mixed");

    let _ = fs::remove_dir_all(&root);
}
