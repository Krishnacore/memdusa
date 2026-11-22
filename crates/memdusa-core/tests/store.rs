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
        let empty = brief::build(&s, &p, None, None, None).await.unwrap();
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

        let b = brief::build(&s, &p, None, None, None).await.unwrap();
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
            worktree: None,
        };
        s.checkpoint_save(NewCheckpoint {
            project: p.clone(),
            goal: "refunds".into(),
            git: Some(saved.clone()),
            ..Default::default()
        })
        .await
        .unwrap();

        let same = brief::build(&s, &p, None, Some(&saved), None).await.unwrap();
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
        let b = brief::build(&s, &p, None, Some(&elsewhere), None).await.unwrap();
        assert_eq!(b.git_warnings.len(), 2, "{name}: {:?}", b.git_warnings);
        let warn_at = b.markdown.find("⚠ Code state differs").expect("warning section");
        assert!(warn_at < b.markdown.find("Where things stand").unwrap(), "{name}: warnings come first");
        assert!(b.markdown.contains("saved on `laptop`") && b.markdown.contains("you're on `main`"), "{name}");
    }
}

#[tokio::test]
async fn brief_uses_the_callers_worktree_and_summarizes_others() {
    for (name, s) in stores().await {
        let p = project();
        let wt = |path: &str, branch: &str, head: &str| GitState {
            branch: Some(branch.into()),
            head: Some(head.into()),
            host: Some("mbp".into()),
            worktree: Some(path.into()),
            ..Default::default()
        };

        // Worker A saved the newest project checkpoint, on its own branch/worktree.
        let a = wt("/work/wta", "feat/a", "aaa1111");
        s.checkpoint_save(NewCheckpoint {
            project: p.clone(),
            goal: "A's work".into(),
            git: Some(a.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
        // A's edits are stored with cwd = its worktree.
        for f in ["a.rs", "b.rs"] {
            s.event_append(NewEvent {
                project: p.clone(),
                kind: "file_change".into(),
                content: f.into(),
                data: Some(json!({ "cwd": "/work/wta" })),
                ..Default::default()
            })
            .await
            .unwrap();
        }

        // Caller in worktree B: no checkpoint of its own yet.
        let b = wt("/work/wtb", "feat/b", "bbb2222");
        let brief = brief::build(&s, &p, None, Some(&b), None).await.unwrap();
        assert!(brief.git_warnings.is_empty(), "{name}: {:?}", brief.git_warnings);
        assert_eq!(
            brief.other_worktree_note.as_deref(),
            Some("_Latest checkpoint was saved in another worktree (../wta, branch feat/a)._"),
            "{name}"
        );
        assert!(!brief.markdown.contains("⚠ Code state differs"), "{name}");
        assert!(brief.markdown.contains("Latest checkpoint was saved in another worktree"), "{name}");
        assert!(brief.markdown.contains("Other worktrees: ../wta (2 files)"), "{name}: {}", brief.markdown);
        assert!(!brief.markdown.contains("**Files changed recently:**"), "{name}: nothing changed in B");

        // An older checkpoint in B wins: compare with that, and no "other worktree" note.
        let older = wt("/work/wtb", "feat/b", "bbb2222");
        s.checkpoint_save(NewCheckpoint {
            project: p.clone(),
            goal: "B's earlier stop".into(),
            created_at: Some(now_ms() - 60_000),
            git: Some(older),
            ..Default::default()
        })
        .await
        .unwrap();
        let brief = brief::build(&s, &p, None, Some(&b), None).await.unwrap();
        assert!(brief.git_warnings.is_empty(), "{name}: {:?}", brief.git_warnings);
        assert!(brief.other_worktree_note.is_none(), "{name}");
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

fn mem(project: &Option<String>, content: &str, tags: &[&str]) -> NewMemory {
    NewMemory {
        project: project.clone(),
        content: content.into(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        ..Default::default()
    }
}

fn query(project: &str, q: &str) -> MemoryQuery {
    MemoryQuery { project: Some(project.into()), query: Some(q.into()), ..Default::default() }
}

fn ids(v: Vec<Memory>) -> Vec<String> {
    let mut ids: Vec<String> = v.into_iter().map(|m| m.id).collect();
    ids.sort();
    ids
}

fn sorted(mut v: Vec<&String>) -> Vec<String> {
    v.sort();
    v.into_iter().cloned().collect()
}

#[tokio::test]
async fn search_ranks_by_relevance_not_recency() {
    for (name, s) in stores().await {
        let p = project();
        let proj = Some(p.clone());
        let u = new_id().replace('-', "");
        let (a, b) = (format!("{u}aa"), format!("{u}bb"));
        // the better match is saved first, so it is the older one
        let both = s.memory_save(mem(&proj, &format!("{a} and {b} together"), &[])).await.unwrap();
        let one = s.memory_save(mem(&proj, &format!("only {a} here"), &[])).await.unwrap();
        // filler, so no term is in most documents
        for i in 0..8 {
            s.memory_save(mem(&proj, &format!("unrelated filler number {i} about gardening"), &["misc"]))
                .await
                .unwrap();
        }

        let found = s.memory_search_ranked(query(&p, &format!("{a} {b}"))).await.unwrap();
        let order: Vec<_> = found.iter().map(|(m, _)| m.id.as_str()).collect();
        assert_eq!(order, vec![both.id.as_str(), one.id.as_str()], "{name}: ANY word matches, best first");
        assert!(found[0].1 > found[1].1, "{name}: scores {:?}", found.iter().map(|f| f.1).collect::<Vec<_>>());

        // tags are searched too
        let tagged = s.memory_save(mem(&proj, "something else entirely", &[&format!("{u}tag")])).await.unwrap();
        let found = s.memory_search(query(&p, &format!("{u}tag"))).await.unwrap();
        assert_eq!(ids(found), vec![tagged.id.clone()], "{name}");

        // updates and deletes keep the index in sync
        s.memory_save(mem(&proj, &format!("only {a} here"), &[&format!("{u}later")])).await.unwrap();
        let found = s.memory_search(query(&p, &format!("{u}later"))).await.unwrap();
        assert_eq!(ids(found), vec![one.id.clone()], "{name}: merged tag searchable");
        s.memory_forget(&one.id).await.unwrap();
        let found = s.memory_search(query(&p, &a)).await.unwrap();
        assert_eq!(ids(found), vec![both.id.clone()], "{name}: forgotten is gone");

        // no query: plain listing, no scores
        let recent =
            s.memory_search_ranked(MemoryQuery { project: Some(p.clone()), ..Default::default() }).await.unwrap();
        assert!(!recent.is_empty() && recent.iter().all(|(_, score)| *score == 0.0), "{name}");
    }
}

#[tokio::test]
async fn search_survives_special_characters() {
    for (name, s) in stores().await {
        let p = project();
        let proj = Some(p.clone());
        s.memory_save(mem(&proj, "Don't use unwrap: prefer ? (the try operator) -- it's \"safer\"", &[]))
            .await
            .unwrap();
        for q in [
            "\"",
            "*",
            ":",
            "(",
            "-",
            "'",
            "\"unbalanced",
            "a AND OR NOT b",
            "NEAR(a b)",
            "col:val",
            "foo*",
            "'; DROP TABLE memories; --",
            "!!!",
            "(((",
            "\\",
            "a | b & !c",
            "<->",
            "unwrap:",
            "-unwrap",
        ] {
            let r = s.memory_search(query(&p, q)).await;
            assert!(r.is_ok(), "{name}: query {q:?} failed: {r:?}");
        }
        let found = s.memory_search(query(&p, "\"unwrap\": (try) -safer*")).await.unwrap();
        assert_eq!(found.len(), 1, "{name}: words inside the noise still match");
        assert!(s.memory_search(query(&p, "!!!")).await.unwrap().is_empty(), "{name}: nothing searchable");
        // a shared Postgres test DB also holds other tests' (visible) global memories: count only this project's
        let recent = s.memory_search(query(&p, "   ")).await.unwrap();
        assert_eq!(
            recent.iter().filter(|m| m.project.as_deref() == Some(p.as_str())).count(),
            1,
            "{name}: blank query lists recent"
        );
    }
}

#[tokio::test]
async fn search_visibility_rules_unchanged() {
    for (name, s) in stores().await {
        let (p, other) = (project(), project());
        let word = format!("{}vis", new_id().replace('-', ""));
        let mine = s.memory_save(mem(&Some(p.clone()), &format!("{word} project memory"), &[])).await.unwrap();
        s.memory_save(mem(&Some(other.clone()), &format!("{word} other project"), &[])).await.unwrap();
        let shared = s.memory_save(mem(&None, &format!("{word} shared global"), &[])).await.unwrap();
        let alice = s
            .memory_save(NewMemory { user: Some("alice".into()), ..mem(&None, &format!("{word} alice global"), &[]) })
            .await
            .unwrap();

        // local mode (no user): own project + user-less globals, never another project's memory
        let got = ids(s.memory_search(query(&p, &word)).await.unwrap());
        assert_eq!(got, sorted(vec![&mine.id, &shared.id]), "{name}");
        // alice sees her globals too
        let got = ids(s.memory_search(MemoryQuery { user: Some("alice".into()), ..query(&p, &word) }).await.unwrap());
        assert_eq!(got, sorted(vec![&mine.id, &shared.id, &alice.id]), "{name}");
        // bob doesn't see alice's
        let got = ids(s.memory_search(MemoryQuery { user: Some("bob".into()), ..query(&p, &word) }).await.unwrap());
        assert_eq!(got, sorted(vec![&mine.id, &shared.id]), "{name}");
        // kind filter still applies
        let got = s.memory_search(MemoryQuery { kind: Some(MemoryKind::Decision), ..query(&p, &word) }).await.unwrap();
        assert!(got.is_empty(), "{name}");
    }
}

#[tokio::test]
async fn v3_database_migrates_to_v4_and_backfills() {
    let dir = std::env::temp_dir().join(format!("memdusa-v3-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = memdusa_core::store::sqlite_url(&dir.join("old.db"));
    let p = project();
    let proj = Some(p.clone());

    // build a current database, then turn it back into a v3 one by hand
    let s = Store::connect(&url).await.unwrap();
    s.memory_save(mem(&proj, "we chose postgres for the ledger", &["db"])).await.unwrap();
    drop(s);
    sqlx::any::install_default_drivers();
    let raw = sqlx::AnyPool::connect(&format!("{url}?mode=rw")).await.unwrap();
    for stmt in [
        "DROP TRIGGER memories_fts_ai",
        "DROP TRIGGER memories_fts_ad",
        "DROP TRIGGER memories_fts_au",
        "DROP TABLE memories_fts",
        // v5 columns, so the migrations from v3 on all run again
        "ALTER TABLE steps DROP COLUMN owner",
        "ALTER TABLE steps DROP COLUMN lease_until",
        "UPDATE memdusa_meta SET value = '3' WHERE key = 'schema_version'",
        "INSERT INTO memories (id, project, kind, content, tags, created_at, updated_at) \
         VALUES ('legacy-1', 'PROJECT', 'note', 'legacy cache invalidation notes', '[\"perf\"]', 1, 1)",
    ] {
        let stmt = stmt.replace("PROJECT", &p);
        sqlx::query(sqlx::AssertSqlSafe(stmt)).execute(&raw).await.unwrap();
    }
    raw.close().await;

    let s = Store::connect(&url).await.unwrap();
    assert_eq!(s.migrate().await.unwrap(), 6);
    // v5: steps can be delegated and leased
    let run = s.run_delegate(&p, "after migration", vec!["t".into()], None).await.unwrap();
    assert!(matches!(
        s.step_claim(&run.run.id, "t", "w", 60, false).await.unwrap(),
        ClaimResult::Claimed { attempt: 1, .. }
    ));
    let found = s.memory_search(query(&p, "cache")).await.unwrap();
    assert_eq!(found.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["legacy-1"], "backfilled");
    assert_eq!(s.memory_search(query(&p, "ledger")).await.unwrap().len(), 1);
    assert_eq!(s.memory_search(query(&p, "perf")).await.unwrap().len(), 1, "tags backfilled");
    // and new writes are indexed
    s.memory_save(mem(&proj, "freshly written note about sharding", &[])).await.unwrap();
    assert_eq!(s.memory_search(query(&p, "sharding")).await.unwrap().len(), 1);
}

#[tokio::test]
async fn memory_update_changes_fields_and_reindexes() {
    for (name, s) in stores().await {
        let p = project();
        let u = new_id().replace('-', "");
        let (old, new) = (format!("{u}old"), format!("{u}new"));
        let m = s.memory_save(mem(&Some(p.clone()), &format!("uses {old} store"), &["a"])).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let patch = MemoryPatch {
            content: Some(format!("uses {new} store")),
            kind: Some(MemoryKind::Decision),
            tags: Some(vec!["b".into()]),
            source: Some("agent".into()),
        };
        let up = s.memory_update(&m.id, patch).await.unwrap();
        assert_eq!(
            (up.kind, up.tags.clone(), up.source.as_deref()),
            (MemoryKind::Decision, vec!["b".to_string()], Some("agent")),
            "{name}"
        );
        assert!(up.updated_at > m.updated_at && up.created_at == m.created_at, "{name}");

        assert!(s.memory_search(query(&p, &old)).await.unwrap().is_empty(), "{name}: old words gone");
        let found = s.memory_search(query(&p, &new)).await.unwrap();
        assert_eq!(ids(found.clone()), vec![m.id.clone()], "{name}");
        assert_eq!((found[0].kind, found[0].content.as_str()), (MemoryKind::Decision, up.content.as_str()), "{name}");

        // a partial patch leaves the rest alone
        let up = s.memory_update(&m.id, MemoryPatch { tags: Some(vec![]), ..Default::default() }).await.unwrap();
        assert_eq!((up.content.contains(&new), up.kind, up.tags.len()), (true, MemoryKind::Decision, 0), "{name}");

        // redaction applies to content and tags
        let gh = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let up = s
            .memory_update(
                &m.id,
                MemoryPatch { content: Some(format!("token {gh}")), tags: Some(vec![gh.into()]), ..Default::default() },
            )
            .await
            .unwrap();
        assert_eq!(up.content, "token [REDACTED:github_token]", "{name}");
        assert!(!up.tags[0].contains("ghp_"), "{name}: {:?}", up.tags);

        assert!(matches!(s.memory_update("missing", MemoryPatch::default()).await, Err(Error::NotFound(_))), "{name}");
        let empty = MemoryPatch { content: Some("  ".into()), ..Default::default() };
        assert!(matches!(s.memory_update(&m.id, empty).await, Err(Error::Invalid(_))), "{name}");
    }
}

#[tokio::test]
async fn near_duplicate_saves_merge_into_the_existing_memory() {
    for (name, s) in stores().await {
        let (p, other) = (project(), project());
        let first = s.memory_save(mem(&Some(p.clone()), "The API uses JWT tokens for auth", &["auth"])).await.unwrap();
        assert!(!first.merged, "{name}");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let second =
            s.memory_save(mem(&Some(p.clone()), "API uses JWT tokens for authentication", &["jwt"])).await.unwrap();
        assert!(second.merged && second.id == first.id, "{name}");
        assert_eq!(second.content, "API uses JWT tokens for authentication", "{name}");
        assert_eq!(second.tags, vec!["auth".to_string(), "jwt".to_string()], "{name}");
        assert!(second.updated_at > first.updated_at, "{name}");
        let all = s.memory_search(query(&p, "jwt")).await.unwrap();
        let mine: Vec<_> = all.iter().filter(|m| m.project.as_deref() == Some(p.as_str())).collect();
        assert_eq!((mine.len(), mine[0].content.as_str()), (1, "API uses JWT tokens for authentication"), "{name}");

        // another kind, another project, or clearly different content stay separate
        let kind =
            NewMemory { kind: MemoryKind::Decision, ..mem(&Some(p.clone()), "The API uses JWT tokens for auth", &[]) };
        let k = s.memory_save(kind).await.unwrap();
        assert!(!k.merged && k.id != first.id, "{name}: different kind");
        let o = s.memory_save(mem(&Some(other.clone()), "The API uses JWT tokens for auth", &[])).await.unwrap();
        assert!(!o.merged && o.id != first.id, "{name}: different project");
        let d =
            s.memory_save(mem(&Some(p.clone()), "Deploys go through the staging cluster first", &[])).await.unwrap();
        assert!(!d.merged, "{name}: different content");
    }
}

#[tokio::test]
async fn brief_flags_memories_about_files_changed_since() {
    let pause = || tokio::time::sleep(std::time::Duration::from_millis(5));
    for (name, s) in stores().await {
        let p = project();
        let before = s.memory_save(mem(&Some(p.clone()), "Auth lives in src/auth/jwt.rs", &[])).await.unwrap();
        s.memory_save(mem(&Some(p.clone()), "Prefer small functions everywhere", &[])).await.unwrap();
        pause().await;
        let change = |path: &str| NewEvent {
            project: p.clone(),
            kind: "file_change".into(),
            content: path.into(),
            ..Default::default()
        };
        s.event_append(change("/home/dev/repo/src/auth/jwt.rs")).await.unwrap();
        pause().await;
        s.memory_save(mem(&Some(p.clone()), "Sessions are stored in src/auth/jwt.rs too", &["x"])).await.unwrap();

        let b = brief::build(&s, &p, None, None, None).await.unwrap();
        let line = |needle: &str| b.markdown.lines().find(|l| l.contains(needle)).unwrap_or_default().to_owned();
        let flagged = line("Auth lives in");
        assert!(flagged.contains("may be outdated: src/auth/jwt.rs changed"), "{name}: {flagged}");
        assert!(!line("Sessions are stored").contains("outdated"), "{name}: saved after the change");
        assert!(!line("Prefer small functions").contains("outdated"), "{name}: no paths");
        let _ = before;
    }
}

/// 30 memories, the 2 about the checkpoint's goal are the oldest.
async fn seed_many(s: &Store, p: &str) {
    let pause = || tokio::time::sleep(std::time::Duration::from_millis(3));
    let project = Some(p.to_owned());
    for text in ["Zebra migration must keep ids stable", "Run zebra migration dry first"] {
        s.memory_save(mem(&project, text, &[])).await.unwrap();
        pause().await;
    }
    for i in 0..28 {
        s.memory_save(mem(&project, &format!("filler{i} lorem{i} ipsum{i} dolor{i} sit{i} amet{i}"), &[]))
            .await
            .unwrap();
        pause().await;
    }
}

fn goal(p: &str, goal: &str) -> NewCheckpoint {
    NewCheckpoint { project: p.into(), goal: goal.into(), ..Default::default() }
}

/// Postgres tests share one database, so global memories other tests saved show up in every brief
/// (global preferences are always shown). Returns how many globals are visible and the budget their
/// preferences take, so assertions can be about the test project's own memories.
async fn visible_globals(s: &Store) -> (usize, usize) {
    let q = MemoryQuery { project: Some("no-such-project".into()), limit: Some(200), ..Default::default() };
    let globals: Vec<Memory> = s.memory_search(q).await.unwrap().into_iter().filter(|m| m.project.is_none()).collect();
    let prefs =
        globals.iter().filter(|m| m.kind == MemoryKind::Preference).map(|m| m.content.chars().count() + 16).sum();
    (globals.len(), prefs)
}

fn own<'a>(b: &'a brief::Brief, p: &str) -> Vec<&'a Memory> {
    b.memories.iter().filter(|m| m.project.as_deref() == Some(p)).collect()
}

#[tokio::test]
async fn brief_prefers_memories_relevant_to_the_checkpoint() {
    for (name, s) in stores().await {
        let p = project();
        seed_many(&s, &p).await;
        s.checkpoint_save(goal(&p, "finish the zebra migration")).await.unwrap();
        let (globals, room) = visible_globals(&s).await;

        let b = brief::build_with_budget(&s, &p, None, None, None, 500 + room).await.unwrap();
        for needle in ["Zebra migration must keep ids stable", "Run zebra migration dry first"] {
            assert!(b.markdown.contains(needle), "{name}: missing {needle}:\n{}", b.markdown);
        }
        assert!(b.markdown.contains("more memories not shown: use the memory_search tool"), "{name}");
        assert!(own(&b, &p).len() < 30, "{name}");
        let omitted = 30 + globals - b.memories.len();
        assert!(b.markdown.contains(&format!("_{omitted} more memories not shown")), "{name}:\n{}", b.markdown);

        // the latest prompt counts when newer than the checkpoint
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        s.event_append(NewEvent {
            project: p.clone(),
            kind: "prompt".into(),
            content: "look at filler7".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        let b = brief::build_with_budget(&s, &p, None, None, None, 500 + room).await.unwrap();
        assert!(b.memories.iter().any(|m| m.content.starts_with("filler7")), "{name}: prompt ignored");
    }
}

#[tokio::test]
async fn brief_always_keeps_preferences_and_respects_budget() {
    for (name, s) in stores().await {
        let p = project();
        seed_many(&s, &p).await;
        let pref = NewMemory {
            kind: MemoryKind::Preference,
            ..mem(&Some(p.clone()), &format!("prefers tabs {}", "wide ".repeat(40)), &[])
        };
        s.memory_save(pref).await.unwrap();
        s.checkpoint_save(goal(&p, "finish the zebra migration")).await.unwrap();

        // the preference alone is bigger than the budget: it is still shown
        let b = brief::build_with_budget(&s, &p, None, None, None, 100).await.unwrap();
        assert!(b.memories.iter().any(|m| m.kind == MemoryKind::Preference), "{name}");
        assert_eq!(own(&b, &p).len(), 1, "{name}: nothing else fits");

        let b = brief::build_with_budget(&s, &p, None, None, None, 1_000).await.unwrap();
        let used: usize =
            own(&b, &p).iter().filter(|m| m.kind != MemoryKind::Preference).map(|m| m.content.len()).sum();
        assert!(used <= 1_000, "{name}: budget exceeded");
        assert!(b.markdown.contains("more memories not shown"), "{name}");
    }
}

#[tokio::test]
async fn brief_without_focus_lists_recent_memories() {
    for (name, s) in stores().await {
        let p = project();
        seed_many(&s, &p).await;
        // no checkpoint, no prompt: most recent first, nothing reordered, all fit a big budget
        let b = brief::build_with_budget(&s, &p, None, None, None, 1_000_000).await.unwrap();
        let mine = own(&b, &p);
        assert_eq!(mine.len(), 30, "{name}");
        assert!(mine[0].content.starts_with("filler27"), "{name}");
        assert!(mine[29].content.starts_with("Zebra migration"), "{name}");
        assert!(!b.markdown.contains("more memories not shown"), "{name}");
    }
}

// ---------------------------------------------------------------- delegated runs

fn tasks(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

fn step<'a>(run: &'a RunWithSteps, key: &str) -> &'a Step {
    run.steps.iter().find(|s| s.key == key).unwrap_or_else(|| panic!("no step {key}"))
}

#[tokio::test]
async fn delegate_creates_pending_steps_and_reuse_adds_missing_tasks() {
    for (name, s) in stores().await {
        let p = project();
        let run = s.run_delegate(&p, "release", tasks(&["install", " docs ", "install", ""]), None).await.unwrap();
        assert_eq!(run.run.status, RunStatus::Running, "{name}");
        assert_eq!(run.steps.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(), vec!["install", "docs"], "{name}");
        assert!(run.steps.iter().all(|s| s.status == StepStatus::Pending && s.owner.is_none()), "{name}");
        assert!(run.steps.iter().all(|s| s.lease_until.is_none() && !s.stale && s.attempts == 0), "{name}");
        assert_eq!(run.steps.iter().map(|s| s.seq).collect::<Vec<_>>(), vec![1, 2], "{name}");
        assert!(!run.run.stale, "{name}: a delegated run has no run-level lease to expire");

        // same name while running: the same run, with the missing task added; existing steps untouched
        s.step_claim(&run.run.id, "install", "w1", 600, false).await.unwrap();
        let again = s.run_delegate(&p, "release", tasks(&["docs", "tests"]), None).await.unwrap();
        assert_eq!(again.run.id, run.run.id, "{name}");
        assert_eq!(again.steps.len(), 3, "{name}");
        assert_eq!(step(&again, "tests").status, StepStatus::Pending, "{name}");
        assert_eq!(step(&again, "install").status, StepStatus::Running, "{name}");

        // an idempotency key finds the run whatever its name
        let keyed = s.run_delegate(&p, "first", tasks(&["a"]), Some(format!("key-{p}"))).await.unwrap();
        let keyed2 = s.run_delegate(&p, "renamed", tasks(&["a", "b"]), Some(format!("key-{p}"))).await.unwrap();
        assert_eq!((keyed2.run.id.clone(), keyed2.steps.len()), (keyed.run.id.clone(), 2), "{name}");

        // a finished run isn't reused by name
        s.run_finish(&run.run.id, RunOutcome::Completed, None, None).await.unwrap();
        let fresh = s.run_delegate(&p, "release", tasks(&["install"]), None).await.unwrap();
        assert_ne!(fresh.run.id, run.run.id, "{name}");

        assert!(matches!(s.run_delegate(&p, "empty", tasks(&[" "]), None).await, Err(Error::Invalid(_))), "{name}");
        // delegated runs don't show up as a run to take over
        let b = brief::build(&s, &p, None, None, None).await.unwrap();
        assert!(b.active_runs.is_empty(), "{name}");
    }
}

#[tokio::test]
async fn claim_hold_force_stale_takeover_and_done() {
    for (name, s) in stores().await {
        let p = project();
        let run = s.run_delegate(&p, "release", tasks(&["a", "b"]), None).await.unwrap();
        let id = &run.run.id;

        // pending → running under our lease
        match s.step_claim(id, "a", "w1", 600, false).await.unwrap() {
            ClaimResult::Claimed { attempt: 1, previous_owner: None, stale: false, previous_error: None } => {}
            c => panic!("{name}: {c:?}"),
        }
        let st = step(&s.run_get(id).await.unwrap(), "a").clone();
        assert_eq!((st.status, st.owner.as_deref(), st.attempts), (StepStatus::Running, Some("w1"), 1), "{name}");
        assert!(st.lease_until.unwrap() > now_ms() && !st.stale, "{name}");

        // renewing our own claim isn't a new attempt
        match s.step_claim(id, "a", "w1", 600, false).await.unwrap() {
            ClaimResult::Claimed { attempt: 1, previous_owner: None, .. } => {}
            c => panic!("{name}: {c:?}"),
        }

        // a second worker sees the live lease
        match s.step_claim(id, "a", "w2", 600, false).await.unwrap() {
            ClaimResult::HeldBy { owner, lease_until } => {
                assert_eq!(owner, "w1", "{name}");
                assert!(lease_until > now_ms(), "{name}");
            }
            c => panic!("{name}: {c:?}"),
        }
        assert_eq!(step(&s.run_get(id).await.unwrap(), "a").owner.as_deref(), Some("w1"), "{name}: unchanged");

        // force takes it anyway
        match s.step_claim(id, "a", "w2", 600, true).await.unwrap() {
            ClaimResult::Claimed { attempt: 2, previous_owner: Some(p), stale: false, .. } => assert_eq!(p, "w1"),
            c => panic!("{name}: {c:?}"),
        }
        assert!(matches!(s.step_claim(id, "a", "w1", 600, false).await.unwrap(), ClaimResult::HeldBy { .. }), "{name}");

        // an expired lease makes the step stale, and a replacement takes over
        s.step_claim(id, "b", "w1", 0, false).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let stale = step(&s.run_get(id).await.unwrap(), "b").clone();
        assert!(stale.stale && stale.status == StepStatus::Running, "{name}");
        assert!(!step(&s.run_get(id).await.unwrap(), "a").stale, "{name}");
        match s.step_claim(id, "b", "w3", 600, false).await.unwrap() {
            ClaimResult::Claimed { attempt: 2, previous_owner: Some(p), stale: true, .. } => assert_eq!(p, "w1"),
            c => panic!("{name}: {c:?}"),
        }
        let st = step(&s.run_get(id).await.unwrap(), "b").clone();
        assert_eq!((st.owner.as_deref(), st.stale), (Some("w3"), false), "{name}");

        // completed: never redone; the recorded output comes back
        s.step_complete(id, "a", Some(json!("installers built"))).await.unwrap();
        for who in ["w2", "w4"] {
            for force in [false, true] {
                match s.step_claim(id, "a", who, 600, force).await.unwrap() {
                    ClaimResult::AlreadyDone { output } => {
                        assert_eq!(output, Some(json!("installers built")), "{name}")
                    }
                    c => panic!("{name}: {c:?}"),
                }
            }
        }
        let done = step(&s.run_get(id).await.unwrap(), "a").clone();
        assert_eq!(
            (done.status, done.owner.as_deref(), done.lease_until),
            (StepStatus::Completed, Some("w2"), None),
            "{name}"
        );

        assert!(matches!(s.step_claim(id, "nope", "w1", 60, false).await, Err(Error::NotFound(_))), "{name}");
        s.run_finish(id, RunOutcome::Cancelled, None, None).await.unwrap();
        assert!(matches!(s.step_claim(id, "b", "w5", 60, false).await, Err(Error::Conflict(_))), "{name}");
        assert!(
            matches!(s.step_claim(id, "a", "w5", 60, false).await.unwrap(), ClaimResult::AlreadyDone { .. }),
            "{name}"
        );
    }
}

#[tokio::test]
async fn heartbeat_extends_only_for_the_owner_and_complete_or_fail_clear_the_lease() {
    for (name, s) in stores().await {
        let p = project();
        let run = s.run_delegate(&p, "release", tasks(&["a", "b"]), None).await.unwrap();
        let id = &run.run.id;

        // nothing to renew before a claim; strangers can't renew
        assert!(!s.step_heartbeat(id, "a", "w1", 600).await.unwrap(), "{name}: pending");
        s.step_claim(id, "a", "w1", 10, false).await.unwrap();
        let before = step(&s.run_get(id).await.unwrap(), "a").lease_until.unwrap();
        assert!(!s.step_heartbeat(id, "a", "w2", 3600).await.unwrap(), "{name}: not the owner");
        assert_eq!(step(&s.run_get(id).await.unwrap(), "a").lease_until.unwrap(), before, "{name}");
        assert!(s.step_heartbeat(id, "a", "w1", 3600).await.unwrap(), "{name}");
        assert!(step(&s.run_get(id).await.unwrap(), "a").lease_until.unwrap() > before + 3_000_000, "{name}");

        // a taken-over worker's heartbeat no longer counts
        s.step_claim(id, "a", "w2", 600, true).await.unwrap();
        assert!(!s.step_heartbeat(id, "a", "w1", 3600).await.unwrap(), "{name}");

        // complete and fail release the lease (the owner stays as "done by")
        s.step_complete(id, "a", None).await.unwrap();
        let a = step(&s.run_get(id).await.unwrap(), "a").clone();
        assert_eq!((a.status, a.lease_until, a.stale), (StepStatus::Completed, None, false), "{name}");
        assert!(!s.step_heartbeat(id, "a", "w2", 600).await.unwrap(), "{name}: finished");

        s.step_claim(id, "b", "w1", 600, false).await.unwrap();
        s.step_fail(id, "b", "no network").await.unwrap();
        let b = step(&s.run_get(id).await.unwrap(), "b").clone();
        assert_eq!(
            (b.status, b.lease_until, b.error.as_deref()),
            (StepStatus::Failed, None, Some("no network")),
            "{name}"
        );

        // a failed task can be claimed again; the notice carries why the last attempt failed
        match s.step_claim(id, "b", "w2", 600, false).await.unwrap() {
            ClaimResult::Claimed { attempt: 2, previous_owner: None, previous_error: Some(e), .. } => {
                assert_eq!(e, "no network", "{name}")
            }
            c => panic!("{name}: {c:?}"),
        }
    }
}

#[tokio::test]
async fn coordinator_brief_shows_each_tasks_step_status() {
    for (name, s) in stores().await {
        let p = project();
        let run = s.run_delegate(&p, "windows release", tasks(&["a", "b", "c", "d", "e"]), None).await.unwrap();
        let id = &run.run.id;
        s.step_claim(id, "a", "w-live", 600, false).await.unwrap();
        s.step_claim(id, "b", "w-gone", 0, false).await.unwrap();
        s.step_claim(id, "c", "w3", 600, false).await.unwrap();
        s.step_complete(id, "c", Some(json!("built the installers"))).await.unwrap();
        s.step_claim(id, "d", "w4", 600, false).await.unwrap();
        s.step_fail(id, "d", "no signing key").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let md = brief::build(&s, &p, None, None, None).await.unwrap().markdown;
        assert!(md.contains("## Delegated work"), "{name}\n{md}");
        assert!(md.contains("- **a**: running by w-live"), "{name}\n{md}");
        assert!(md.contains("- **b**: stale (owner w-gone, lease expired "), "{name}\n{md}");
        assert!(md.contains("- **c**: done\n  - result: built the installers"), "{name}\n{md}");
        assert!(md.contains("- **d**: failed: no signing key"), "{name}\n{md}");
        assert!(md.contains("- **e**: pending"), "{name}\n{md}");
        assert!(!md.contains("### windows release"), "{name}: a single run gets no sub-heading\n{md}");
        assert!(!md.contains("Unfinished durable runs"), "{name}\n{md}");

        // a second run: grouped under the run names
        s.run_delegate(&p, "docs sprint", tasks(&["x"]), None).await.unwrap();
        let md = brief::build(&s, &p, None, None, None).await.unwrap().markdown;
        assert!(md.contains("### docs sprint\n- **x**: pending"), "{name}\n{md}");
        assert!(md.contains("### windows release\n- **a**: running by w-live"), "{name}\n{md}");
        assert!(md.find("### docs sprint").unwrap() < md.find("### windows release").unwrap(), "{name}\n{md}");

        // a worker's own brief has no coordinator section
        let w = brief::build(&s, &p, None, None, Some("a")).await.unwrap();
        assert!(w.delegated.is_empty() && !w.markdown.contains("Delegated work"), "{name}");
    }
}

#[tokio::test]
async fn review_groups_related_memories_of_same_kind_and_scope() {
    for (name, s) in stores().await {
        let p = Some(project());
        let save = |kind: MemoryKind, project: &Option<String>, content: &str| {
            s.memory_save(NewMemory { kind, ..mem(project, content, &[]) })
        };
        let a = save(MemoryKind::Fact, &p, "Payments webhook retries use exponential backoff").await.unwrap();
        let b = save(MemoryKind::Fact, &p, "Payments webhook retries use capped backoff delays").await.unwrap();
        assert_ne!(a.id, b.id, "{name}: related, not near-duplicate");
        // same words but another kind: not grouped
        let other_kind =
            save(MemoryKind::Note, &p, "Payments webhook retries use jittered backoff delays").await.unwrap();
        // unrelated
        let c = save(MemoryKind::Fact, &p, "Database migrations run at startup").await.unwrap();

        let r = s.memory_review(p.as_deref().unwrap(), None).await.unwrap();
        let groups: Vec<Vec<&str>> = r
            .related
            .iter()
            .map(|g| g.iter().map(|m| m.id.as_str()).collect())
            .filter(|g: &Vec<&str>| g.contains(&a.id.as_str()))
            .collect();
        assert_eq!(groups.len(), 1, "{name}");
        assert!(groups[0].contains(&b.id.as_str()) && groups[0].len() == 2, "{name}: {groups:?}");
        let grouped: Vec<&str> = r.related.iter().flatten().map(|m| m.id.as_str()).collect();
        assert!(!grouped.contains(&other_kind.id.as_str()) && !grouped.contains(&c.id.as_str()), "{name}");

        let md = r.markdown(p.as_deref().unwrap(), now_ms());
        assert!(md.contains(&a.id) && md.contains("Related memories"), "{name}");
    }
}

#[tokio::test]
async fn review_flags_stale_memories_and_old_todos() {
    let pause = || tokio::time::sleep(std::time::Duration::from_millis(5));
    for (name, s) in stores().await {
        let p = project();
        let stale = s.memory_save(mem(&Some(p.clone()), "Auth lives in src/auth/jwt.rs", &[])).await.unwrap();
        let todo = s
            .memory_save(NewMemory { kind: MemoryKind::Todo, ..mem(&Some(p.clone()), "Rotate the signing keys", &[]) })
            .await
            .unwrap();
        pause().await;
        s.event_append(NewEvent {
            project: p.clone(),
            kind: "file_change".into(),
            content: "/repo/src/auth/jwt.rs".into(),
            ..Default::default()
        })
        .await
        .unwrap();

        let r = s.memory_review(&p, None).await.unwrap();
        assert_eq!(r.stale.len(), 1, "{name}");
        assert_eq!((r.stale[0].memory.id.as_str(), r.stale[0].path.as_str()), (stale.id.as_str(), "src/auth/jwt.rs"));
        assert!(r.old.is_empty(), "{name}: a fresh todo is not old");

        // 29 days later the todo is still fine, 31 days later it is old
        let day = 86_400_000;
        let r = s.memory_review_at(&p, None, todo.updated_at + 29 * day).await.unwrap();
        assert!(r.old.is_empty(), "{name}");
        let r = s.memory_review_at(&p, None, todo.updated_at + 31 * day).await.unwrap();
        assert_eq!(r.old.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec![todo.id.as_str()], "{name}");
        // only todos get old
        assert!(r.old.iter().all(|m| m.kind == MemoryKind::Todo), "{name}");
    }
}

#[tokio::test]
async fn brief_nudges_to_review_from_three_flagged_memories() {
    let pause = || tokio::time::sleep(std::time::Duration::from_millis(5));
    const NUDGE: &str = "may need consolidation: run the memory_review tool";
    for (name, s) in stores().await {
        let p = project();
        for text in [
            "Billing totals are computed in src/a_mod.rs",
            "Cache eviction follows an LRU policy, see src/b_mod.rs",
            "Webhook signatures get verified by src/c_mod.rs",
        ] {
            s.memory_save(mem(&Some(p.clone()), text, &[])).await.unwrap();
        }
        pause().await;
        let change = |f: &str| NewEvent {
            project: p.clone(),
            kind: "file_change".into(),
            content: format!("src/{f}_mod.rs"),
            ..Default::default()
        };
        // Postgres tests share one database: other tests' global memories can be flagged too, so
        // count relative to what is flagged before this test's files change
        let flagged = |md: &str| -> usize {
            md.lines()
                .find(|l| l.contains(NUDGE))
                .and_then(|l| l.trim_start_matches('_').split_whitespace().next()?.parse().ok())
                .unwrap_or(0)
        };
        let base = flagged(&brief::build(&s, &p, None, None, None).await.unwrap().markdown);
        s.event_append(change("a")).await.unwrap();
        s.event_append(change("b")).await.unwrap();
        let two = brief::build(&s, &p, None, None, None).await.unwrap();
        if base == 0 {
            assert!(!two.markdown.contains(NUDGE), "{name}: only 2 flagged\n{}", two.markdown);
        }
        s.event_append(change("c")).await.unwrap();
        let three = brief::build(&s, &p, None, None, None).await.unwrap();
        assert!(flagged(&three.markdown) >= base + 3, "{name}\n{}", three.markdown);
    }
}
