//! Semantic search tests with a deterministic fake embedder (no network). Run against SQLite always, and
//! against Postgres too when `MEMDUSA_TEST_POSTGRES_URL` is set (assertions stay inside each test's own
//! project: Postgres tests share the global memories).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use memdusa_core::embed::{EmbedFuture, Embedder};
use memdusa_core::model::*;
use memdusa_core::store::words;
use memdusa_core::{Store, store};

async fn stores() -> Vec<(String, Store)> {
    let dir = std::env::temp_dir().join(format!("memdusa-test-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut out = vec![("sqlite".to_string(), Store::connect(&store::sqlite_url(&dir.join("t.db"))).await.unwrap())];
    if let Ok(url) = std::env::var("MEMDUSA_TEST_POSTGRES_URL") {
        out.push(("postgres".into(), Store::connect(&url).await.unwrap()));
    }
    out
}

fn project() -> String {
    format!("test/{}", new_id())
}

/// Bag of words hashed into 64 dimensions; short words are dropped and synonyms share a dimension, so
/// "ship" ~ "release" and "deploy" ~ "release" without any word in common.
struct Fake {
    model: String,
    calls: AtomicUsize,
    fail: AtomicBool,
}

impl Fake {
    fn new(model: &str) -> Arc<Self> {
        Arc::new(Self { model: model.into(), calls: AtomicUsize::new(0), fail: AtomicBool::new(false) })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn vector(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; 64];
    for w in words(text).into_iter().filter(|w| w.chars().count() >= 4) {
        let canon = match w.as_str() {
            "ship" | "ships" | "shipped" | "shipping" | "releases" | "deploy" | "deploys" | "publish" => "release",
            "artifacts" | "bundles" | "binaries" => "artifact",
            other => other,
        };
        let h = canon.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
        v[(h % 64) as usize] += 1.0;
    }
    v
}

impl Embedder for Fake {
    fn model(&self) -> &str {
        &self.model
    }
    fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) {
                return Err("provider down".into());
            }
            Ok(texts.iter().map(|t| vector(t)).collect())
        })
    }
}

fn fact(p: &str, content: &str) -> NewMemory {
    NewMemory { project: Some(p.into()), content: content.into(), ..Default::default() }
}

fn q(p: &str, text: &str) -> MemoryQuery {
    // a high limit: Postgres tests share one database, and other tests' global memories must not
    // push this test's memories out of the results
    MemoryQuery { project: Some(p.into()), query: Some(text.into()), limit: Some(200), ..Default::default() }
}

fn mine(p: &str, found: Vec<Memory>) -> Vec<String> {
    found.into_iter().filter(|m| m.project.as_deref() == Some(p)).map(|m| m.id).collect()
}

#[tokio::test]
async fn semantic_search_finds_what_lexical_search_misses() {
    for (name, base) in stores().await {
        let p = project();
        let fake = Fake::new("fake-1");
        let s = base.clone().with_embedder(fake.clone());
        let release = s.memory_save(fact(&p, "the release workflow pushes binaries on tag")).await.unwrap();
        s.memory_save(fact(&p, "database schema migrations live in sql files")).await.unwrap();
        assert!(s.memory_embedding(&release.id).await.unwrap().is_some(), "{name}: embedded on save");

        let query = "how do we ship releases";
        // no embedder: lexical only, no common word
        assert!(mine(&p, base.memory_search(q(&p, query)).await.unwrap()).is_empty(), "{name}: lexical misses");
        // with the embedder: found, and only that memory
        let found = s.memory_search_scored(q(&p, query), None).await.unwrap();
        let ids: Vec<String> =
            found.iter().filter(|h| h.memory.project.as_deref() == Some(&p)).map(|h| h.memory.id.clone()).collect();
        assert_eq!(ids, vec![release.id.clone()], "{name}");
        assert!(found.iter().find(|h| h.memory.id == release.id).unwrap().cosine.unwrap() >= 0.3, "{name}");
        // the public ranked search and memory_search use it too
        assert_eq!(mine(&p, s.memory_search(q(&p, query)).await.unwrap()), vec![release.id.clone()], "{name}");
        // no query: unchanged (recent first), no embedding call
        let before = fake.calls();
        assert!(
            s.memory_search(MemoryQuery { project: Some(p.clone()), ..Default::default() }).await.unwrap().len() >= 2
        );
        assert_eq!(fake.calls(), before, "{name}: listing needs no embedding");
    }
}

