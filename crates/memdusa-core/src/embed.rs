//! Semantic memory search: embeddings behind a trait.
//!
//! Core knows nothing about providers (or HTTP): the host injects an [`Embedder`] with
//! [`Store::with_embedder`]. Without one everything behaves exactly like the lexical search.
//!
//! Vectors live in the portable `memory_embeddings` table (base64 of little-endian `f32`s in a TEXT
//! column, so SQLite and Postgres share the code; similarity is computed here, no pgvector).
//! Embedding is best effort everywhere: a provider failure is logged as a warning and never fails a
//! save, a search or a hook.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::model::*;
use crate::store::{MEMORY_COLS_M, Q, Store, push_scope, search_terms};
use crate::{Error, Result};

/// Boxed future returned by [`Embedder::embed`], so the trait can be used as `Arc<dyn Embedder>`.
pub type EmbedFuture<'a> = Pin<Box<dyn Future<Output = std::result::Result<Vec<Vec<f32>>, String>> + Send + 'a>>;

/// Turns texts into vectors. One vector per input text, in order, all of the same length.
pub trait Embedder: Send + Sync {
    /// Identifies the vector space: vectors of different models are never compared.
    fn model(&self) -> &str;
    fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a>;
}

/// Weight of the cosine similarity in the hybrid score; the rest is normalized lexical relevance.
pub const COSINE_WEIGHT: f64 = 0.6;
/// A memory with at least this cosine similarity to the query is a semantic hit.
pub const COSINE_FLOOR: f32 = 0.3;
/// A memory found by meaning alone (no lexical match) must also be within this much of the best
/// cosine in the result set: absolute similarities differ a lot between models (unrelated text scores
/// ~0.3-0.4 with some local models), so a fixed floor alone would let everything through.
pub const COSINE_MARGIN: f32 = 0.08;
/// Pairwise cosine from which two memories count as related in a memory review.
pub const RELATED_COSINE: f32 = 0.80;
/// Most recent memories scored semantically per search.
const SEMANTIC_CANDIDATES: i64 = 1000;
/// Lexical hits merged into the candidates (the lexical search's own maximum).
const LEXICAL_HITS: i64 = 200;
/// Texts per embedding request.
pub const BATCH: usize = 64;
/// Longest query text sent to the provider, in characters.
const MAX_QUERY_CHARS: usize = 8000;

/// Cosine similarity of two vectors (0 when the lengths differ or one has no magnitude).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 { 0.0 } else { (dot / (na.sqrt() * nb.sqrt())) as f32 }
}

pub fn encode_vector(v: &[f32]) -> String {
    let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn decode_vector(s: &str) -> Option<Vec<f32>> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(s).ok()?;
    if bytes.len() % 4 != 0 {
        return None;
    }
    Some(bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect())
}

/// What is embedded for a memory: its content and tags.
fn embed_text(content: &str, tags: &[String]) -> String {
    if tags.is_empty() { content.to_owned() } else { format!("{content}\n\ntags: {}", tags.join(", ")) }
}

fn text_hash(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// A memory with its relevance to a query.
#[derive(Debug, Clone)]
pub struct Scored {
    pub memory: Memory,
    /// Higher is better; comparable within one result set only.
    pub score: f64,
    /// Cosine similarity to the query, when the memory has a vector (semantic search only).
    pub cosine: Option<f32>,
}

/// The stored embedding of a memory (without the vector itself).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredEmbedding {
    pub model: String,
    pub dim: i64,
    pub content_hash: String,
}

/// Outcome of [`Store::embeddings_backfill`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillReport {
    pub model: String,
    pub embedded: usize,
    /// Already embedded with the current model and content; left alone.
    pub up_to_date: usize,
    /// Could not be embedded (provider error); a later run retries them.
    pub failed: usize,
}

/// How much of the corpus semantic search covers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingStats {
    /// The configured model; `None` = semantic search is off.
    pub model: Option<String>,
    /// Memories embedded with the configured model.
    pub embedded: i64,
    pub total: i64,
}

impl Store {
    /// Turn on semantic search with this embedder.
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    pub fn embedder(&self) -> Option<&Arc<dyn Embedder>> {
        self.embedder.as_ref()
    }

    // ------------------------------------------------------------ writing

