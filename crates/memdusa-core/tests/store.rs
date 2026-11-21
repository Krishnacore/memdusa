//! Store behaviour tests. Run against SQLite always, and against Postgres too
//! when `MEMDUSA_TEST_POSTGRES_URL` is set.

use memdusa_core::model::*;
use memdusa_core::{Error, Store, brief};
use serde_json::json;

async fn stores() -> Vec<(String, Store)> {
    let dir = std::env::temp_dir().join(format!("memdusa-test-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut out = vec![(
        "sqlite".to_string(),
        Store::connect(&memdusa_core::store::sqlite_url(&dir.join("t.db"))).await.unwrap(),
    )];
    if let Ok(url) = std::env::var("MEMDUSA_TEST_POSTGRES_URL") {
        out.push(("postgres".into(), Store::connect(&url).await.unwrap()));
    }
    out
}

/// Unique project per test so Postgres runs don't see each other's data.
fn project() -> String {
    format!("test/{}", new_id())
}

#[tokio::test]
async fn memory_dedup_scope_and_search() {
    for (name, s) in stores().await {
        let p = project();
        let a = s
            .memory_save(NewMemory {
                project: Some(p.clone()),
                content: "Uses Axum for HTTP".into(),
                tags: vec!["web".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        let b = s
            .memory_save(NewMemory {
                project: Some(p.clone()),
                content: "Uses Axum for HTTP".into(),
                tags: vec!["rust".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(a.id, b.id, "{name}: identical memory should dedupe");
        assert_eq!(b.tags, vec!["web", "rust"], "{name}");

        let g = s
            .memory_save(NewMemory {
                kind: MemoryKind::Preference,
                content: format!("likes small PRs {p}"),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(g.project.is_none());

        let found = s
            .memory_search(MemoryQuery { project: Some(p.clone()), query: Some("axum".into()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(found.len(), 1, "{name}: case-insensitive search");
        let found = s
            .memory_search(MemoryQuery { project: Some(p.clone()), query: Some("RUST".into()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(found.len(), 1, "{name}: tag search");
        let all = s
            .memory_search(MemoryQuery { project: Some(p.clone()), limit: Some(200), ..Default::default() })
            .await
            .unwrap();
        assert!(all.iter().any(|m| m.id == g.id), "{name}: global memories visible in project");

        s.memory_forget(&a.id).await.unwrap();
        assert!(matches!(s.memory_forget(&a.id).await, Err(Error::NotFound(_))), "{name}");
    }
}

/// The core durability promise: an agent crashes mid-run, another agent (a
/// different harness) claims the stale run and only redoes unfinished work.
#[tokio::test]
async fn run_survives_crash_and_handoff() {
    for (name, s) in stores().await {
        let p = project();
        let run = s
            .run_start(NewRun {
                project: p.clone(),
                name: "migrate".into(),
                owner: Some("claude-code".into()),
                lease_secs: Some(0),
                ..Default::default()
            })
            .await
            .unwrap()
            .run;

        // agent A: step 1 done, crashes during step 2
        assert!(matches!(
            s.step_begin(&run.id, "1-schema", None, "claude-code").await.unwrap(),
            StepDecision::Execute { attempt: 1, .. }
        ));
        s.step_complete(&run.id, "1-schema", Some(json!({"tables": 3}))).await.unwrap();
        assert!(matches!(
            s.step_begin(&run.id, "2-backfill", None, "claude-code").await.unwrap(),
            StepDecision::Execute { .. }
        ));

        // lease_secs = 0 → lease already expired → run is stale
        let r = s.run_get(&run.id).await.unwrap();
        assert!(r.run.stale, "{name}");

        // agent B (another harness) takes over and replays
        s.run_claim(&run.id, "cursor", Some(600), false).await.unwrap();
        match s.step_begin(&run.id, "1-schema", None, "cursor").await.unwrap() {
            StepDecision::Replay { output } => assert_eq!(output, Some(json!({"tables": 3})), "{name}"),
            d => panic!("{name}: expected replay, got {d:?}"),
        }
        match s.step_begin(&run.id, "2-backfill", None, "cursor").await.unwrap() {
            StepDecision::Execute { attempt, .. } => assert_eq!(attempt, 2, "{name}: crashed step re-executes"),
            d => panic!("{name}: expected execute, got {d:?}"),
        }

        // the original agent comes back but no longer holds the lease
        assert!(
            matches!(s.step_begin(&run.id, "3-verify", None, "claude-code").await, Err(Error::Conflict(_))),
            "{name}"
        );

        s.step_complete(&run.id, "2-backfill", Some(json!("ok"))).await.unwrap();
        // completing twice keeps the first output (journal is append-only)
        let st = s.step_complete(&run.id, "2-backfill", Some(json!("other"))).await.unwrap();
        assert_eq!(st.output, Some(json!("ok")), "{name}");

        let done = s.run_finish(&run.id, RunOutcome::Completed, Some(json!({"rows": 10})), None).await.unwrap();
        assert_eq!(done.run.status, RunStatus::Completed, "{name}");
        assert_eq!(done.steps.iter().map(|s| s.seq).collect::<Vec<_>>(), vec![1, 2], "{name}");
        assert!(matches!(s.step_begin(&run.id, "4", None, "cursor").await, Err(Error::Conflict(_))), "{name}");
    }
}

#[tokio::test]
async fn step_failure_then_retry() {
    for (name, s) in stores().await {
        let p = project();
        let run = s
            .run_start(NewRun { project: p, name: "deploy".into(), owner: Some("a".into()), ..Default::default() })
            .await
            .unwrap()
            .run;
        s.step_begin(&run.id, "push", None, "a").await.unwrap();
        s.step_fail(&run.id, "push", "registry timeout").await.unwrap();
        match s.step_begin(&run.id, "push", None, "a").await.unwrap() {
            StepDecision::Execute { attempt, previous_error } => {
                assert_eq!(attempt, 2, "{name}");
                assert_eq!(previous_error.as_deref(), Some("registry timeout"), "{name}");
            }
            d => panic!("{name}: {d:?}"),
        }
    }
}

#[tokio::test]
async fn idempotent_run_start() {
    for (name, s) in stores().await {
        let p = project();
        let mk = || NewRun {
            project: p.clone(),
            name: "nightly".into(),
            idempotency_key: Some("2026-11-21".into()),
            owner: Some("a".into()),
            ..Default::default()
        };
        let a = s.run_start(mk()).await.unwrap();
        let b = s.run_start(mk()).await.unwrap();
        assert_eq!(a.run.id, b.run.id, "{name}");
        // a different agent re-entering while the lease is live gets the run back, not an error
        let c = s.run_start(NewRun { owner: Some("b".into()), ..mk() }).await.unwrap();
        assert_eq!(c.run.id, a.run.id, "{name}");
        assert_eq!(c.run.owner.as_deref(), Some("a"), "{name}");
    }
}

#[tokio::test]
async fn brief_contains_everything() {
    for (name, s) in stores().await {
        let p = project();
        // global memories from other tests may show up; project-scoped state must be empty
        let empty = brief::build(&s, &p, None, None).await.unwrap();
        assert!(
            empty.checkpoints.is_empty() && empty.active_runs.is_empty() && empty.recent_events.is_empty(),
            "{name}"
        );

        s.checkpoint_save(NewCheckpoint {
            project: p.clone(),
            goal: "ship v1".into(),
            next: vec!["write README".into()],
            source: Some("codex".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        s.checkpoint_save(NewCheckpoint { project: p.clone(), goal: "ship v2".into(), ..Default::default() })
            .await
            .unwrap();
        s.memory_save(NewMemory {
            project: Some(p.clone()),
            kind: MemoryKind::Decision,
            content: "SQLite for local mode".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        s.event_append(NewEvent {
            project: p.clone(),
            kind: "commit".into(),
            content: "init".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        s.run_start(NewRun { project: p.clone(), name: "refactor".into(), ..Default::default() }).await.unwrap();

        let b = brief::build(&s, &p, None, None).await.unwrap();
        assert_eq!(b.checkpoints.len(), 1, "{name}: only latest checkpoint per task");
        assert_eq!(b.checkpoints[0].goal, "ship v2", "{name}");
        for needle in ["ship v2", "SQLite for local mode", "commit", "refactor"] {
            assert!(b.markdown.contains(needle), "{name}: brief missing {needle}:\n{}", b.markdown);
        }
    }
}

#[tokio::test]
async fn secrets_are_redacted_before_storage() {
    for (name, s) in stores().await {
        let p = project();
        let gh = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let m = s
            .memory_save(NewMemory {
                project: Some(p.clone()),
                content: format!("deploy token is {gh}"),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(m.content, "deploy token is [REDACTED:github_token]", "{name}");

        let e = s
            .event_append(NewEvent {
                project: p.clone(),
                kind: "command".into(),
                content: "export STRIPE_SECRET_KEY=sk_live_abcdefghijklmnop1234".into(),
                data: Some(json!({"password": "hunter2hunter2", "n": 3})),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!e.content.contains("sk_live"), "{name}: {}", e.content);
        assert_eq!(e.data.unwrap()["password"], "[REDACTED:field]", "{name}");

        let cp = s
            .checkpoint_save(NewCheckpoint {
                project: p.clone(),
                goal: "rotate creds".into(),
                notes: Some("old db url postgres://app:s3cr3tpass@db/prod".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(cp.notes.as_deref(), Some("old db url postgres://app:[REDACTED:url_password]@db/prod"), "{name}");

        let run =
            s.run_start(NewRun { project: p.clone(), name: "issue key".into(), ..Default::default() }).await.unwrap();
        s.step_begin(&run.run.id, "create", None, "anonymous").await.unwrap();
        let step = s.step_complete(&run.run.id, "create", Some(json!({"key": format!("new key {gh}")}))).await.unwrap();
        assert_eq!(step.output.unwrap()["key"], "new key [REDACTED:github_token]", "{name}");

        // and the persisted copy is redacted, not just the returned value
        let again = s.memory_search(MemoryQuery { project: Some(p.clone()), ..Default::default() }).await.unwrap();
        assert!(again.iter().all(|m| !m.content.contains("ghp_")), "{name}");
    }
}

#[tokio::test]
async fn brief_warns_when_code_state_drifted() {
    for (name, s) in stores().await {
        let p = project();
        let saved = GitState {
            branch: Some("feat/refunds".into()),
            head: Some("a1b2c3d4e5f6".into()),
            dirty_count: 2,
            dirty_files: vec!["src/refund.ts".into(), "src/api.ts".into()],
            host: Some("laptop".into()),
        };
        s.checkpoint_save(NewCheckpoint {
            project: p.clone(),
            goal: "refunds".into(),
            git: Some(saved.clone()),
            ..Default::default()
        })
        .await
        .unwrap();

        let same = brief::build(&s, &p, None, Some(&saved)).await.unwrap();
        assert!(same.git_warnings.is_empty(), "{name}");
        assert!(
            same.markdown.contains("**Code:** `feat/refunds` @ `a1b2c3d` · 2 uncommitted"),
            "{name}: {}",
            same.markdown
        );

        let elsewhere = GitState {
            host: Some("desktop".into()),
            branch: Some("main".into()),
            dirty_count: 0,
            dirty_files: vec![],
            ..saved
        };
        let b = brief::build(&s, &p, None, Some(&elsewhere)).await.unwrap();
        assert_eq!(b.git_warnings.len(), 2, "{name}: {:?}", b.git_warnings);
        let warn_at = b.markdown.find("⚠ Code state differs").expect("warning section");
        assert!(warn_at < b.markdown.find("Where things stand").unwrap(), "{name}: warnings come first");
        assert!(b.markdown.contains("saved on `laptop`") && b.markdown.contains("you're on `main`"), "{name}");
    }
}

#[tokio::test]
async fn retention_summarizes_old_activity_and_keeps_what_matters() {
    use memdusa_core::retention::RetentionPolicy;
    for (name, s) in stores().await {
        let p = project();
        let day = 86_400_000;
        let old = now_ms() - 40 * day;
        let ev = |kind: &str, content: &str, session: &str, at: i64| NewEvent {
            project: p.clone(),
            session: Some(session.into()),
            source: Some("claude-code".into()),
            kind: kind.into(),
            content: content.into(),
            created_at: Some(at),
            ..Default::default()
        };
        for e in [
            ev("prompt", "add refunds", "old-sess", old),
            ev("file_change", "src/refund.ts", "old-sess", old + 1),
            ev("file_change", "src/refund.ts", "old-sess", old + 2),
            ev("command", "npm test", "old-sess", old + 3),
            ev("note", "agent-logged insight: keep forever", "old-sess", old + 4),
            ev("file_change", "src/recent.ts", "new-sess", now_ms()),
        ] {
            s.event_append(e).await.unwrap();
        }
        // three checkpoints for one task (two old) + one old checkpoint that is the latest of its task
        let cp = |goal: &str, task: Option<&str>, at: i64| NewCheckpoint {
            project: p.clone(),
            goal: goal.into(),
            task: task.map(String::from),
            created_at: Some(at),
            ..Default::default()
        };
        let ancient = now_ms() - 100 * day; // beyond the 90-day checkpoint window
        for c in [
            cp("v1", None, ancient),
            cp("v2", None, ancient + 1),
            cp("v3", None, now_ms()),
            cp("docs", Some("docs"), ancient),
        ] {
            s.checkpoint_save(c).await.unwrap();
        }
        let run = s.run_start(NewRun { project: p.clone(), name: "old".into(), ..Default::default() }).await.unwrap();
        s.step_begin(&run.run.id, "a", None, "anonymous").await.unwrap();
        s.run_finish(&run.run.id, RunOutcome::Completed, None, None).await.unwrap();

        // runs are aged by updated_at: finished "now", so nothing to delete with 90 days...
        let policy = RetentionPolicy::default();
        let dry = s.prune(&policy, Some(&p), true).await.unwrap();
        assert_eq!(
            (dry.sessions_summarized, dry.events_deleted, dry.checkpoints_deleted, dry.runs_deleted),
            (1, 4, 2, 0),
            "{name}: {dry:?}"
        );
        assert_eq!(s.events_recent(&p, None, 100).await.unwrap().len(), 6, "{name}: dry run changes nothing");

        let report = s.prune(&policy, Some(&p), false).await.unwrap();
        assert_eq!((report.events_deleted, report.checkpoints_deleted), (4, 2), "{name}");
        let events = s.events_recent(&p, None, 100).await.unwrap();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert!(
            kinds.contains(&"note") && kinds.contains(&"file_change"),
            "{name}: agent notes and recent activity stay"
        );
        let summary = events.iter().find(|e| e.kind == "session_summary").expect("summary");
        assert_eq!(summary.created_at, old + 3, "{name}: summary sits where the session ended");
        assert!(
            summary.content.contains(
                "1 file(s) changed (src/refund.ts), 1 command(s), 1 prompt(s). First request: \"add refunds\""
            ),
            "{name}: {}",
            summary.content
        );
        let goals: Vec<String> = s.checkpoints_latest(&p, 10).await.unwrap().into_iter().map(|c| c.goal).collect();
        assert_eq!(goals, vec!["v3", "docs"], "{name}: latest per task kept, even if old");

        // finished runs: aged by when they finished (here: just now), so use an explicit cutoff
        let runs_only = memdusa_core::retention::Cutoffs { runs: Some(now_ms() + 1), ..Default::default() };
        let dry = s.prune_older_than(runs_only, Some(&p), true).await.unwrap();
        assert_eq!((dry.runs_deleted, dry.steps_deleted), (1, 1), "{name}");
        let gone = s.prune_older_than(runs_only, Some(&p), false).await.unwrap();
        assert_eq!((gone.runs_deleted, gone.steps_deleted), (1, 1), "{name}");
        assert!(matches!(s.run_get(&run.run.id).await, Err(memdusa_core::Error::NotFound(_))), "{name}");

        // 0 days means "keep forever"
        let none = s
            .prune(&RetentionPolicy { activity_days: 0, checkpoint_days: 0, run_days: 0 }, Some(&p), false)
            .await
            .unwrap();
        assert_eq!(none, PruneReportLike::zero(), "{name}");
    }
}

struct PruneReportLike;
impl PruneReportLike {
    fn zero() -> memdusa_core::retention::PruneReport {
        memdusa_core::retention::PruneReport::default()
    }
}

#[tokio::test]
async fn export_import_round_trip_is_idempotent() {
    use memdusa_core::transfer::{self, Record};
    let stores = stores().await;
    let (_, src) = &stores[0]; // sqlite
    let p = project();
    src.memory_save(NewMemory { project: Some(p.clone()), content: "uses JWT".into(), ..Default::default() })
        .await
        .unwrap();
    src.event_append(NewEvent {
        project: p.clone(),
        kind: "commit".into(),
        content: "init".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    src.checkpoint_save(NewCheckpoint {
        project: p.clone(),
        goal: "ship".into(),
        next: vec!["docs".into()],
        ..Default::default()
    })
    .await
    .unwrap();
    let run = src
        .run_start(NewRun {
            project: p.clone(),
            name: "deploy".into(),
            idempotency_key: Some("k".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    src.step_begin(&run.run.id, "build", None, "anonymous").await.unwrap();
    src.step_complete(&run.run.id, "build", Some(json!({"image": "app:1"}))).await.unwrap();

    let records = src.export(Some(&p)).await.unwrap();
    assert!(matches!(records[0], Record::Header { .. }));
    let text = transfer::to_jsonl(&records);
    let parsed = transfer::parse_jsonl(&text).unwrap();
    assert_eq!(parsed.len(), 5, "header + memory + event + checkpoint + run");

    // into every backend (sqlite -> fresh sqlite, and -> postgres when available)
    for (name, _) in &stores {
        let dir = std::env::temp_dir().join(format!("memdusa-import-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dst = match name.as_str() {
            "sqlite" => Store::connect(&memdusa_core::store::sqlite_url(&dir.join("dst.db"))).await.unwrap(),
            _ => stores.iter().find(|(n, _)| n == name).unwrap().1.clone(),
        };
        let r = dst.import(parsed.clone()).await.unwrap();
        assert_eq!((r.memories, r.events, r.checkpoints, r.runs, r.steps, r.skipped), (1, 1, 1, 1, 1, 0), "{name}");
        let again = dst.import(parsed.clone()).await.unwrap();
        assert_eq!((again.memories, again.runs, again.skipped), (0, 0, 4), "{name}: re-import skips everything");

        // identical project data on both sides (a shared test DB may hold other tests' global memories,
        // so compare the project-scoped export rather than the brief)
        let strip = |r: Vec<Record>| -> Vec<String> {
            r.into_iter()
                .filter(|r| !matches!(r, Record::Header { .. }))
                .map(|r| serde_json::to_string(&r).unwrap().replace("\"stale\":true", "\"stale\":false"))
                .collect()
        };
        assert_eq!(strip(src.export(Some(&p)).await.unwrap()), strip(dst.export(Some(&p)).await.unwrap()), "{name}");
        let step = &dst.run_get(&run.run.id).await.unwrap().steps[0];
        assert_eq!(step.output, Some(json!({"image": "app:1"})), "{name}: journal intact, replay still works");
    }

    assert!(transfer::parse_jsonl("{\"type\":\"bogus\"}").is_err());

    // SQLite online backup
    let dir = std::env::temp_dir().join(format!("memdusa-backup-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("copy.db");
    src.backup_sqlite(&file.display().to_string()).await.unwrap();
    let copy = Store::connect(&memdusa_core::store::sqlite_url(&file)).await.unwrap();
    assert_eq!(copy.export(Some(&p)).await.unwrap().len(), 5);
    assert!(src.backup_sqlite(&file.display().to_string()).await.is_err(), "won't overwrite");
}