#[tokio::test]
async fn hybrid_score_orders_by_cosine_and_lexical() {
    for (name, base) in stores().await {
        let p = project();
        let s = base.clone().with_embedder(Fake::new("fake-1"));
        // both semantic hits for "releases"; only `both` also matches the word
        let semantic = s.memory_save(fact(&p, "ship the binaries with the workflow")).await.unwrap();
        let both = s.memory_save(fact(&p, "releases are cut every friday")).await.unwrap();
        // lexical hit with no vector (saved before the embedder was configured)
        let plain = base.memory_save(fact(&p, "releases need changelog entries first")).await.unwrap();
        assert!(s.memory_embedding(&plain.id).await.unwrap().is_none(), "{name}");

        let found = s.memory_search_scored(q(&p, "releases"), None).await.unwrap();
        let mine: Vec<&_> = found.iter().filter(|h| h.memory.project.as_deref() == Some(&p)).collect();
        let order: Vec<&str> = mine.iter().map(|h| h.memory.id.as_str()).collect();
        assert_eq!(order.len(), 3, "{name}: {order:?}");
        assert!(order.contains(&semantic.id.as_str()), "{name}: semantic-only hit is returned");
        let pos = |id: &str| order.iter().position(|x| *x == id).unwrap();
        assert!(pos(&both.id) < pos(&semantic.id), "{name}: lexical + semantic beats semantic only");
        let hit = |id: &str| mine.iter().find(|h| h.memory.id == id).unwrap();
        assert!(hit(&plain.id).cosine.is_none(), "{name}: no vector, lexical only");
        assert!(hit(&plain.id).score > 0.0, "{name}");
        assert!(hit(&both.id).score > hit(&semantic.id).score, "{name}");
        assert!(hit(&both.id).score <= 1.0 + 1e-9, "{name}");
    }
}

#[tokio::test]
async fn embedding_follows_content_changes() {
    for (name, base) in stores().await {
        let p = project();
        let fake = Fake::new("fake-1");
        let s = base.clone().with_embedder(fake.clone());
        let m = s.memory_save(fact(&p, "we ship the artifacts on friday")).await.unwrap();
        let first = s.memory_embedding(&m.id).await.unwrap().unwrap();
        assert_eq!((first.model.as_str(), first.dim), ("fake-1", 64), "{name}");

        // a patch that leaves the embedded text alone costs no API call
        let calls = fake.calls();
        s.memory_update(&m.id, MemoryPatch { kind: Some(MemoryKind::Decision), ..Default::default() }).await.unwrap();
        assert_eq!(fake.calls(), calls, "{name}: same text, same vector");

        // new content: re-embedded, hash changes, and search follows the new meaning
        s.memory_update(
            &m.id,
            MemoryPatch { content: Some("database schema migrations are reviewed".into()), ..Default::default() },
        )
        .await
        .unwrap();
        assert_eq!(fake.calls(), calls + 1, "{name}");
        let second = s.memory_embedding(&m.id).await.unwrap().unwrap();
        assert_ne!(second.content_hash, first.content_hash, "{name}");
        assert!(
            mine(&p, s.memory_search(q(&p, "deploy bundles")).await.unwrap()).is_empty(),
            "{name}: old meaning gone"
        );

        // tags are part of the embedded text
        s.memory_update(&m.id, MemoryPatch { tags: Some(vec!["release".into()]), ..Default::default() }).await.unwrap();
        assert_ne!(s.memory_embedding(&m.id).await.unwrap().unwrap().content_hash, second.content_hash, "{name}");

        // a near-duplicate merge rewrites the content in place: re-embedded too
        let merged = s
            .memory_save(NewMemory {
                kind: MemoryKind::Decision, // the patch above made it a decision; merging needs the same kind
                ..fact(&p, "database schema migrations are always reviewed")
            })
            .await
            .unwrap();
        assert!(merged.merged && merged.id == m.id, "{name}");
        let third = s.memory_embedding(&m.id).await.unwrap().unwrap();
        assert_ne!(third.content_hash, second.content_hash, "{name}");
    }
}

