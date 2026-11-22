//! Quality gate for built-in local embeddings: the exact probe from `probe-reference.swift`.
//!
//! Ignored by default because it downloads a model on first use. Run with:
//!
//! ```sh
//! MEMDUSA_HOME=/path/to/scratch cargo test -p memdusa --features local-embed \
//!     --test local_embed_quality -- --ignored --nocapture
//! MEMDUSA_LOCAL_QUALITY_MODEL=bge-small-en-v1.5  # optional alternative candidate
//! ```
//!
//! Prints one cosine row per query (memory order: release workflow, schema migrations, money in
//! integer cents) and cold/warm single-query latency.

#![cfg(feature = "local-embed")]

use std::path::PathBuf;
use std::time::Instant;

use memdusa_core::embed::{Embedder, cosine};

use super::{DEFAULT_MODEL, LocalEmbedder};

const MEMORIES: &[&str] = &[
    "the release workflow pushes binaries and the docker image when a tag is pushed",
    "database schema changes are versioned migrations in store.rs",
    "money is always stored as integer cents, never floats",
];
/// (query, index of the memory that should rank first; `None` for unrelated queries).
const QUERIES: &[(&str, Option<usize>)] = &[
    ("how do we ship a new version", Some(0)),
    ("currency precision", Some(2)),
    ("add a discount field, careful with currency precision", Some(2)),
    ("let's cut a new release tonight", Some(0)),
    ("what color is the sky", None),
    ("fix the typo in the README", None),
];
/// Same relative margin the hybrid search uses to discard weak semantic hits.
const CLEAR_WINNER: f32 = 0.08;

#[tokio::test]
#[ignore = "downloads an embedding model; see the command at the top of this file"]
async fn local_embed_quality_probe() {
    // Never use the user's real MEMDUSA_HOME for a test download.
    let home = std::env::var_os("MEMDUSA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("memdusa-local-embed-quality"));
    let model = std::env::var("MEMDUSA_LOCAL_QUALITY_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
    let dir = home.join("models");
    println!("model: {model}  cache: {}", dir.display());

    let setup = LocalEmbedder::new(&model, dir.clone()).expect("known model");
    let t0 = Instant::now();
    setup.prepare().await.expect("model download and load");
    println!("first download + load: {:.0} ms", t0.elapsed().as_secs_f64() * 1000.0);

    // A fresh embedder pays session creation on its first query (a hook process does this); the
    // second call on the same instance is the warm path.
    let cold = LocalEmbedder::new(&model, dir).expect("known model");
    let one: Vec<String> = vec![QUERIES[0].0.into()];
    let t0 = Instant::now();
    cold.embed(&one).await.expect("single query");
    let cold_ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!("single query, cold (load + 1 text): {cold_ms:.0} ms");
    assert!(cold_ms < 3000.0, "cold single-query latency exceeds the 3 s recall budget");

    let texts: Vec<String> = MEMORIES.iter().map(|s| (*s).to_owned()).collect();
    let memory_vectors = cold.embed(&texts).await.expect("embed memories");
    let t0 = Instant::now();
    let queries: Vec<String> = QUERIES.iter().map(|(q, _)| (*q).to_owned()).collect();
    let query_vectors = cold.embed(&queries).await.expect("embed queries");
    let warm_ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!("warm (6 queries, 1 call): {warm_ms:.0} ms ({:.0} ms/query)", warm_ms / 6.0);
    assert_eq!(memory_vectors.len(), 3);
    assert_eq!(query_vectors.len(), QUERIES.len());

    println!("cosine matrix (release, schema, money):");
    let mut correct = 0;
    for ((query, want), qv) in QUERIES.iter().zip(&query_vectors) {
        let scores: Vec<f32> = memory_vectors.iter().map(|mv| cosine(qv, mv)).collect();
        let shown: Vec<String> = scores.iter().map(|s| format!("{s:.3}")).collect();
        println!("  {query}: [{}]", shown.join(", "));
        let best = scores.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        match want {
            Some(want) => {
                if best == *want {
                    correct += 1;
                } else {
                    println!("    WRONG top: {best}, expected {want}");
                }
            }
            None => {
                let mut sorted = scores.clone();
                sorted.sort_by(f32::total_cmp);
                let gap = sorted[2] - sorted[1];
                println!("    unrelated gap to runner-up: {gap:.3}");
                assert!(gap < CLEAR_WINNER, "unrelated query {query:?} clearly favours one memory");
            }
        }
    }
    assert!(correct >= 3, "only {correct}/4 target queries put the right memory on top");
    println!("quality gate: {correct}/4 target queries correct");

    // A single query is what prompt recall does, with a 3 s cap (hooks).
    let t0 = Instant::now();
    cold.embed(&one).await.expect("single query");
    let single_ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!("single query (warm): {single_ms:.0} ms (cap 3000 ms)");
    assert!(single_ms < 3000.0, "single-query latency exceeds the 3 s recall budget");
}
