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
        "UPDATE memdusa_meta SET value = '3' WHERE key = 'schema_version'",
        "INSERT INTO memories (id, project, kind, content, tags, created_at, updated_at) \
         VALUES ('legacy-1', 'PROJECT', 'note', 'legacy cache invalidation notes', '[\"perf\"]', 1, 1)",
    ] {
        let stmt = stmt.replace("PROJECT", &p);
        sqlx::query(sqlx::AssertSqlSafe(stmt)).execute(&raw).await.unwrap();
    }
    raw.close().await;

    let s = Store::connect(&url).await.unwrap();
    assert_eq!(s.migrate().await.unwrap(), 4);
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

        let b = brief::build(&s, &p, None, None).await.unwrap();
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

        let b = brief::build_with_budget(&s, &p, None, None, 500 + room).await.unwrap();
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
        let b = brief::build_with_budget(&s, &p, None, None, 500 + room).await.unwrap();
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
        let b = brief::build_with_budget(&s, &p, None, None, 100).await.unwrap();
        assert!(b.memories.iter().any(|m| m.kind == MemoryKind::Preference), "{name}");
        assert_eq!(own(&b, &p).len(), 1, "{name}: nothing else fits");

        let b = brief::build_with_budget(&s, &p, None, None, 1_000).await.unwrap();
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
        let b = brief::build_with_budget(&s, &p, None, None, 1_000_000).await.unwrap();
        let mine = own(&b, &p);
        assert_eq!(mine.len(), 30, "{name}");
        assert!(mine[0].content.starts_with("filler27"), "{name}");
        assert!(mine[29].content.starts_with("Zebra migration"), "{name}");
        assert!(!b.markdown.contains("more memories not shown"), "{name}");
    }
}