#[tokio::test]
async fn backfill_fills_gaps_and_follows_model_changes() {
    // Backfill is now project-scoped, but keep this model-migration test on the private SQLite
    // store: its `up_to_date` / `embedded` counts are about this test's corpus only.
    for (name, base) in stores().await.into_iter().filter(|(name, _)| *name == "sqlite") {
        let p = project();
        let old = base.memory_save(fact(&p, "we ship the artifacts on friday")).await.unwrap();
        assert!(base.memory_embedding(&old.id).await.unwrap().is_none(), "{name}: no embedder, no vector");
        assert!(base.embeddings_backfill(false, Some(&p)).await.is_err(), "{name}: backfill needs an embedder");

        let v1 = base.clone().with_embedder(Fake::new("model-1"));
        let r = v1.embeddings_backfill(false, Some(&p)).await.unwrap();
        assert!(r.embedded >= 1 && r.failed == 0 && r.model == "model-1", "{name}: {r:?}");
        assert_eq!(v1.memory_embedding(&old.id).await.unwrap().unwrap().model, "model-1", "{name}");
        let stats = v1.embedding_stats().await.unwrap();
        assert_eq!(stats.model.as_deref(), Some("model-1"));
        assert!(stats.embedded >= 1 && stats.embedded <= stats.total, "{name}: {stats:?}");
        assert_eq!(base.embedding_stats().await.unwrap().model, None, "{name}");

        // nothing to do the second time
        let again = v1.embeddings_backfill(false, Some(&p)).await.unwrap();
        assert_eq!(again.embedded, 0, "{name}: {again:?}");
        assert!(again.up_to_date >= 1, "{name}");
        // `all` redoes everything
        assert!(v1.embeddings_backfill(true, Some(&p)).await.unwrap().embedded >= 1, "{name}");

        // another model: vectors of the old one are not used, and backfill re-embeds
        let v2 = base.clone().with_embedder(Fake::new("model-2"));
        assert!(
            mine(&p, v2.memory_search(q(&p, "deploy bundles")).await.unwrap()).is_empty(),
            "{name}: stale model ignored"
        );
        let r = v2.embeddings_backfill(false, Some(&p)).await.unwrap();
        assert!(r.embedded >= 1, "{name}: {r:?}");
        assert_eq!(v2.memory_embedding(&old.id).await.unwrap().unwrap().model, "model-2", "{name}");
        assert_eq!(mine(&p, v2.memory_search(q(&p, "deploy bundles")).await.unwrap()), vec![old.id.clone()], "{name}");
    }
}

#[tokio::test]
async fn backfill_scope_only_embeds_one_project() {
    for (name, base) in stores().await {
        let (p, other) = (project(), project());
        let s = base.clone().with_embedder(Fake::new("fake-1"));
        let mine = base.memory_save(fact(&p, "we ship artifacts")).await.unwrap();
        let theirs = base.memory_save(fact(&other, "deploy bundles")).await.unwrap();

        let r = s.embeddings_backfill(false, Some(&p)).await.unwrap();
        assert_eq!(r.embedded, 1, "{name}: {r:?}");
        assert!(s.memory_embedding(&mine.id).await.unwrap().is_some(), "{name}");
        assert!(s.memory_embedding(&theirs.id).await.unwrap().is_none(), "{name}: other project untouched");
    }
}

#[tokio::test]
async fn forgetting_a_memory_drops_its_embedding() {
    for (name, base) in stores().await {
        let p = project();
        let s = base.clone().with_embedder(Fake::new("fake-1"));
        let m = s.memory_save(fact(&p, "we ship the artifacts on friday")).await.unwrap();
        assert_eq!(mine(&p, s.memory_search(q(&p, "deploy bundles")).await.unwrap()), vec![m.id.clone()], "{name}");
        s.memory_forget(&m.id).await.unwrap();
        assert!(s.memory_embedding(&m.id).await.unwrap().is_none(), "{name}");
        assert!(mine(&p, s.memory_search(q(&p, "deploy bundles")).await.unwrap()).is_empty(), "{name}");
    }
}

