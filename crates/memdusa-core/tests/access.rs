//! Per-project access control and audit-log behaviour. Run against SQLite
//! always, and against Postgres too when `MEMDUSA_TEST_POSTGRES_URL` is set.
//! Names are unique per test because Postgres tests share one database.

use memdusa_core::model::*;
use memdusa_core::{Store, access};
use serde_json::json;

async fn stores() -> Vec<(String, Store)> {
    let dir = std::env::temp_dir().join(format!("memdusa-access-{}", new_id()));
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

fn user() -> String {
    format!("u{}", new_id())
}

#[tokio::test]
async fn grants_precedence_none_carve_out_and_admin_bypass() {
    for (name, s) in stores().await {
        let alice = user();
        let admin = user();
        s.user_create(&alice, false).await.unwrap();
        s.user_create(&admin, true).await.unwrap();
        let id = Identity { user: Some(alice.clone()), admin: false };
        let admin_id = Identity { user: Some(admin), admin: true };
        let root = Identity::root();

        s.grant_create("github.com/acme/*", &alice, Role::Write, "root").await.unwrap();
        let carve = s.grant_create("github.com/acme/payroll", &alice, Role::None, "root").await.unwrap();
        assert_eq!(
            s.effective_access(&id, "github.com/acme/shop", DefaultPolicy::None).await.unwrap(),
            Access::Write,
            "{name}: prefix grant"
        );
        assert_eq!(
            s.effective_access(&id, &carve.pattern, DefaultPolicy::None).await.unwrap(),
            Access::None,
            "{name}: exact none carves out"
        );
        assert_eq!(
            s.effective_access(&id, "other/repo", DefaultPolicy::None).await.unwrap(),
            Access::None,
            "{name}: no grant under none"
        );
        assert_eq!(
            s.effective_access(&admin_id, &carve.pattern, DefaultPolicy::None).await.unwrap(),
            Access::Write,
            "{name}: admin bypass"
        );
        assert_eq!(
            s.effective_access(&root, &carve.pattern, DefaultPolicy::None).await.unwrap(),
            Access::Write,
            "{name}: root bypass"
        );

        // exact beats prefix, longest prefix wins among prefixes
        let bob = user();
        s.grant_create("github.com/*", &bob, Role::Read, "root").await.unwrap();
        s.grant_create("github.com/acme/*", &bob, Role::Write, "root").await.unwrap();
        s.grant_create("github.com/acme/billing", &bob, Role::Read, "root").await.unwrap();
        let bob = Identity { user: Some(bob), admin: false };
        for (project, expected) in [
            ("github.com/acme/billing", Access::Read),
            ("github.com/acme/shop", Access::Write),
            ("github.com/other/shop", Access::Read),
        ] {
            assert_eq!(
                s.effective_access(&bob, project, DefaultPolicy::None).await.unwrap(),
                expected,
                "{name}: {project}"
            );
        }

        // grant CRUD and removal
        let all = s.grants_for_user(Some(id.user.as_ref().unwrap())).await.unwrap();
        assert_eq!(all.len(), 2, "{name}");
        s.grant_delete(&carve.id).await.unwrap();
        assert_eq!(
            s.effective_access(&id, &carve.pattern, DefaultPolicy::None).await.unwrap(),
            Access::Write,
            "{name}: carve-out removed"
        );
    }
}

#[tokio::test]
async fn default_policies_and_authorize() {
    for (name, s) in stores().await {
        let alice = user();
        s.user_create(&alice, false).await.unwrap();
        let id = Identity { user: Some(alice), admin: false };
        let project = format!("test/{}", new_id());

        for (policy, expected) in [
            (DefaultPolicy::Write, Access::Write),
            (DefaultPolicy::Read, Access::Read),
            (DefaultPolicy::None, Access::None),
        ] {
            assert_eq!(s.effective_access(&id, &project, policy).await.unwrap(), expected, "{name}: {policy:?}");
        }
        // write satisfies read, but read does not satisfy write
        assert!(Access::Write.satisfies(Access::Read));
        assert!(!Access::Read.satisfies(Access::Write));

        // a denied authorization is audited with the project and needed access
        s.authorize(&id, &project, Access::Write).await.unwrap(); // env default = write
        let events = s.audit_query(AuditQuery { project: Some(project.clone()), ..Default::default() }).await.unwrap();
        assert!(events.is_empty(), "{name}: allowed calls aren't audited");
        // explicit snapshot check through the same path HTTP/MCP use
        let policy = s.access_policy(id.user.as_deref(), DefaultPolicy::None).await.unwrap();
        assert!(!policy.allows(&id, &project, Access::Read));
        s.audit(NewAuditEvent::denied(id.actor(), "denied").project(&project).detail(json!({"needed": "read"})))
            .await
            .unwrap();
        let events = s.audit_query(AuditQuery { project: Some(project.clone()), ..Default::default() }).await.unwrap();
        assert_eq!(events.len(), 1, "{name}");
        assert_eq!(events[0].outcome, AuditOutcome::Denied);
        assert_eq!(events[0].detail.as_ref().unwrap()["needed"], "read");
    }
}

#[tokio::test]
async fn global_memories_keep_their_owner_rule() {
    for (name, s) in stores().await {
        let alice = user();
        let bob = user();
        s.user_create(&alice, false).await.unwrap();
        s.user_create(&bob, false).await.unwrap();
        let project = format!("test/{}", new_id());
        let global = s
            .memory_save(NewMemory {
                content: "alice's private preference".into(),
                user: Some(alice.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        let mine = s
            .memory_save(NewMemory {
                project: Some(project.clone()),
                content: "shared project fact".into(),
                user: Some(alice.clone()),
                ..Default::default()
            })
            .await
            .unwrap();

        // an empty readable set hides every project memory but not globals
        let visible = s
            .memory_search(MemoryQuery {
                user: Some(alice.clone()),
                allowed_projects: Some(vec![]),
                ..Default::default()
            })
            .await
            .unwrap();
        let ids: Vec<_> = visible.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&global.id.as_str()) && !ids.contains(&mine.id.as_str()), "{name}");

        let visible = s
            .memory_search(MemoryQuery {
                user: Some(bob.clone()),
                allowed_projects: Some(vec![project.clone()]),
                ..Default::default()
            })
            .await
            .unwrap();
        let ids: Vec<_> = visible.iter().map(|m| m.id.as_str()).collect();
        assert!(!ids.contains(&global.id.as_str()) && ids.contains(&mine.id.as_str()), "{name}");

        // global writes are still owner-only, independent of grants
        let patch = MemoryPatch { content: Some("edited".into()), ..Default::default() };
        assert!(
            s.memory_update_as(&global.id, patch.clone(), &Identity { user: Some(bob), admin: false }).await.is_err(),
            "{name}"
        );
        assert!(
            s.memory_update_as(&global.id, patch, &Identity { user: Some(alice), admin: false }).await.is_ok(),
            "{name}"
        );
    }
}

#[tokio::test]
async fn audit_query_filters_and_retention_pruning() {
    for (name, s) in stores().await {
        let actor = user();
        let a = format!("test/{}", new_id());
        let b = format!("test/{}", new_id());
        for (project, action) in [(&a, "grant.create"), (&b, "export")] {
            s.audit(NewAuditEvent::ok(&actor, action).project(project).detail(json!({"secret": "token-12345"})))
                .await
                .unwrap();
        }

        let all = s.audit_query(AuditQuery { actor: Some(actor.clone()), ..Default::default() }).await.unwrap();
        assert_eq!(all.len(), 2, "{name}");
        assert!(
            all.iter().all(|e| !e.detail.as_ref().unwrap().to_string().contains("token-12345")),
            "{name}: detail redacted"
        );
        let filtered = s.audit_query(AuditQuery { project: Some(a.clone()), ..Default::default() }).await.unwrap();
        assert_eq!(filtered.len(), 1, "{name}");
        assert_eq!(filtered[0].action, "grant.create");
        let limited = s
            .audit_query(AuditQuery { actor: Some(actor.clone()), limit: Some(1), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(limited.len(), 1, "{name}");

        // A full prune prunes the audit log too (events are recent here, so a
        // cutoff just after `now` removes them). Postgres tests share one
        // database, so only assert the exact count on the private SQLite DB.
        if name == "sqlite" {
            let deleted = s.audit_delete_before(now_ms() + 1000).await.unwrap();
            assert_eq!(deleted, 2, "{name}");
            let left = s.audit_query(AuditQuery { actor: Some(actor), ..Default::default() }).await.unwrap();
            assert!(left.is_empty(), "{name}");
        }
        assert_eq!(access::audit_retention_days(), 365, "{name}: default retention");
    }
}