    /// Embed a memory after it was written; a no-op without an embedder. Never fails: problems are
    /// logged as one warning line.
    pub(crate) async fn embed_memory_best_effort(&self, id: &str, content: &str, tags: &[String]) {
        let Some(e) = &self.embedder else { return };
        if let Err(err) = self.embed_memory(e.as_ref(), id, content, tags).await {
            tracing::warn!("semantic search: could not embed memory {id}, it stays lexical-only: {err}");
            // never keep a vector of the old wording; `memdusa embed` fills it in later
            let _ = self.embedding_delete(id).await;
        }
    }

    async fn embed_memory(&self, e: &dyn Embedder, id: &str, content: &str, tags: &[String]) -> Result<()> {
        let text = embed_text(content, tags);
        let hash = text_hash(&text);
        if let Some(have) = self.memory_embedding(id).await?
            && have.model == e.model()
            && have.content_hash == hash
        {
            return Ok(());
        }
        let texts = [text];
        let mut vectors = check_vectors(e.embed(&texts).await, 1).map_err(Error::Invalid)?;
        let v = vectors.remove(0);
        self.store_embedding(id, e.model(), &hash, &v).await
    }

    async fn store_embedding(&self, id: &str, model: &str, hash: &str, v: &[f32]) -> Result<()> {
        let mut q =
            Q::new("INSERT INTO memory_embeddings (memory_id, model, dim, vector, content_hash, updated_at) VALUES (");
        let vals =
            [q.text(id), q.text(model), q.int(v.len() as i64), q.text(encode_vector(v)), q.text(hash), q.int(now_ms())];
        q.push(&vals.join(", ")).push(
            ") ON CONFLICT (memory_id) DO UPDATE SET model = excluded.model, dim = excluded.dim, \
             vector = excluded.vector, content_hash = excluded.content_hash, updated_at = excluded.updated_at",
        );
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    pub(crate) async fn embedding_delete(&self, id: &str) -> Result<()> {
        let mut q = Q::new("DELETE FROM memory_embeddings WHERE memory_id = ");
        let id = q.text(id);
        q.push(&id);
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    /// The stored embedding of a memory, if any.
    pub async fn memory_embedding(&self, id: &str) -> Result<Option<StoredEmbedding>> {
        let mut q = Q::new("SELECT model, dim, content_hash FROM memory_embeddings WHERE memory_id = ");
        let id = q.text(id);
        q.push(&id);
        q.build()
            .fetch_optional(&self.pool)
            .await?
            .map(|r| {
                Ok(StoredEmbedding {
                    model: r.try_get("model")?,
                    dim: r.try_get("dim")?,
                    content_hash: r.try_get("content_hash")?,
                })
            })
            .transpose()
    }

    /// Embed the memories that have no embedding for the current model or whose content changed
    /// since; with `all`, re-embed every memory. With `project`, only that project's memories are
    /// considered (`None` embeds the whole corpus, as `memdusa embed` does by default). Scoping is
    /// useful for a targeted backfill and keeps concurrent users from touching each other's data.
    /// Provider errors are counted, not raised.
    pub async fn embeddings_backfill(&self, all: bool, project: Option<&str>) -> Result<BackfillReport> {
        let Some(e) = self.embedder.clone() else {
            return Err(Error::Invalid("semantic search is off: set MEMDUSA_EMBED_URL on the memdusa process".into()));
        };
        let mut report = BackfillReport { model: e.model().to_owned(), ..Default::default() };
        let mut q = Q::new(
            "SELECT m.id AS id, m.content AS content, m.tags AS tags, e.model AS e_model, e.content_hash AS e_hash \
             FROM memories m LEFT JOIN memory_embeddings e ON e.memory_id = m.id",
        );
        if let Some(p) = project {
            let p = q.text(p);
            q.push(" WHERE m.project = ").push(&p);
        }
        q.push(" ORDER BY m.updated_at DESC");
        let rows = q.build().fetch_all(&self.pool).await?;

        let mut todo: Vec<(String, String, String)> = vec![]; // id, text, hash
        for r in &rows {
            let tags: Vec<String> = serde_json::from_str(&r.try_get::<String, _>("tags")?).unwrap_or_default();
            let text = embed_text(&r.try_get::<String, _>("content")?, &tags);
            let hash = text_hash(&text);
            let (m, h): (Option<String>, Option<String>) = (r.try_get("e_model")?, r.try_get("e_hash")?);
            if all || m.as_deref() != Some(e.model()) || h.as_deref() != Some(hash.as_str()) {
                todo.push((r.try_get("id")?, text, hash));
            } else {
                report.up_to_date += 1;
            }
        }

        let mut chunks = todo.chunks(BATCH);
        for chunk in chunks.by_ref() {
            let texts: Vec<String> = chunk.iter().map(|(_, t, _)| t.clone()).collect();
            match check_vectors(e.embed(&texts).await, texts.len()) {
                Ok(vectors) => {
                    for ((id, _, hash), v) in chunk.iter().zip(&vectors) {
                        self.store_embedding(id, e.model(), hash, v).await?;
                        report.embedded += 1;
                    }
                }
                Err(err) => {
                    // the provider is probably down: don't hammer it with the remaining batches
                    tracing::warn!("semantic search: backfill stopped, the embedding provider failed: {err}");
                    report.failed += chunk.len() + chunks.by_ref().map(<[_]>::len).sum::<usize>();
                    break;
                }
            }
        }
        Ok(report)
    }

    // ------------------------------------------------------------ status

    pub async fn embedding_stats(&self) -> Result<EmbeddingStats> {
        let total: i64 =
            Q::new("SELECT COUNT(*) AS n FROM memories").build().fetch_one(&self.pool).await?.try_get("n")?;
        let Some(e) = &self.embedder else { return Ok(EmbeddingStats { model: None, embedded: 0, total }) };
        let mut q = Q::new(
            "SELECT COUNT(*) AS n FROM memory_embeddings e JOIN memories m ON m.id = e.memory_id WHERE e.model = ",
        );
        let model = q.text(e.model());
        q.push(&model);
        let embedded: i64 = q.build().fetch_one(&self.pool).await?.try_get("n")?;
        Ok(EmbeddingStats { model: Some(e.model().to_owned()), embedded, total })
    }

    /// Vectors (of the configured model) of the given memories; memories without one are absent.
    pub(crate) async fn memory_vectors(&self, ids: &[&str]) -> Result<HashMap<String, Vec<f32>>> {
        let mut out = HashMap::new();
        let Some(e) = &self.embedder else { return Ok(out) };
        for chunk in ids.chunks(200) {
            let mut q = Q::new("SELECT memory_id, vector FROM memory_embeddings WHERE model = ");
            let model = q.text(e.model());
            q.push(&model).push(" AND memory_id IN (");
            let marks: Vec<String> = chunk.iter().map(|id| q.text(*id)).collect();
            q.push(&marks.join(", ")).push(")");
            for r in q.build().fetch_all(&self.pool).await? {
                if let Some(v) = decode_vector(&r.try_get::<String, _>("vector")?) {
                    out.insert(r.try_get("memory_id")?, v);
                }
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------ search

    /// Search like [`Store::memory_search_ranked`], keeping each hit's cosine similarity.
    ///
    /// With an embedder and a query the result is hybrid: `0.6 * cosine + 0.4 * lexical` (lexical
    /// relevance normalized to 0..1 within the result; a memory without a vector scores its
    /// normalized lexical relevance alone). A memory is returned when its cosine reaches
    /// [`COSINE_FLOOR`] (and within [`COSINE_MARGIN`] of the best match) or the lexical search matched it. `embed_timeout` caps the one embedding call
    /// for the query; on timeout or any provider error this falls back to the lexical search.
    pub async fn memory_search_scored(&self, f: MemoryQuery, embed_timeout: Option<Duration>) -> Result<Vec<Scored>> {
        if let (Some(e), Some(text)) = (&self.embedder, f.query.as_deref())
            && !search_terms(text).is_empty()
            && let Some(qv) = embed_query(e.as_ref(), text, embed_timeout).await
        {
            return self.hybrid_search(e.model(), f, &qv).await;
        }
        Ok(self
            .lexical_ranked(f)
            .await?
            .into_iter()
            .map(|(memory, score)| Scored { memory, score, cosine: None })
            .collect())
    }

    async fn hybrid_search(&self, model: &str, f: MemoryQuery, qv: &[f32]) -> Result<Vec<Scored>> {
        let limit = f.limit.unwrap_or(20).clamp(1, 200) as usize;

        struct Entry {
            memory: Memory,
            cosine: Option<f32>,
            lexical: Option<f64>,
        }
        let mut entries: Vec<Entry> = vec![];
        let mut at: HashMap<String, usize> = HashMap::new();

        let mut q = Q::new(&format!(
            "SELECT {MEMORY_COLS_M}, e.vector AS vector FROM memories m \
             LEFT JOIN memory_embeddings e ON e.memory_id = m.id AND e.model = "
        ));
        let model_p = q.text(model);
        q.push(&model_p).push(" WHERE ");
        push_scope(&mut q, &f);
        let n = q.int(SEMANTIC_CANDIDATES);
        q.push(&format!(" ORDER BY m.updated_at DESC LIMIT {n}"));
        for r in q.build().fetch_all(&self.pool).await? {
            let memory = Self::row_to_memory(&r)?;
            let cosine = r
                .try_get::<Option<String>, _>("vector")?
                .and_then(|s| decode_vector(&s))
                .filter(|v| v.len() == qv.len())
                .map(|v| cosine(qv, &v));
            at.insert(memory.id.clone(), entries.len());
            entries.push(Entry { memory, cosine, lexical: None });
        }

        let hits = self.lexical_ranked(MemoryQuery { limit: Some(LEXICAL_HITS), ..f }).await?;
        let max = hits.iter().map(|(_, s)| *s).fold(f64::MIN, f64::max);
        for (memory, score) in hits {
            // relative to the best hit; when no hit has a positive score all count the same
            let norm = if max > 0.0 { (score / max).clamp(0.0, 1.0) } else { 1.0 };
            match at.get(&memory.id) {
                Some(&i) => entries[i].lexical = Some(norm),
                None => entries.push(Entry { memory, cosine: None, lexical: Some(norm) }),
            }
        }

        let best = entries.iter().filter_map(|e| e.cosine).fold(f32::MIN, f32::max);
        let cutoff = COSINE_FLOOR.max(best - COSINE_MARGIN);
        let mut out: Vec<Scored> = entries
            .into_iter()
            .filter(|e| e.lexical.is_some() || e.cosine.is_some_and(|c| c >= cutoff))
            .map(|e| {
                let lexical = e.lexical.unwrap_or(0.0);
                let score = match e.cosine {
                    Some(c) => COSINE_WEIGHT * f64::from(c.max(0.0)) + (1.0 - COSINE_WEIGHT) * lexical,
                    None => lexical,
                };
                Scored { memory: e.memory, score, cosine: e.cosine }
            })
            .collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| b.memory.updated_at.cmp(&a.memory.updated_at)));
        out.truncate(limit);
        Ok(out)
    }
}

/// The query's vector, or `None` (after one warning line) when the provider failed or timed out.
async fn embed_query(e: &dyn Embedder, text: &str, timeout: Option<Duration>) -> Option<Vec<f32>> {
    let texts = [text.chars().take(MAX_QUERY_CHARS).collect::<String>()];
    let result = match timeout {
        Some(t) => match tokio::time::timeout(t, e.embed(&texts)).await {
            Ok(r) => r,
            Err(_) => Err(format!("timed out after {}ms", t.as_millis())),
        },
        None => e.embed(&texts).await,
    };
    match check_vectors(result, 1) {
        Ok(mut v) => Some(v.remove(0)),
        Err(err) => {
            tracing::warn!("semantic search: query embedding failed, using lexical search: {err}");
            None
        }
    }
}

/// A provider answer must hold `n` non-empty vectors of one length.
fn check_vectors(
    r: std::result::Result<Vec<Vec<f32>>, String>,
    n: usize,
) -> std::result::Result<Vec<Vec<f32>>, String> {
    let v = r?;
    if v.len() != n {
        return Err(format!("expected {n} embedding(s), got {}", v.len()));
    }
    if v.iter().any(|x| x.is_empty() || x.len() != v[0].len()) {
        return Err("empty or inconsistent embedding vectors".into());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_round_trip_and_cosine() {
        let v = vec![0.5f32, -1.25, 3.0, 0.0];
        assert_eq!(decode_vector(&encode_vector(&v)), Some(v.clone()));
        assert_eq!(decode_vector("!!"), None);
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0, "different lengths");
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 2.0]), 0.0, "no magnitude");
    }

    #[test]
    fn provider_answers_are_validated() {
        assert!(check_vectors(Ok(vec![vec![1.0]]), 1).is_ok());
        assert!(check_vectors(Ok(vec![vec![1.0]]), 2).is_err());
        assert!(check_vectors(Ok(vec![vec![]]), 1).is_err());
        assert!(check_vectors(Ok(vec![vec![1.0], vec![1.0, 2.0]]), 2).is_err());
        assert_eq!(check_vectors(Err("boom".into()), 1), Err("boom".into()));
    }
}