#[tokio::test]
async fn provider_failures_never_break_saves_or_searches() {
    for (name, base) in stores().await {
        let p = project();
        let fake = Fake::new("fake-1");
        let s = base.clone().with_embedder(fake.clone());
        let ok = s.memory_save(fact(&p, "we ship the artifacts on friday")).await.unwrap();
        assert!(s.memory_embedding(&ok.id).await.unwrap().is_some(), "{name}");

        fake.fail.store(true, Ordering::SeqCst);
        // save still works, without a vector
        let m = s.memory_save(fact(&p, "rollbacks use the previous tag")).await.unwrap();
        assert!(s.memory_embedding(&m.id).await.unwrap().is_none(), "{name}");
        // a failed re-embed must not leave the vector of the old wording behind
        s.memory_update(
            &ok.id,
            MemoryPatch { content: Some("completely different words here".into()), ..Default::default() },
        )
        .await
        .unwrap();
        assert!(s.memory_embedding(&ok.id).await.unwrap().is_none(), "{name}: stale vector removed");
        // search falls back to lexical, which is identical to the store without an embedder
        let lexical = mine(&p, base.memory_search(q(&p, "rollbacks tag")).await.unwrap());
        assert_eq!(mine(&p, s.memory_search(q(&p, "rollbacks tag")).await.unwrap()), lexical, "{name}");
        assert_eq!(lexical, vec![m.id.clone()], "{name}");
        // backfill reports the failure instead of raising it
        // Scope the backfill to this test's project: the shared Postgres database is used by other
        // tests in parallel, and embedding their memories here both changes their expectations and
        // can give us vectors we did not create.
        let r = s.embeddings_backfill(false, Some(&p)).await.unwrap();
        assert!(r.failed >= 1 && r.embedded == 0, "{name}: {r:?}");
        // a provider that comes back is picked up by the next backfill
        fake.fail.store(false, Ordering::SeqCst);
        assert!(s.embeddings_backfill(false, Some(&p)).await.unwrap().embedded >= 2, "{name}");
        assert!(s.memory_embedding(&m.id).await.unwrap().is_some(), "{name}");
    }
}

#[tokio::test]
async fn slow_provider_times_out_to_lexical() {
    struct Slow;
    impl Embedder for Slow {
        fn model(&self) -> &str {
            "slow"
        }
        fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a> {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Ok(texts.iter().map(|_| vec![1.0]).collect())
            })
        }
    }
    for (name, base) in stores().await {
        let p = project();
        let m = base.memory_save(fact(&p, "rollbacks use the previous tag")).await.unwrap();
        let s = base.clone().with_embedder(Arc::new(Slow));
        let t = std::time::Instant::now();
        let found =
            s.memory_search_scored(q(&p, "rollbacks"), Some(std::time::Duration::from_millis(100))).await.unwrap();
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{name}: capped");
        let ids: Vec<&str> =
            found.iter().filter(|h| h.memory.project.as_deref() == Some(&p)).map(|h| h.memory.id.as_str()).collect();
        assert_eq!(ids, vec![m.id.as_str()], "{name}: lexical fallback");
        assert!(found.iter().all(|h| h.cosine.is_none()), "{name}");
    }
}

#[tokio::test]
async fn save_time_merging_stays_lexical() {
    for (name, base) in stores().await {
        let p = project();
        let fake = Fake::new("fake-1");
        let s = base.clone().with_embedder(fake.clone());
        // same meaning for the embedder, no shared words: must stay two memories
        let a = s.memory_save(fact(&p, "we ship artifacts")).await.unwrap();
        let b = s.memory_save(fact(&p, "deploy bundles")).await.unwrap();
        assert_ne!(a.id, b.id, "{name}");
        assert!(!b.merged, "{name}");
    }
}

#[tokio::test]
async fn review_groups_memories_with_close_embeddings() {
    for (name, base) in stores().await {
        let p = project();
        let s = base.clone().with_embedder(Fake::new("fake-1"));
        let a = base.memory_save(fact(&p, "we ship artifacts")).await.unwrap();
        let b = base.memory_save(fact(&p, "deploy bundles")).await.unwrap();
        let c = base.memory_save(fact(&p, "database schema migrations are reviewed")).await.unwrap();
        let grouped = |r: &memdusa_core::review::MemoryReview| {
            r.related.iter().any(|g| g.iter().any(|i| i.id == a.id) && g.iter().any(|i| i.id == b.id))
        };

        // no vectors yet: nothing in common lexically
        assert!(!grouped(&base.memory_review(&p, None).await.unwrap()), "{name}");
        assert!(!grouped(&s.memory_review(&p, None).await.unwrap()), "{name}: no vectors stored yet");

        // Only this project may be embedded: other tests run in parallel against the same Postgres
        // database, and a global backfill would embed their memories (and let theirs embed ours).
        s.embeddings_backfill(false, Some(&p)).await.unwrap();
        let r = s.memory_review(&p, None).await.unwrap();
        let group = r.related.iter().find(|g| g.iter().any(|i| i.id == a.id)).expect("a is grouped");
        assert!(group.iter().any(|i| i.id == b.id), "{name}: same meaning, grouped");
        assert!(!group.iter().any(|i| i.id == c.id), "{name}: unrelated one is not");
        // without the embedder configured the review is lexical again
        assert!(!grouped(&base.memory_review(&p, None).await.unwrap()), "{name}");
    }
}
