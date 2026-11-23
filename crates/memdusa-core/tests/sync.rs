//! Sync primitives: change tracking, LWW, tombstones, pagination, prune safety.
//! Run against SQLite always, and against Postgres when
//! `MEMDUSA_TEST_POSTGRES_URL` is set.

use memdusa_core::model::*;
use memdusa_core::retention::Cutoffs;
use memdusa_core::store::Store;
use memdusa_core::sync::{SyncActor, SyncKind, SyncOp, SyncRecord};
use serde_json::to_value;

async fn stores() -> Vec<(String, Store, String)> {
    let dir = std::env::temp_dir().join(format!("memdusa-sync-test-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = memdusa_core::store::sqlite_url(&dir.join("t.db"));
    let mut out = vec![("sqlite".to_string(), Store::connect(&url).await.unwrap(), url)];
    if let Ok(url) = std::env::var("MEMDUSA_TEST_POSTGRES_URL") {
        out.push(("postgres".into(), Store::connect(&url).await.unwrap(), url));
    }
    out
}

fn project() -> String {
    format!("sync/{}", new_id())
}

fn mem(id: &str, project: &str, content: &str, at: Millis) -> SyncRecord {
    SyncRecord {
        kind: SyncKind::Memory,
        op: SyncOp::Upsert,
        record_id: id.into(),
        record: Some(
            to_value(Memory {
                id: id.into(),
                project: Some(project.into()),
                kind: MemoryKind::Fact,
                content: content.into(),
                tags: vec![],
                source: Some("codex".into()),
                user: None,
                created_at: at,
                updated_at: at,
                merged: false,
            })
            .unwrap(),
        ),
        at: Some(at),
        origin: None,
    }
}

fn del(kind: SyncKind, id: &str, _project: &str, at: Millis) -> SyncRecord {
    SyncRecord { kind, op: SyncOp::Delete, record_id: id.into(), record: None, at: Some(at), origin: None }
}

async fn content(s: &Store, project: &str) -> Vec<String> {
    // Searches always include globals; on a shared Postgres database those
    // belong to other tests, so keep only this project's rows.
    s.memory_search(MemoryQuery { project: Some(project.into()), limit: Some(100), ..Default::default() })
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.project.as_deref() == Some(project))
        .map(|m| m.content)
        .collect()
}

#[tokio::test]
async fn every_data_write_appends_a_change() {
    for (name, s, _raw_url) in stores().await {
        let p = project();
        let m = s
            .memory_save(NewMemory {
                project: Some(p.clone()),
                content: "unique alpha beta gamma".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        s.memory_update(&m.id, MemoryPatch { content: Some("unique alpha beta delta".into()), ..Default::default() })
            .await
            .unwrap();
        // Identical content refreshes the existing memory; that is a write too.
        s.memory_save(NewMemory {
            project: Some(p.clone()),
            content: "unique alpha beta delta".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        s.event_append(NewEvent { project: p.clone(), kind: "note".into(), content: "e".into(), ..Default::default() })
            .await
            .unwrap();
        s.checkpoint_save(NewCheckpoint { project: p.clone(), goal: "g".into(), ..Default::default() }).await.unwrap();
        s.memory_forget(&m.id).await.unwrap();

        assert_eq!(s.sync_pending(Some(&p), 0).await.unwrap(), 6, "{name}: every write is logged");
        let (changes, last) = s.sync_local_changes(Some(&p), 0, 500).await.unwrap();
        // The memory was deleted last: its earlier upserts are skipped in the
        // push stream (no row to inline) but the tombstone still travels, and
        // the cursor spans all six changes.
        assert_eq!(changes.len(), 3, "{name}: {changes:?}");
        assert!(last >= 6, "{name}: cursor spans skipped changes too");
        assert!(changes.iter().any(|c| c.kind == SyncKind::Memory && c.op == SyncOp::Delete));
        assert_eq!(changes.iter().filter(|c| c.kind == SyncKind::Event).count(), 1);
        assert_eq!(changes.iter().filter(|c| c.kind == SyncKind::Checkpoint).count(), 1);
        // The forget is a tombstone: no inline record, but its time travels.
        let tomb = changes.iter().find(|c| c.op == SyncOp::Delete).unwrap();
        assert!(tomb.record.is_none() && tomb.at.is_some());
    }
}

#[tokio::test]
async fn lww_winner_everywhere_and_loser_in_revisions() {
    for (name, s, _raw_url) in stores().await {
        let p = project();
        let id = new_id();
        let old = mem(&id, &p, "old wording", 100);
        let new = mem(&id, &p, "new wording", 200);

        let applied = s.sync_apply(Some(&p), std::slice::from_ref(&old), &SyncActor::Local, None).await.unwrap();
        assert_eq!(applied[0].outcome, "applied", "{name}");
        let applied = s.sync_apply(Some(&p), std::slice::from_ref(&new), &SyncActor::Local, None).await.unwrap();
        assert_eq!(applied[0].outcome, "applied", "{name}");
        assert_eq!(content(&s, &p).await, vec!["new wording"], "{name}");

        let revs = s.memory_revisions(&id).await.unwrap();
        assert_eq!(revs.len(), 1, "{name}");
        assert_eq!(revs[0].content, "old wording", "{name}");

        // Re-pushing the loser is idempotent: it stays a revision, no new change.
        let before = s.sync_local_changes(Some(&p), 0, 500).await.unwrap().0.len();
        let res = s.sync_apply(Some(&p), &[old], &SyncActor::Local, None).await.unwrap();
        assert_eq!(res[0].outcome, "revision", "{name}");
        assert_eq!(content(&s, &p).await, vec!["new wording"], "{name}");
        let after = s.sync_local_changes(Some(&p), 0, 500).await.unwrap().0.len();
        assert_eq!(before, after, "{name}: a losing version writes no change");
    }
}

#[tokio::test]
async fn delete_wins_unless_the_edit_is_newer() {
    for (name, s, _raw_url) in stores().await {
        let p = project();
        let id = new_id();
        s.sync_apply(Some(&p), &[mem(&id, &p, "v1", 100)], &SyncActor::Local, None).await.unwrap();

        // Delete at 200 removes the record...
        let res =
            s.sync_apply(Some(&p), &[del(SyncKind::Memory, &id, &p, 200)], &SyncActor::Local, None).await.unwrap();
        assert_eq!(res[0].outcome, "applied", "{name}");
        assert!(content(&s, &p).await.is_empty(), "{name}");

        // ...and stops an older offline edit from resurrecting it.
        let res = s.sync_apply(Some(&p), &[mem(&id, &p, "stale v0", 150)], &SyncActor::Local, None).await.unwrap();
        assert_eq!(res[0].outcome, "skipped_deleted", "{name}");
        assert!(content(&s, &p).await.is_empty(), "{name}");

        // A genuinely newer edit wins over the tombstone.
        s.sync_apply(Some(&p), &[mem(&id, &p, "v2", 300)], &SyncActor::Local, None).await.unwrap();
        assert_eq!(content(&s, &p).await, vec!["v2"], "{name}");

        // An old tombstone cannot delete a record edited after it.
        let res =
            s.sync_apply(Some(&p), &[del(SyncKind::Memory, &id, &p, 250)], &SyncActor::Local, None).await.unwrap();
        assert_eq!(res[0].outcome, "kept_newer", "{name}");
        assert_eq!(content(&s, &p).await, vec!["v2"], "{name}");
    }
}

#[tokio::test]
async fn pull_paginates_and_advances_the_cursor() {
    for (name, s, _raw_url) in stores().await {
        let p = project();
        for i in 0..5 {
            s.event_append(NewEvent {
                project: p.clone(),
                kind: "note".into(),
                content: format!("event {i}"),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let mut after = 0;
        let mut seen = vec![];
        let mut pages = 0;
        loop {
            let before = after;
            let page = s.sync_pull(Some(&p), after, 2, &Identity::root()).await.unwrap();
            pages += 1;
            assert!(page.next_cursor > before, "{name}: cursor did not advance");
            seen.extend(page.changes.iter().map(|c| c.record_id.clone()));
            after = page.next_cursor;
            if !page.has_more {
                break;
            }
            assert!(pages < 10, "{name}: pagination did not terminate");
        }
        assert_eq!(seen.len(), 5, "{name}");
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 5, "{name}: no duplicates across pages");
        assert!(pages >= 3, "{name}: limit of 2 was respected");
        let last = s.sync_pull(Some(&p), after, 10, &Identity::root()).await.unwrap();
        assert!(last.changes.is_empty() && !last.has_more, "{name}: cursor is settled");
    }
}

#[tokio::test]
async fn pruning_never_resurrects_rows_on_a_fresh_pull() {
    for (name, s, _raw_url) in stores().await {
        let p = project();
        let mut ids = vec![];
        for i in 0..3 {
            let e = s
                .event_append(NewEvent {
                    project: p.clone(),
                    kind: "file_change".into(),
                    content: format!("file {i}"),
                    created_at: Some(1_000 + i),
                    ..Default::default()
                })
                .await
                .unwrap();
            ids.push(e.id);
        }
        // Force the retention window past all three events.
        s.prune_older_than(Cutoffs { activity: Some(now_ms() + 1000), ..Default::default() }, Some(&p), false)
            .await
            .unwrap();
        // A peer pulling from zero sees the summary, never the pruned events
        // (their upsert changes have no row left to inline).
        let page = s.sync_pull(Some(&p), 0, 500, &Identity::root()).await.unwrap();
        for c in &page.changes {
            assert!(!ids.contains(&c.record_id), "{name}: pruned event came back: {c:?}");
        }
        assert!(page.changes.iter().any(|c| c.record.is_some()), "{name}: the session summary should still sync");
    }
}

#[tokio::test]
async fn sync_state_and_initial_seeding() {
    for (name, s, raw_url) in stores().await {
        let p = project();
        s.memory_save(NewMemory { project: Some(p.clone()), content: "seed me".into(), ..Default::default() })
            .await
            .unwrap();
        s.event_append(NewEvent {
            project: p.clone(),
            kind: "note".into(),
            content: "seed".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        s.checkpoint_save(NewCheckpoint { project: p.clone(), goal: "seed".into(), ..Default::default() })
            .await
            .unwrap();

        // Simulate a database whose rows predate the change log (migration
        // v11 doesn't backfill changes): drop just this project's changes.
        sqlx::any::install_default_drivers();
        let raw_conn_url = if name == "sqlite" { format!("{raw_url}?mode=rwc") } else { raw_url.clone() };
        let raw = sqlx::AnyPool::connect(&raw_conn_url).await.unwrap();
        sqlx::query("DELETE FROM changes WHERE project = $1").bind(p.clone()).execute(&raw).await.unwrap();
        raw.close().await;
        assert_eq!(s.sync_seed(Some(&p)).await.unwrap(), 3, "{name}");
        assert_eq!(s.sync_seed(Some(&p)).await.unwrap(), 0, "{name}: idempotent");

        let st = s.sync_state_add("https://team.example", &p, "client-1").await.unwrap();
        assert_eq!((st.push_after, st.pull_after, st.origin.as_str()), (0, 0, "client-1"));
        let (_, last) = s.sync_local_changes(Some(&p), 0, 500).await.unwrap();
        assert!(last >= 3, "{name}: cursor is monotonic even after rows are removed");
        s.sync_state_update("https://team.example", &p, Some(last), Some(last + 2), None, false).await.unwrap();
        let st = s.sync_state_get("https://team.example", &p).await.unwrap().unwrap();
        assert_eq!((st.push_after, st.pull_after), (last, last + 2));
        assert_eq!(s.sync_pending(Some(&p), st.push_after).await.unwrap(), 0, "{name}");
        assert_eq!(s.sync_pending(Some(&p), 0).await.unwrap(), 3, "{name}");

        s.sync_state_update("https://team.example", &p, None, None, Some("boom"), false).await.unwrap();
        assert_eq!(
            s.sync_state_get("https://team.example", &p).await.unwrap().unwrap().last_error.as_deref(),
            Some("boom")
        );
        s.sync_state_update("https://team.example", &p, None, None, None, true).await.unwrap();
        let st = s.sync_state_get("https://team.example", &p).await.unwrap().unwrap();
        assert!(st.last_ok_at.is_some() && st.last_error.is_none(), "{name}");

        assert_eq!(s.sync_state_list("https://team.example").await.unwrap().len(), 1);
        s.sync_state_remove("https://team.example", &p).await.unwrap();
        assert!(s.sync_state_get("https://team.example", &p).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn attribution_and_project_mismatch_are_rejected() {
    for (name, s, _raw_url) in stores().await {
        let p = project();
        let other = project();
        let alice = Identity { user: Some("alice".into()), admin: false };

        // A record from a different project can't be pushed into this one.
        let foreign = mem(&new_id(), &other, "elsewhere", 1);
        assert!(s.sync_apply(Some(&p), &[foreign], &SyncActor::Server(alice.clone()), None).await.is_err(), "{name}");

        // Neither can a record attributed to another user.
        let id = new_id();
        let mut r = mem(&id, &p, "bob's", 1);
        r.record.as_mut().unwrap().as_object_mut().unwrap().insert("user".into(), to_value("bob").unwrap());
        assert!(s.sync_apply(Some(&p), &[r], &SyncActor::Server(alice.clone()), None).await.is_err(), "{name}");

        // Nor can a tombstone in project `p` delete a record in `other`.
        let victim = new_id();
        s.sync_apply(Some(&other), &[mem(&victim, &other, "victim", 1)], &SyncActor::Local, None).await.unwrap();
        let cross_delete = memdusa_core::sync::SyncRecord {
            kind: SyncKind::Memory,
            op: SyncOp::Delete,
            record_id: victim.clone(),
            record: None,
            at: Some(500),
            origin: None,
        };
        assert!(
            s.sync_apply(Some(&p), &[cross_delete], &SyncActor::Server(alice.clone()), None).await.is_err(),
            "{name}"
        );
        assert_eq!(content(&s, &other).await, vec!["victim"], "{name}: victim still there");
        assert!(content(&s, &p).await.is_empty(), "{name}: no partial writes on rejection");
    }
}

/// Regression: a pull used to move the push cursor to the newest local change, so an edit another
/// process made between the push and the pull of a pass was never pushed. Pulled changes are now
/// tagged PULLED_ORIGIN and skipped by push instead; every other local change is still pushed.
#[tokio::test]
async fn pulled_changes_are_skipped_but_concurrent_local_edits_still_push() {
    use memdusa_core::sync::PULLED_ORIGIN;
    for (name, s, _) in stores().await {
        let p = project();
        let before = s.sync_last_seq().await.unwrap();
        // a pulled memory, applied locally
        let pulled = mem("01a20000-0000-7000-8000-00000000aaaa", &p, "pulled from the server", 1_700_000_000_000);
        s.sync_apply(Some(&p), &[pulled], &SyncActor::Local, Some(PULLED_ORIGIN)).await.unwrap();
        // meanwhile another process saves a memory locally
        let local = s
            .memory_save(NewMemory {
                project: Some(p.clone()),
                kind: MemoryKind::Fact,
                content: "written by a hook during the sync pass".into(),
                tags: vec![],
                source: Some("hook".into()),
                user: None,
            })
            .await
            .unwrap();
        let (changes, last) = s.sync_local_changes(Some(&p), before, 500).await.unwrap();
        let ids: Vec<&str> = changes.iter().map(|c| c.record_id.as_str()).collect();
        assert_eq!(ids, vec![local.id.as_str()], "{name}: only the local edit is pushed");
        assert!(last > before, "{name}: the cursor still moves past the pulled change");
    }
}
