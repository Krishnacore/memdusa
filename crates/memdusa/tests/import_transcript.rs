//! `memdusa import-transcript` end to end against a local database in a temp dir.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const T0: i64 = 1_772_359_200_000; // 2026-03-01T10:00:00Z, the fixture's first prompt

struct Env {
    dir: PathBuf,
}

impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("memdusa-import-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn run(&self, args: &[&str]) -> Output {
        // sqlite_url, not "sqlite://{path}": a Windows path's "C:" parses as host:port
        let db = memdusa_core::store::sqlite_url(&self.dir.join("t.db"));
        let out = Command::new(env!("CARGO_BIN_EXE_memdusa"))
            .args(["--database-url", &db, "--project", "demo"])
            .args(args)
            .env("MEMDUSA_HOME", &self.dir)
            .env("HOME", &self.dir)
            .env_remove("MEMDUSA_URL")
            .env_remove("MEMDUSA_TOKEN")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        out
    }

    fn stdout(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.run(args).stdout).into_owned()
    }

    fn records(&self, kind: &str) -> Vec<Value> {
        let export = self.stdout(&["export"]);
        let mut found: Vec<Value> = export
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["type"] == kind)
            .collect();
        found.sort_by_key(|v| v["created_at"].as_i64());
        found
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transcripts").join(name).display().to_string()
}

#[test]
fn claude_code_import_records_events_checkpoint_and_is_idempotent() {
    let env = Env::new("claude");
    let file = fixture("claude-code.jsonl");

    let dry = env.stdout(&["import-transcript", "claude-code", &file, "--dry-run"]);
    assert!(dry.starts_with("would import sess-claude-1 (2 prompts, 3 file changes, 1 commands)"), "{dry}");
    assert!(dry.contains("2026-03-01T10:00:00Z to 2026-03-01T10:02:00Z"), "{dry}");
    assert!(env.records("event").is_empty(), "dry run writes nothing");

    let out = env.stdout(&["import-transcript", "claude-code", &file]);
    assert!(out.starts_with("imported sess-claude-1 (2 prompts, 3 file changes, 1 commands) from "), "{out}");

    let events = env.records("event");
    let of = |kind: &str| events.iter().filter(|e| e["kind"] == kind).collect::<Vec<_>>();
    let prompts = of("prompt");
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0]["created_at"], T0);
    assert_eq!(prompts[0]["content"], "Add a refunds endpoint to the billing service");
    assert_eq!(prompts[1]["created_at"], T0 + 60_000);
    assert_eq!(prompts[1]["data"]["text"], "Also add tests\nfor partial refunds");
    let changes = of("file_change");
    let paths: Vec<_> = changes.iter().map(|e| e["content"].as_str().unwrap()).collect();
    assert_eq!(paths, ["src/refunds.rs", "tests/refunds.rs", "src/lib.rs"]);
    assert_eq!(changes[0]["created_at"], T0 + 5_000);
    assert_eq!(of("command").len(), 1);
    assert_eq!(of("command")[0]["created_at"], T0 + 20_000);
    assert_eq!(of("command")[0]["content"], "cargo test refunds");
    assert!(events.iter().all(|e| e["session"] == "sess-claude-1" && e["source"] == "claude-code"));
    assert_eq!(of("session_end")[0]["created_at"], T0 + 120_000);

    let checkpoints = env.records("checkpoint");
    assert_eq!(checkpoints.len(), 1, "fallback checkpoint");
    let notes = checkpoints[0]["notes"].as_str().unwrap();
    assert!(notes.contains("> Add a refunds endpoint to the billing service"), "{notes}");

    // a second import is skipped...
    let again = env.stdout(&["import-transcript", "claude-code", &file]);
    assert!(again.starts_with("skipped sess-claude-1: already imported"), "{again}");
    assert_eq!(env.records("event").len(), events.len());

    // ...unless forced
    let forced = env.stdout(&["import-transcript", "claude-code", &file, "--force"]);
    assert!(forced.starts_with("imported sess-claude-1"), "{forced}");
    assert_eq!(env.records("event").len(), events.len() * 2);
}

#[test]
fn codex_and_pi_fixtures_import() {
    let env = Env::new("others");
    let codex = env.stdout(&["import-transcript", "codex", &fixture("codex.jsonl")]);
    assert!(codex.starts_with("imported sess-codex-1 (1 prompts, 3 file changes, 1 commands)"), "{codex}");
    let pi = env.stdout(&["import-transcript", "pi", &fixture("pi.jsonl")]);
    assert!(pi.starts_with("imported sess-pi-1 (1 prompts, 2 file changes, 1 commands)"), "{pi}");
}
