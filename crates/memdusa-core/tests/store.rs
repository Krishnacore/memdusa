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
        Store::connect(&format!("sqlite://{}", dir.join("t.db").display())).await.unwrap(),
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
        let empty = brief::build(&s, &p, None).await.unwrap();
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

        let b = brief::build(&s, &p, None).await.unwrap();
        assert_eq!(b.checkpoints.len(), 1, "{name}: only latest checkpoint per task");
        assert_eq!(b.checkpoints[0].goal, "ship v2", "{name}");
        for needle in ["ship v2", "SQLite for local mode", "commit", "refactor"] {
            assert!(b.markdown.contains(needle), "{name}: brief missing {needle}:\n{}", b.markdown);
        }
    }
}
