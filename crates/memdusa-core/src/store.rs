//! Storage layer. A single portable SQL implementation on top of `sqlx::Any`,
//! so the same code runs against SQLite (local-first) and Postgres (hosted).
//!
//! Portability rules used throughout:
//! * placeholders are `$N` (understood by both drivers),
//! * ids are TEXT (UUIDv7), timestamps are BIGINT unix millis, JSON is TEXT,
//! * never bind a NULL where the server must infer its type (Postgres can't);
//!   optional filters are added to the SQL text instead.

use std::sync::Arc;

use serde_json::Value;
use sqlx::any::{AnyPoolOptions, AnyRow};
use sqlx::{AnyPool, Executor, Row};

use crate::model::*;
use crate::redact::Redactor;
use crate::{Error, Result};

const DEFAULT_LEASE_SECS: i64 = 600;

/// Lease a worker holds on its step of a delegated run (15 minutes); every hook event of the worker renews it.
pub const DEFAULT_STEP_LEASE_SECS: i64 = 900;

/// The step lease length: `MEMDUSA_STEP_LEASE_SECS` (a test/ops override, read where the claim happens, so on the
/// server in remote mode) or [`DEFAULT_STEP_LEASE_SECS`].
pub fn step_lease_secs() -> i64 {
    std::env::var("MEMDUSA_STEP_LEASE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|s| *s >= 0)
        .unwrap_or(DEFAULT_STEP_LEASE_SECS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Sqlite,
    Postgres,
}

#[derive(Clone)]
pub struct Store {
    pub(crate) pool: AnyPool,
    backend: Backend,
    /// Applied to every piece of user/agent text before it is written.
    redactor: Arc<Redactor>,
    /// Semantic search provider; `None` = lexical search only (the default).
    pub(crate) embedder: Option<Arc<dyn crate::embed::Embedder>>,
}

// ---------------------------------------------------------------- tiny query builder

pub(crate) enum Arg {
    Text(Option<String>),
    Int(i64),
    /// A nullable BIGINT. Binding `None::<String>` would make Postgres infer
    /// TEXT and fail against an integer column, even for a NULL.
    IntOpt(Option<i64>),
}

pub(crate) struct Q {
    sql: String,
    args: Vec<Arg>,
}

impl Q {
    pub(crate) fn new(sql: &str) -> Self {
        Self { sql: sql.to_owned(), args: vec![] }
    }
    pub(crate) fn push(&mut self, s: &str) -> &mut Self {
        self.sql.push_str(s);
        self
    }
    pub(crate) fn text(&mut self, v: impl Into<String>) -> String {
        self.args.push(Arg::Text(Some(v.into())));
        format!("${}", self.args.len())
    }
    pub(crate) fn opt_text(&mut self, v: Option<String>) -> String {
        self.args.push(Arg::Text(v));
        format!("${}", self.args.len())
    }
    pub(crate) fn int(&mut self, v: i64) -> String {
        self.args.push(Arg::Int(v));
        format!("${}", self.args.len())
    }
    pub(crate) fn opt_int(&mut self, v: Option<i64>) -> String {
        self.args.push(Arg::IntOpt(v));
        format!("${}", self.args.len())
    }
    pub(crate) fn build(&self) -> sqlx::query::Query<'_, sqlx::Any, sqlx::any::AnyArguments> {
        // every dynamic part is a `$N` placeholder; values are always bound
        let mut q = sqlx::query(sqlx::AssertSqlSafe(self.sql.as_str()));
        for a in &self.args {
            q = match a {
                Arg::Text(v) => q.bind(v.clone()),
                Arg::Int(v) => q.bind(*v),
                Arg::IntOpt(v) => q.bind(*v),
            };
        }
        q
    }
}

/// A caller-supplied timestamp, never later than now.
fn past_or_now(t: Option<Millis>) -> Millis {
    let now = now_ms();
    t.map_or(now, |t| t.min(now))
}

fn to_json(v: &Option<Value>) -> Option<String> {
    v.as_ref().map(|v| v.to_string())
}
fn from_json(s: Option<String>) -> Option<Value> {
    s.and_then(|s| serde_json::from_str(&s).ok())
}
fn list_to_json(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}
fn list_from_json(s: String) -> Vec<String> {
    serde_json::from_str(&s).unwrap_or_default()
}

// ---------------------------------------------------------------- schema

/// One schema migration: statements run in order inside the migration transaction.
enum Migration {
    /// Valid on both SQLite and Postgres.
    Portable(&'static [&'static str]),
    /// Needs backend-specific SQL (full-text search, ...).
    PerBackend { sqlite: &'static [&'static str], postgres: &'static [&'static str] },
}

impl Migration {
    fn statements(&self, backend: Backend) -> &'static [&'static str] {
        match (self, backend) {
            (Self::Portable(s), _) => s,
            (Self::PerBackend { sqlite, .. }, Backend::Sqlite) => sqlite,
            (Self::PerBackend { postgres, .. }, Backend::Postgres) => postgres,
        }
    }
}

/// Columns of `memories` that are read back. Never `SELECT *`: Postgres adds a
/// `search` tsvector column that `sqlx::Any` can't decode.
pub(crate) const MEMORY_COLS: &str = "id, project, kind, content, tags, source, user_name, created_at, updated_at";
/// [`MEMORY_COLS`] qualified with the `m` alias (searches join the FTS table, which has its own `id`/`content`).
pub(crate) const MEMORY_COLS_M: &str =
    "m.id, m.project, m.kind, m.content, m.tags, m.source, m.user_name, m.created_at, m.updated_at";

/// Most distinct terms a search considers.
const MAX_SEARCH_TERMS: usize = 24;

/// The words of a text: lowercase runs of letters/digits, without duplicates
/// (first occurrences, in order). Matches how both backends tokenize.
pub fn words(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && seen.insert(*w))
        .map(str::to_owned)
        .collect()
}

/// The terms a full-text query is built from: the text's words minus stopwords and 1-letter words, at most 24.
/// This is the only form user text takes in a query, so FTS5/tsquery syntax characters can't get
/// through. Stopwords and short words are dropped here (not from the index): matching "the" or
/// "is" says nothing about relevance. A query made only of those yields no terms and matches
/// nothing — falling back to the raw words is exactly the bug this avoids.
pub fn search_terms(text: &str) -> Vec<String> {
    // Two-letter words stay searchable ("CI", "db", "ui", "go") unless they are stopwords; the
    // stricter `significant_words` (3+ letters) is for near-duplicate detection.
    let mut w: Vec<String> =
        words(text).into_iter().filter(|w| w.chars().count() >= 2 && !STOPWORDS.contains(&w.as_str())).collect();
    w.truncate(MAX_SEARCH_TERMS);
    w
}

/// Jaccard similarity from which a saved memory counts as a near-duplicate of an existing one.
pub(crate) const NEAR_DUPLICATE: f64 = 0.75;
/// How many full-text candidates are compared when looking for a near-duplicate.
const NEAR_DUPLICATE_CANDIDATES: i64 = 10;
/// Common English glue and question words. Shared by near-duplicate detection and full-text query
/// building, so neither hinges on "the"/"is" matching. Words shorter than 3 chars are filtered
/// separately by [`significant_words`] (many of the glues below are already covered by that rule,
/// but listing the recognizable ones keeps the intent explicit).
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "was", "were", "with", "that", "this", "from", "has", "have", "had", "its", "not",
    "but", "you", "all", "can", "will", "our", "their", "they", "them", "into", "also", "any", "than", "then", "when",
    "which", "what", "how", "why", "who", "where", "is", "be", "do", "does", "did", "of", "to", "in", "on", "at", "by",
    "it", "as", "an", "or", "if", "so", "we", "us", "me", "my", "your", "there", "here", "about", "should", "would",
    "could", "just", "some", "more", "most", "very", "need", "want", "please", "lets",
    "let", // "let's" tokenizes to "let" + "s"
];

/// The words that carry a text's meaning: [`words`] without short words and stopwords.
/// This is the single filter behind both near-duplicate detection and [`search_terms`].
pub fn significant_words(text: &str) -> Vec<String> {
    words(text).into_iter().filter(|w| w.chars().count() >= 3 && !STOPWORDS.contains(&w.as_str())).collect()
}

/// Two words count as the same when equal, or when one is a prefix (4+ chars) of the
/// other, so "auth" ~ "authentication" and "test" ~ "testing".
fn same_word(a: &str, b: &str) -> bool {
    a == b || (a.chars().count().min(b.chars().count()) >= 4 && (a.starts_with(b) || b.starts_with(a)))
}

/// Jaccard similarity (0..=1) of the significant word sets of two texts.
pub(crate) fn similarity(a: &str, b: &str) -> f64 {
    let (wa, wb) = (significant_words(a), significant_words(b));
    if wa.is_empty() || wb.is_empty() {
        return 0.0;
    }
    let common = wa.iter().filter(|x| wb.iter().any(|y| same_word(x, y))).count();
    common as f64 / (wa.len() + wb.len() - common) as f64
}

/// Append the visibility and kind conditions of a memory search (the SQL must alias `memories` as `m`).
pub(crate) fn push_scope(q: &mut Q, f: &MemoryQuery) {
    let globals = Store::global_visible(q, f.user.as_deref());
    let mut scope = match &f.project {
        Some(p) => {
            let p = q.text(p.as_str());
            format!("m.project = {p}")
        }
        None => "m.project IS NOT NULL".into(),
    };
    // Cross-project listings only include projects the caller can read. An
    // empty allowlist keeps global memories (private to their owner) only.
    if let Some(allowed) = &f.allowed_projects {
        scope = if allowed.is_empty() {
            "1=0".into()
        } else {
            let ps: Vec<String> = allowed.iter().map(|p| q.text(p.as_str())).collect();
            format!("({scope} AND m.project IN ({}))", ps.join(", "))
        };
    }
    q.push(&format!("({scope} OR {globals})"));
    if let Some(k) = f.kind {
        let k = q.text(k.as_str());
        q.push(&format!(" AND m.kind = {k}"));
    }
}

/// Ordered, append-only list of migrations. Never edit a released entry:
/// add a new one (e.g. `ALTER TABLE ... ADD COLUMN ...`). Portable statements
/// must be valid on both SQLite and Postgres.
const MIGRATIONS: &[Migration] = &[
    // 1: initial schema
    Migration::Portable(&[
        "CREATE TABLE IF NOT EXISTS memories (
        id TEXT PRIMARY KEY,
        project TEXT,
        kind TEXT NOT NULL,
        content TEXT NOT NULL,
        tags TEXT NOT NULL,
        source TEXT,
        created_at BIGINT NOT NULL,
        updated_at BIGINT NOT NULL)",
        "CREATE INDEX IF NOT EXISTS memories_project ON memories (project, updated_at)",
        "CREATE TABLE IF NOT EXISTS events (
        id TEXT PRIMARY KEY,
        project TEXT NOT NULL,
        session TEXT,
        source TEXT,
        kind TEXT NOT NULL,
        content TEXT NOT NULL,
        data TEXT,
        created_at BIGINT NOT NULL)",
        "CREATE INDEX IF NOT EXISTS events_project ON events (project, created_at)",
        "CREATE TABLE IF NOT EXISTS checkpoints (
        id TEXT PRIMARY KEY,
        project TEXT NOT NULL,
        task TEXT,
        goal TEXT NOT NULL,
        done TEXT NOT NULL,
        next TEXT NOT NULL,
        blockers TEXT NOT NULL,
        files TEXT NOT NULL,
        notes TEXT,
        source TEXT,
        created_at BIGINT NOT NULL)",
        "CREATE INDEX IF NOT EXISTS checkpoints_project ON checkpoints (project, created_at)",
        "CREATE TABLE IF NOT EXISTS runs (
        id TEXT PRIMARY KEY,
        project TEXT NOT NULL,
        name TEXT NOT NULL,
        idempotency_key TEXT,
        status TEXT NOT NULL,
        input TEXT,
        output TEXT,
        error TEXT,
        owner TEXT,
        lease_until BIGINT,
        lease_secs BIGINT NOT NULL,
        created_at BIGINT NOT NULL,
        updated_at BIGINT NOT NULL)",
        "CREATE UNIQUE INDEX IF NOT EXISTS runs_idem ON runs (project, idempotency_key)",
        "CREATE INDEX IF NOT EXISTS runs_project ON runs (project, status, updated_at)",
        "CREATE TABLE IF NOT EXISTS steps (
        run_id TEXT NOT NULL,
        step_key TEXT NOT NULL,
        seq BIGINT NOT NULL,
        status TEXT NOT NULL,
        input TEXT,
        output TEXT,
        error TEXT,
        attempts BIGINT NOT NULL,
        started_at BIGINT NOT NULL,
        completed_at BIGINT,
        PRIMARY KEY (run_id, step_key))",
    ]),
    // 2: users, API keys, per-user global memories
    Migration::Portable(&[
        "CREATE TABLE users (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            admin BIGINT NOT NULL,
            created_at BIGINT NOT NULL,
            disabled_at BIGINT)",
        "CREATE TABLE api_keys (
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            name TEXT NOT NULL,
            prefix TEXT NOT NULL,
            hash TEXT NOT NULL UNIQUE,
            created_at BIGINT NOT NULL,
            last_used_at BIGINT,
            revoked_at BIGINT)",
        "CREATE INDEX api_keys_user ON api_keys (user_id)",
        "ALTER TABLE memories ADD COLUMN user_name TEXT",
    ]),
    // 3: code state on checkpoints
    Migration::Portable(&["ALTER TABLE checkpoints ADD COLUMN git TEXT"]),
    // 4: ranked full-text search over memories (content + tags)
    Migration::PerBackend {
        // FTS5 table kept in sync by triggers, joined back on `id` (rowids aren't stable
        // across VACUUM for this table). Backfilled from the existing rows.
        sqlite: &[
            "CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(content, tags, id UNINDEXED)",
            "INSERT INTO memories_fts (id, content, tags) SELECT id, content, tags FROM memories",
            "CREATE TRIGGER IF NOT EXISTS memories_fts_ai AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts (id, content, tags) VALUES (new.id, new.content, new.tags);
            END",
            "CREATE TRIGGER IF NOT EXISTS memories_fts_ad AFTER DELETE ON memories BEGIN
                DELETE FROM memories_fts WHERE id = old.id;
            END",
            "CREATE TRIGGER IF NOT EXISTS memories_fts_au AFTER UPDATE OF content, tags ON memories BEGIN
                DELETE FROM memories_fts WHERE id = old.id;
                INSERT INTO memories_fts (id, content, tags) VALUES (new.id, new.content, new.tags);
            END",
        ],
        // A generated column is maintained (and backfilled) by Postgres itself.
        postgres: &[
            "ALTER TABLE memories ADD COLUMN IF NOT EXISTS search tsvector
                GENERATED ALWAYS AS (to_tsvector('simple', content || ' ' || tags)) STORED",
            "CREATE INDEX IF NOT EXISTS memories_search ON memories USING GIN (search)",
        ],
    },
    // 5: per-step leases for delegated runs (steps can be `pending`, claimed by a worker, and go stale)
    Migration::Portable(&[
        "ALTER TABLE steps ADD COLUMN owner TEXT",
        "ALTER TABLE steps ADD COLUMN lease_until BIGINT",
    ]),
    // 6: semantic search. Vectors as base64 text (little-endian f32) so both backends share the code.
    Migration::Portable(&["CREATE TABLE IF NOT EXISTS memory_embeddings (
        memory_id TEXT PRIMARY KEY,
        model TEXT NOT NULL,
        dim BIGINT NOT NULL,
        vector TEXT NOT NULL,
        content_hash TEXT NOT NULL,
        updated_at BIGINT NOT NULL)"]),
    // 7: per-project access control and the audit log. No data changes: the
    // default policy (`write`) keeps existing servers behaving as before.
    Migration::Portable(&[
        "CREATE TABLE IF NOT EXISTS project_grants (
            id TEXT PRIMARY KEY,
            pattern TEXT NOT NULL,
            user_name TEXT NOT NULL,
            role TEXT NOT NULL,
            created_by TEXT NOT NULL,
            created_at BIGINT NOT NULL)",
        "CREATE INDEX IF NOT EXISTS project_grants_user ON project_grants (user_name)",
        "CREATE INDEX IF NOT EXISTS project_grants_pattern ON project_grants (pattern)",
        "CREATE TABLE IF NOT EXISTS audit_events (
            id TEXT PRIMARY KEY,
            at BIGINT NOT NULL,
            actor TEXT NOT NULL,
            action TEXT NOT NULL,
            project TEXT,
            target TEXT,
            detail TEXT,
            outcome TEXT NOT NULL)",
        "CREATE INDEX IF NOT EXISTS audit_events_at ON audit_events (at)",
        "CREATE INDEX IF NOT EXISTS audit_events_actor ON audit_events (actor, at)",
        "CREATE INDEX IF NOT EXISTS audit_events_project ON audit_events (project, at)",
    ]),
    // 8: OAuth 2.1 authorization server (H2a). All credentials are stored as
    // SHA-256 hashes; ids/timestamps follow the same portable conventions.
    Migration::Portable(&[
        "CREATE TABLE IF NOT EXISTS oauth_clients (
            id TEXT PRIMARY KEY,
            client_id TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL,
            redirect_uris TEXT NOT NULL,
            grant_types TEXT NOT NULL,
            created_at BIGINT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS oauth_transactions (
            csrf TEXT PRIMARY KEY,
            client_id TEXT NOT NULL,
            redirect_uri TEXT NOT NULL,
            code_challenge TEXT NOT NULL,
            scope TEXT NOT NULL,
            resource TEXT,
            state TEXT NOT NULL,
            user_name TEXT,
            created_at BIGINT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS oauth_grants (
            id TEXT PRIMARY KEY,
            user_name TEXT NOT NULL,
            client_id TEXT NOT NULL,
            scope TEXT NOT NULL,
            resource TEXT,
            method TEXT NOT NULL,
            created_at BIGINT NOT NULL,
            last_interactive_at BIGINT NOT NULL,
            revoked_at BIGINT)",
        "CREATE INDEX IF NOT EXISTS oauth_grants_user ON oauth_grants (user_name)",
        "CREATE TABLE IF NOT EXISTS oauth_codes (
            id TEXT PRIMARY KEY,
            hash TEXT NOT NULL UNIQUE,
            client_id TEXT NOT NULL,
            redirect_uri TEXT NOT NULL,
            code_challenge TEXT NOT NULL,
            scope TEXT NOT NULL,
            resource TEXT,
            grant_id TEXT NOT NULL,
            expires_at BIGINT NOT NULL,
            used_at BIGINT,
            created_at BIGINT NOT NULL)",
        "CREATE INDEX IF NOT EXISTS oauth_codes_grant ON oauth_codes (grant_id)",
        "CREATE TABLE IF NOT EXISTS oauth_tokens (
            id TEXT PRIMARY KEY,
            hash TEXT NOT NULL UNIQUE,
            grant_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            prefix TEXT NOT NULL,
            scope TEXT NOT NULL,
            resource TEXT,
            expires_at BIGINT NOT NULL,
            rotated_from TEXT,
            revoked_at BIGINT,
            last_used_at BIGINT,
            created_at BIGINT NOT NULL)",
        "CREATE INDEX IF NOT EXISTS oauth_tokens_grant ON oauth_tokens (grant_id)",
        "CREATE TABLE IF NOT EXISTS user_identities (
            id TEXT PRIMARY KEY,
            user_name TEXT NOT NULL,
            provider TEXT NOT NULL,
            subject TEXT NOT NULL,
            email TEXT,
            login TEXT,
            linked_at BIGINT NOT NULL,
            UNIQUE (provider, subject))",
        "CREATE TABLE IF NOT EXISTS oauth_device_codes (
            id TEXT PRIMARY KEY,
            hash TEXT NOT NULL UNIQUE,
            user_code TEXT NOT NULL UNIQUE,
            client_id TEXT NOT NULL,
            scope TEXT NOT NULL,
            resource TEXT,
            expires_at BIGINT NOT NULL,
            interval BIGINT NOT NULL,
            user_name TEXT,
            created_at BIGINT NOT NULL)",
    ]),
];

/// A SQLite URL for a file path that works on every OS. `sqlite://C:\x.db`
/// breaks on Windows (the URL parser reads `C:` as host:port), so use the
/// authority-less form with forward slashes: `sqlite:C:/x.db`, `sqlite:/tmp/x.db`.
pub fn sqlite_url(path: &std::path::Path) -> String {
    format!("sqlite:{}", path.display().to_string().replace('\\', "/"))
}

impl Store {
    /// Connect to `sqlite://path/to.db` or `postgres://...` and apply the schema.
    pub async fn connect(url: &str) -> Result<Self> {
        sqlx::any::install_default_drivers();
        let backend = if url.starts_with("sqlite:") {
            Backend::Sqlite
        } else if url.starts_with("postgres:") || url.starts_with("postgresql:") {
            Backend::Postgres
        } else {
            return Err(Error::Invalid(format!("unsupported database url: {url}")));
        };

        let url = if backend == Backend::Sqlite && !url.contains("mode=") {
            let sep = if url.contains('?') { '&' } else { '?' };
            format!("{url}{sep}mode=rwc")
        } else {
            url.to_owned()
        };

        let pool = AnyPoolOptions::new()
            .max_connections(if backend == Backend::Sqlite { 4 } else { 16 })
            .after_connect(move |conn, _| {
                Box::pin(async move {
                    if backend == Backend::Sqlite {
                        // Several harnesses may open the same file concurrently.
                        conn.execute("PRAGMA busy_timeout = 5000").await?;
                        conn.execute("PRAGMA journal_mode = WAL").await?;
                    }
                    Ok(())
                })
            })
            .connect(&url)
            .await?;

        let store = Self { pool, backend, redactor: Arc::new(Redactor::new(&[])?), embedder: None };
        store.migrate().await?;
        Ok(store)
    }

    /// Replace the default redaction rules (e.g. add custom patterns, or disable).
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = Arc::new(redactor);
        self
    }

    pub fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Bring the schema up to date. Safe to call concurrently from many
    /// processes; returns the resulting schema version.
    pub async fn migrate(&self) -> Result<usize> {
        let mut tx = self.pool.begin().await?;
        if self.backend == Backend::Postgres {
            // Concurrent DDL races in Postgres; serialize migrations across
            // all replicas starting at the same time.
            tx.execute("SELECT pg_advisory_xact_lock(707701)").await?;
        }
        tx.execute("CREATE TABLE IF NOT EXISTS memdusa_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)").await?;
        let current: usize = sqlx::query("SELECT value FROM memdusa_meta WHERE key = 'schema_version'")
            .fetch_optional(&mut *tx)
            .await?
            .map(|r| r.try_get::<String, _>("value"))
            .transpose()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if current > MIGRATIONS.len() {
            return Err(Error::Invalid(format!(
                "database schema v{current} is newer than this binary supports (v{}); upgrade memdusa",
                MIGRATIONS.len()
            )));
        }
        for (i, stmts) in MIGRATIONS.iter().enumerate().skip(current) {
            tracing::info!("applying schema migration v{}", i + 1);
            for stmt in stmts.statements(self.backend) {
                tx.execute(*stmt).await?;
            }
        }
        let mut q = Q::new("INSERT INTO memdusa_meta (key, value) VALUES ('schema_version', ");
        let v = q.text(MIGRATIONS.len().to_string());
        q.push(&v).push(") ON CONFLICT (key) DO UPDATE SET value = excluded.value");
        q.build().execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(MIGRATIONS.len())
    }

    // ------------------------------------------------------------ memory

    pub(crate) fn row_to_memory(r: &AnyRow) -> Result<Memory> {
        Ok(Memory {
            id: r.try_get("id")?,
            project: r.try_get("project")?,
            kind: MemoryKind::parse(&r.try_get::<String, _>("kind")?),
            content: r.try_get("content")?,
            tags: list_from_json(r.try_get("tags")?),
            source: r.try_get("source")?,
            user: r.try_get("user_name")?,
            created_at: r.try_get("created_at")?,
            updated_at: r.try_get("updated_at")?,
            merged: false,
        })
    }

    /// SQL condition: global memories visible to `user`. Globals are private
    /// to their author; those written without a user (local mode) are shared.
    fn global_visible(q: &mut Q, user: Option<&str>) -> String {
        match user {
            Some(u) => {
                let u = q.text(u);
                format!("(project IS NULL AND (user_name IS NULL OR user_name = {u}))")
            }
            None => "(project IS NULL AND user_name IS NULL)".into(),
        }
    }

    /// Save a memory. Saving identical content (same kind and scope) again
    /// refreshes it and merges tags instead of creating a duplicate.
    pub async fn memory_save(&self, mut m: NewMemory) -> Result<Memory> {
        let red = &self.redactor;
        m.tags = red.list(m.tags);
        let content = red.string(m.content.trim().to_owned());
        if content.is_empty() {
            return Err(Error::Invalid("memory content is empty".into()));
        }
        let now = now_ms();

        let mut q = Q::new(&format!("SELECT {MEMORY_COLS} FROM memories WHERE "));
        let (c, k) = (q.text(&content), q.text(m.kind.as_str()));
        q.push(&format!("content = {c} AND kind = {k} AND "));
        match &m.project {
            Some(p) => {
                let p = q.text(p);
                q.push(&format!("project = {p}"))
            }
            None => {
                let g = Self::global_visible(&mut q, m.user.as_deref());
                q.push(&g)
            }
        };
        if let Some(row) = q.build().fetch_optional(&self.pool).await? {
            let mut existing = Self::row_to_memory(&row)?;
            for t in m.tags {
                if !existing.tags.contains(&t) {
                    existing.tags.push(t);
                }
            }
            existing.updated_at = now;
            let mut q = Q::new("UPDATE memories SET ");
            let (t, u, id) = (q.text(list_to_json(&existing.tags)), q.int(now), q.text(&existing.id));
            q.push(&format!("tags = {t}, updated_at = {u} WHERE id = {id}"));
            q.build().execute(&self.pool).await?;
            return Ok(existing);
        }

        // a near-duplicate (same kind and scope) is updated in place: newer wording wins, tags merge.
        // Always lexical: auto-merge must not depend on (or call) an embedding provider.
        let candidates: Vec<Memory> = self
            .lexical_ranked(MemoryQuery {
                query: Some(content.clone()),
                project: m.project.clone(),
                kind: Some(m.kind),
                limit: Some(NEAR_DUPLICATE_CANDIDATES),
                user: m.user.clone(),
                allowed_projects: None,
            })
            .await?
            .into_iter()
            .map(|(m, _)| m)
            .collect();
        if let Some(mut near) = candidates
            .into_iter()
            .find(|c| c.project == m.project && similarity(&c.content, &content) >= NEAR_DUPLICATE)
        {
            for t in m.tags {
                if !near.tags.contains(&t) {
                    near.tags.push(t);
                }
            }
            self.memory_write(&near.id, &content, near.kind, &near.tags, m.source.as_deref(), now).await?;
            self.embed_memory_best_effort(&near.id, &content, &near.tags).await;
            near.content = content;
            near.updated_at = now;
            if m.source.is_some() {
                near.source = m.source;
            }
            near.merged = true;
            return Ok(near);
        }

        let mem = Memory {
            id: new_id(),
            project: m.project,
            kind: m.kind,
            content,
            tags: m.tags,
            source: m.source,
            user: m.user,
            created_at: now,
            updated_at: now,
            merged: false,
        };
        let mut q = Q::new(
            "INSERT INTO memories (id, project, kind, content, tags, source, user_name, created_at, updated_at) VALUES (",
        );
        let vals = [
            q.text(&mem.id),
            q.opt_text(mem.project.clone()),
            q.text(mem.kind.as_str()),
            q.text(&mem.content),
            q.text(list_to_json(&mem.tags)),
            q.opt_text(mem.source.clone()),
            q.opt_text(mem.user.clone()),
            q.int(now),
            q.int(now),
        ];
        q.push(&vals.join(", ")).push(")");
        q.build().execute(&self.pool).await?;
        self.embed_memory_best_effort(&mem.id, &mem.content, &mem.tags).await;
        Ok(mem)
    }

    /// Overwrite a memory's editable columns (`source` only when given) and bump `updated_at`.
    /// The full-text index follows: SQLite triggers on content/tags, a generated column on Postgres.
    async fn memory_write(
        &self,
        id: &str,
        content: &str,
        kind: MemoryKind,
        tags: &[String],
        source: Option<&str>,
        now: Millis,
    ) -> Result<()> {
        let mut q = Q::new("UPDATE memories SET ");
        let (c, k, t, u) = (q.text(content), q.text(kind.as_str()), q.text(list_to_json(tags)), q.int(now));
        q.push(&format!("content = {c}, kind = {k}, tags = {t}, updated_at = {u}"));
        if let Some(s) = source {
            let s = q.text(s);
            q.push(&format!(", source = {s}"));
        }
        let id_p = q.text(id);
        q.push(&format!(" WHERE id = {id_p}"));
        if q.build().execute(&self.pool).await?.rows_affected() == 0 {
            return Err(Error::NotFound(format!("memory {id}")));
        }
        Ok(())
    }

    async fn memory_get(&self, id: &str) -> Result<Memory> {
        let mut q = Q::new(&format!("SELECT {MEMORY_COLS} FROM memories WHERE id = "));
        let id_p = q.text(id);
        q.push(&id_p);
        let row = q.build().fetch_optional(&self.pool).await?.ok_or_else(|| Error::NotFound(format!("memory {id}")))?;
        Self::row_to_memory(&row)
    }

    /// A global memory may only be changed by its owner or an admin; project memories by anyone.
    /// The root identity (no user, admin) is an admin.
    fn memory_check(mem: &Memory, actor: &Identity) -> Result<()> {
        let allowed = actor.admin || mem.project.is_some() || (actor.user.is_some() && mem.user == actor.user);
        if allowed {
            Ok(())
        } else {
            Err(Error::Forbidden("global memories can only be changed by their owner".into()))
        }
    }

    async fn memory_authorize(&self, id: &str, actor: &Identity) -> Result<()> {
        Self::memory_check(&self.memory_get(id).await?, actor)
    }

    /// Correct or refine a memory in place, with full access (local use).
    pub async fn memory_update(&self, id: &str, patch: MemoryPatch) -> Result<Memory> {
        self.memory_update_as(id, patch, &Identity::root()).await
    }

    /// Correct or refine a memory in place on behalf of `actor`. Only the fields set in
    /// `patch` change; content and tags are redacted like on save. A global memory can
    /// only be changed by its owner or an admin (else [`Error::Forbidden`]).
    pub async fn memory_update_as(&self, id: &str, patch: MemoryPatch, actor: &Identity) -> Result<Memory> {
        let mut mem = self.memory_get(id).await?;
        Self::memory_check(&mem, actor)?;

        if let Some(c) = patch.content {
            let c = self.redactor.string(c.trim().to_owned());
            if c.is_empty() {
                return Err(Error::Invalid("memory content is empty".into()));
            }
            mem.content = c;
        }
        if let Some(k) = patch.kind {
            mem.kind = k;
        }
        if let Some(t) = patch.tags {
            mem.tags = self.redactor.list(t);
        }
        if patch.source.is_some() {
            mem.source = patch.source;
        }
        mem.updated_at = now_ms();
        self.memory_write(id, &mem.content, mem.kind, &mem.tags, mem.source.as_deref(), mem.updated_at).await?;
        self.embed_memory_best_effort(id, &mem.content, &mem.tags).await;
        Ok(mem)
    }

    /// Search memories. With a query: full-text, any word may match, best match
    /// first (ties: newest). Without one: the most recently updated.
    pub async fn memory_search(&self, f: MemoryQuery) -> Result<Vec<Memory>> {
        Ok(self.memory_search_ranked(f).await?.into_iter().map(|(m, _)| m).collect())
    }

    /// Like [`Store::memory_search`], with each memory's relevance score
    /// (higher is better, comparable within one result set only; 0 when there
    /// was no query).
    ///
    /// With an embedder configured and a query, the search is semantic + lexical, see
    /// [`Store::memory_search_scored`]; otherwise (or when the provider fails) it is purely lexical.
    pub async fn memory_search_ranked(&self, f: MemoryQuery) -> Result<Vec<(Memory, f64)>> {
        Ok(self.memory_search_scored(f, None).await?.into_iter().map(|s| (s.memory, s.score)).collect())
    }

    /// Full-text search only (FTS5 / tsvector): the behaviour without an embedder.
    pub(crate) async fn lexical_ranked(&self, f: MemoryQuery) -> Result<Vec<(Memory, f64)>> {
        let terms = f.query.as_deref().map(search_terms);
        let limit = f.limit.unwrap_or(20).clamp(1, 200);
        let ranked = match &terms {
            // a query with nothing searchable in it (only punctuation, stopwords or short words)
            // matches nothing; using the raw words instead is what made "the"/"is" match everything
            Some(t) if t.is_empty() && f.query.as_deref().is_some_and(|s| !s.trim().is_empty()) => return Ok(vec![]),
            Some(t) if !t.is_empty() => Some(t),
            _ => None,
        };

        let mut q;
        match ranked {
            Some(terms) => {
                // Terms are plain alphanumeric words (see `search_terms`): quoted for FTS5,
                // bare for tsquery. User text is never passed on as query syntax.
                match self.backend {
                    Backend::Sqlite => {
                        q = Q::new(&format!(
                            "SELECT {MEMORY_COLS_M}, -bm25(memories_fts, 1.0, 0.5, 0.0) AS score \
                             FROM memories_fts JOIN memories m ON m.id = memories_fts.id WHERE memories_fts MATCH "
                        ));
                        let m = q.text(terms.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(" OR "));
                        q.push(&m).push(" AND ");
                    }
                    Backend::Postgres => {
                        q = Q::new("SELECT ");
                        let tsq = q.text(terms.join(" | "));
                        q.push(&format!(
                            "{MEMORY_COLS_M}, CAST(ts_rank(m.search, to_tsquery('simple', CAST({tsq} AS TEXT))) AS DOUBLE PRECISION) AS score \
                             FROM memories m WHERE m.search @@ to_tsquery('simple', CAST({tsq} AS TEXT)) AND "
                        ));
                    }
                }
            }
            None => {
                q = Q::new(&format!(
                    "SELECT {MEMORY_COLS_M}, CAST(0 AS DOUBLE PRECISION) AS score FROM memories m WHERE "
                ))
            }
        }

        push_scope(&mut q, &f);
        let limit = q.int(limit);
        if ranked.is_some() {
            q.push(&format!(" ORDER BY score DESC, m.updated_at DESC LIMIT {limit}"));
        } else {
            q.push(&format!(" ORDER BY m.updated_at DESC LIMIT {limit}"));
        }
        let rows = q.build().fetch_all(&self.pool).await?;
        rows.iter().map(|r| Ok((Self::row_to_memory(r)?, r.try_get::<f64, _>("score")?))).collect()
    }

    /// Delete a memory, with full access (local use).
    pub async fn memory_forget(&self, id: &str) -> Result<()> {
        self.memory_forget_as(id, &Identity::root()).await
    }

    /// Delete a memory on behalf of `actor`; see [`Store::memory_update_as`] for who may.
    pub async fn memory_forget_as(&self, id: &str, actor: &Identity) -> Result<()> {
        self.memory_authorize(id, actor).await?;
        let mut q = Q::new("DELETE FROM memories WHERE id = ");
        let id_p = q.text(id);
        q.push(&id_p);
        if q.build().execute(&self.pool).await?.rows_affected() == 0 {
            return Err(Error::NotFound(format!("memory {id}")));
        }
        self.embedding_delete(id).await?;
        Ok(())
    }

    // ------------------------------------------------------------ journal

    pub async fn event_append(&self, e: NewEvent) -> Result<Event> {
        let ev = Event {
            id: new_id(),
            project: e.project,
            session: e.session,
            source: e.source,
            kind: e.kind,
            content: self.redactor.string(e.content),
            data: self.redactor.opt_json(e.data),
            created_at: past_or_now(e.created_at),
        };
        let mut q =
            Q::new("INSERT INTO events (id, project, session, source, kind, content, data, created_at) VALUES (");
        let vals = [
            q.text(&ev.id),
            q.text(&ev.project),
            q.opt_text(ev.session.clone()),
            q.opt_text(ev.source.clone()),
            q.text(&ev.kind),
            q.text(&ev.content),
            q.opt_text(to_json(&ev.data)),
            q.int(ev.created_at),
        ];
        q.push(&vals.join(", ")).push(")");
        q.build().execute(&self.pool).await?;
        Ok(ev)
    }

    /// Most recent events first.
    pub async fn events_recent(&self, project: &str, session: Option<&str>, limit: i64) -> Result<Vec<Event>> {
        self.events_query(project, session, None, limit).await
    }

    /// Most recent events of one `kind` first (so busy kinds can't push them out of a window).
    pub async fn events_recent_kind(
        &self,
        project: &str,
        session: Option<&str>,
        kind: &str,
        limit: i64,
    ) -> Result<Vec<Event>> {
        self.events_query(project, session, Some(kind), limit).await
    }

    /// Events of one `kind` belonging to any of `sessions`, oldest first. Unlike
    /// [`Self::events_recent`] this does not go through the "N most recent events"
    /// window, so an old event (e.g. a worker's original assignment) survives a
    /// long, busy task. Portable `Q` SQL: SQLite + Postgres.
    pub async fn events_for_sessions_kind(
        &self,
        project: &str,
        sessions: &[&str],
        kind: &str,
        limit: i64,
    ) -> Result<Vec<Event>> {
        if sessions.is_empty() {
            return Ok(vec![]);
        }
        let mut q = Q::new("SELECT * FROM events WHERE project = ");
        let p = q.text(project);
        q.push(&p);
        let k = q.text(kind);
        q.push(&format!(" AND kind = {k} AND session IN ("));
        let vals: Vec<String> = sessions.iter().map(|s| q.text(*s)).collect();
        q.push(&vals.join(", ")).push(")");
        let l = q.int(limit.clamp(1, 500));
        q.push(&format!(" ORDER BY created_at ASC, id ASC LIMIT {l}"));
        q.build()
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| {
                Ok(Event {
                    id: r.try_get("id")?,
                    project: r.try_get("project")?,
                    session: r.try_get("session")?,
                    source: r.try_get("source")?,
                    kind: r.try_get("kind")?,
                    content: r.try_get("content")?,
                    data: from_json(r.try_get("data")?),
                    created_at: r.try_get("created_at")?,
                })
            })
            .collect()
    }

    async fn events_query(
        &self,
        project: &str,
        session: Option<&str>,
        kind: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Event>> {
        let mut q = Q::new("SELECT * FROM events WHERE project = ");
        let p = q.text(project);
        q.push(&p);
        if let Some(s) = session {
            let s = q.text(s);
            q.push(&format!(" AND session = {s}"));
        }
        if let Some(k) = kind {
            let k = q.text(k);
            q.push(&format!(" AND kind = {k}"));
        }
        let l = q.int(limit.clamp(1, 500));
        q.push(&format!(" ORDER BY created_at DESC, id DESC LIMIT {l}"));
        q.build()
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| {
                Ok(Event {
                    id: r.try_get("id")?,
                    project: r.try_get("project")?,
                    session: r.try_get("session")?,
                    source: r.try_get("source")?,
                    kind: r.try_get("kind")?,
                    content: r.try_get("content")?,
                    data: from_json(r.try_get("data")?),
                    created_at: r.try_get("created_at")?,
                })
            })
            .collect()
    }

    // ------------------------------------------------------------ checkpoints

    pub async fn checkpoint_save(&self, c: NewCheckpoint) -> Result<Checkpoint> {
        let cp = Checkpoint {
            id: new_id(),
            project: c.project,
            task: c.task,
            goal: self.redactor.string(c.goal),
            done: self.redactor.list(c.done),
            next: self.redactor.list(c.next),
            blockers: self.redactor.list(c.blockers),
            files: c.files,
            notes: self.redactor.opt(c.notes),
            source: c.source,
            git: c.git,
            created_at: past_or_now(c.created_at),
        };
        let mut q = Q::new(
            "INSERT INTO checkpoints (id, project, task, goal, done, next, blockers, files, notes, source, git, created_at) VALUES (",
        );
        let vals = [
            q.text(&cp.id),
            q.text(&cp.project),
            q.opt_text(cp.task.clone()),
            q.text(&cp.goal),
            q.text(list_to_json(&cp.done)),
            q.text(list_to_json(&cp.next)),
            q.text(list_to_json(&cp.blockers)),
            q.text(list_to_json(&cp.files)),
            q.opt_text(cp.notes.clone()),
            q.opt_text(cp.source.clone()),
            q.opt_text(cp.git.as_ref().and_then(|g| serde_json::to_string(g).ok())),
            q.int(cp.created_at),
        ];
        q.push(&vals.join(", ")).push(")");
        q.build().execute(&self.pool).await?;
        Ok(cp)
    }

    /// When the most recent checkpoint (any task, any agent) was saved.
    pub async fn last_checkpoint_at(&self, project: &str) -> Result<Option<Millis>> {
        let mut q = Q::new("SELECT MAX(created_at) AS t FROM checkpoints WHERE project = ");
        let p = q.text(project);
        q.push(&p);
        let row = q.build().fetch_one(&self.pool).await?;
        Ok(row.try_get::<Option<i64>, _>("t")?)
    }

    /// Number of events of `kind` in `session` since `since` (exclusive).
    pub async fn count_events_since(&self, project: &str, session: &str, kind: &str, since: Millis) -> Result<i64> {
        let mut q = Q::new("SELECT COUNT(*) AS n FROM events WHERE project = ");
        let (p, s, k, t) = (q.text(project), q.text(session), q.text(kind), q.int(since));
        q.push(&format!("{p} AND session = {s} AND kind = {k} AND created_at > {t}"));
        Ok(q.build().fetch_one(&self.pool).await?.try_get::<i64, _>("n")?)
    }

    /// Latest checkpoint per task (most recent first).
    pub async fn checkpoints_latest(&self, project: &str, limit: i64) -> Result<Vec<Checkpoint>> {
        let mut q = Q::new("SELECT * FROM checkpoints WHERE project = ");
        let p = q.text(project);
        q.push(&p).push(" ORDER BY created_at DESC, id DESC LIMIT 200");
        let rows = q.build().fetch_all(&self.pool).await?;
        let mut seen = std::collections::HashSet::new();
        let mut out = vec![];
        for r in rows {
            let task: Option<String> = r.try_get("task")?;
            if !seen.insert(task.clone()) {
                continue;
            }
            out.push(Self::row_to_checkpoint(&r, task)?);
            if out.len() as i64 >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// The newest checkpoints of the project, all tasks included, most recent first.
    /// Unlike [`Self::checkpoints_latest`] this keeps older checkpoints of the same task, so a
    /// caller can find the last checkpoint saved in its own worktree (which may be an older one).
    pub async fn checkpoints_recent(&self, project: &str, limit: i64) -> Result<Vec<Checkpoint>> {
        let mut q = Q::new("SELECT * FROM checkpoints WHERE project = ");
        let (p, l) = (q.text(project), q.int(limit.clamp(1, 500)));
        q.push(&format!("{p} ORDER BY created_at DESC, id DESC LIMIT {l}"));
        q.build().fetch_all(&self.pool).await?.iter().map(|r| Self::row_to_checkpoint(r, r.try_get("task")?)).collect()
    }

    /// The newest checkpoints of one task, most recent first.
    pub async fn checkpoints_for_task(&self, project: &str, task: &str, limit: i64) -> Result<Vec<Checkpoint>> {
        let mut q = Q::new("SELECT * FROM checkpoints WHERE project = ");
        let (p, t, l) = (q.text(project), q.text(task), q.int(limit.clamp(1, 50)));
        q.push(&format!("{p} AND task = {t} ORDER BY created_at DESC, id DESC LIMIT {l}"));
        q.build()
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| Self::row_to_checkpoint(r, Some(task.to_owned())))
            .collect()
    }

    fn row_to_checkpoint(r: &AnyRow, task: Option<String>) -> Result<Checkpoint> {
        Ok(Checkpoint {
            id: r.try_get("id")?,
            project: r.try_get("project")?,
            task,
            goal: r.try_get("goal")?,
            done: list_from_json(r.try_get("done")?),
            next: list_from_json(r.try_get("next")?),
            blockers: list_from_json(r.try_get("blockers")?),
            files: list_from_json(r.try_get("files")?),
            notes: r.try_get("notes")?,
            source: r.try_get("source")?,
            git: r.try_get::<Option<String>, _>("git")?.and_then(|s| serde_json::from_str(&s).ok()),
            created_at: r.try_get("created_at")?,
        })
    }

    // ------------------------------------------------------------ durable runs

    fn row_to_run(r: &AnyRow) -> Result<Run> {
        let status = RunStatus::parse(&r.try_get::<String, _>("status")?);
        let lease_until: Option<i64> = r.try_get("lease_until")?;
        let input = from_json(r.try_get("input")?);
        // a delegated run has no run-level lease: its steps carry the leases
        let stale = status == RunStatus::Running
            && !is_delegated_input(input.as_ref())
            && lease_until.is_none_or(|l| l <= now_ms());
        Ok(Run {
            id: r.try_get("id")?,
            project: r.try_get("project")?,
            name: r.try_get("name")?,
            idempotency_key: r.try_get("idempotency_key")?,
            status,
            input,
            output: from_json(r.try_get("output")?),
            error: r.try_get("error")?,
            owner: r.try_get("owner")?,
            lease_until,
            created_at: r.try_get("created_at")?,
            updated_at: r.try_get("updated_at")?,
            stale,
        })
    }

    fn row_to_step(r: &AnyRow) -> Result<Step> {
        let status = StepStatus::parse(&r.try_get::<String, _>("status")?);
        let lease_until: Option<i64> = r.try_get("lease_until")?;
        Ok(Step {
            run_id: r.try_get("run_id")?,
            key: r.try_get("step_key")?,
            seq: r.try_get("seq")?,
            status,
            input: from_json(r.try_get("input")?),
            output: from_json(r.try_get("output")?),
            error: r.try_get("error")?,
            attempts: r.try_get("attempts")?,
            started_at: r.try_get("started_at")?,
            completed_at: r.try_get("completed_at")?,
            owner: r.try_get("owner")?,
            lease_until,
            // a running step without a lease belongs to a single-agent run, whose run-level lease governs it
            stale: status == StepStatus::Running && lease_until.is_some_and(|l| l < now_ms()),
        })
    }

    async fn run_find(&self, id: &str) -> Result<Run> {
        let mut q = Q::new("SELECT * FROM runs WHERE id = ");
        let p = q.text(id);
        q.push(&p);
        let row = q.build().fetch_optional(&self.pool).await?.ok_or_else(|| Error::NotFound(format!("run {id}")))?;
        Self::row_to_run(&row)
    }

    pub async fn run_get(&self, id: &str) -> Result<RunWithSteps> {
        let run = self.run_find(id).await?;
        let mut q = Q::new("SELECT * FROM steps WHERE run_id = ");
        let p = q.text(id);
        q.push(&p).push(" ORDER BY seq");
        let steps = q.build().fetch_all(&self.pool).await?.iter().map(Self::row_to_step).collect::<Result<_>>()?;
        Ok(RunWithSteps { run, steps })
    }

    /// Start a run, or return the existing one with the same idempotency key.
    pub async fn run_start(&self, mut r: NewRun) -> Result<RunWithSteps> {
        r.name = self.redactor.string(r.name);
        r.input = self.redactor.opt_json(r.input);
        let owner = r.owner.unwrap_or_else(|| "anonymous".into());
        let lease = r.lease_secs.unwrap_or(DEFAULT_LEASE_SECS).max(0);
        if let Some(key) = &r.idempotency_key {
            let mut q = Q::new("SELECT id FROM runs WHERE project = ");
            let (p, k) = (q.text(&r.project), q.text(key));
            q.push(&format!("{p} AND idempotency_key = {k}"));
            if let Some(row) = q.build().fetch_optional(&self.pool).await? {
                let id: String = row.try_get("id")?;
                // Re-entering an existing run: try to take the lease, but a
                // terminal or live-leased run is simply returned as-is.
                return match self.run_claim(&id, &owner, r.lease_secs, false).await {
                    Ok(r) => Ok(r),
                    Err(Error::Conflict(_)) => self.run_get(&id).await,
                    Err(e) => Err(e),
                };
            }
        }
        let now = now_ms();
        let id = new_id();
        let mut q = Q::new(
            "INSERT INTO runs (id, project, name, idempotency_key, status, input, owner, lease_until, lease_secs, created_at, updated_at) VALUES (",
        );
        let vals = [
            q.text(&id),
            q.text(&r.project),
            q.text(&r.name),
            q.opt_text(r.idempotency_key.clone()),
            q.text(RunStatus::Running.as_str()),
            q.opt_text(to_json(&r.input)),
            q.text(&owner),
            q.int(now + lease * 1000),
            q.int(lease),
            q.int(now),
            q.int(now),
        ];
        q.push(&vals.join(", ")).push(")");
        match q.build().execute(&self.pool).await {
            Ok(_) => self.run_get(&id).await,
            // lost a race on the idempotency key: return the winner
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                Box::pin(self.run_start(NewRun { owner: Some(owner), ..r })).await
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Take (or renew) the lease on a running run. Succeeds when the run is
    /// unowned, already owned by `owner`, its lease expired, or `force` is set.
    /// `lease_secs` changes the run's lease length; `None` keeps the stored one.
    pub async fn run_claim(&self, id: &str, owner: &str, lease_secs: Option<i64>, force: bool) -> Result<RunWithSteps> {
        let now = now_ms();
        let mut q = Q::new("UPDATE runs SET ");
        let o = q.text(owner);
        let n = q.int(now);
        match lease_secs {
            Some(secs) => {
                let secs = q.int(secs.max(0));
                q.push(&format!("lease_secs = {secs}, lease_until = {n} + {secs} * 1000, "))
            }
            None => q.push(&format!("lease_until = {n} + lease_secs * 1000, ")),
        };
        let (i, s) = (q.text(id), q.text(RunStatus::Running.as_str()));
        q.push(&format!("owner = {o}, updated_at = {n} WHERE id = {i} AND status = {s}"));
        if !force {
            q.push(&format!(" AND (owner IS NULL OR owner = {o} OR lease_until IS NULL OR lease_until <= {n})"));
        }
        if q.build().execute(&self.pool).await?.rows_affected() == 0 {
            let run = self.run_find(id).await?;
            return Err(if run.status.is_terminal() {
                Error::Conflict(format!("run {id} is already {}", run.status.as_str()))
            } else {
                Error::Conflict(format!(
                    "run {id} is leased by '{}' for another {}s; wait, or claim with force=true if that agent is gone",
                    run.owner.unwrap_or_default(),
                    (run.lease_until.unwrap_or(now) - now) / 1000
                ))
            });
        }
        self.run_get(id).await
    }

    /// Enter a step. Completed steps are replayed from the journal; anything
    /// else (new, crashed mid-way, failed) must be executed by the caller.
    pub async fn step_begin(&self, run_id: &str, key: &str, input: Option<Value>, owner: &str) -> Result<StepDecision> {
        let input = self.redactor.opt_json(input);
        self.run_claim(run_id, owner, None, false).await?;

        let mut q = Q::new("SELECT * FROM steps WHERE run_id = ");
        let (r, k) = (q.text(run_id), q.text(key));
        q.push(&format!("{r} AND step_key = {k}"));
        if let Some(row) = q.build().fetch_optional(&self.pool).await? {
            let step = Self::row_to_step(&row)?;
            if step.status == StepStatus::Completed {
                return Ok(StepDecision::Replay { output: step.output });
            }
            let mut q = Q::new("UPDATE steps SET ");
            let (s, n, i, r, k) = (
                q.text(StepStatus::Running.as_str()),
                q.int(now_ms()),
                q.opt_text(to_json(&input).or(to_json(&step.input))),
                q.text(run_id),
                q.text(key),
            );
            q.push(&format!(
                "status = {s}, attempts = attempts + 1, started_at = {n}, input = {i} WHERE run_id = {r} AND step_key = {k}"
            ));
            q.build().execute(&self.pool).await?;
            return Ok(StepDecision::Execute { attempt: step.attempts + 1, previous_error: step.error });
        }

        let mut q = Q::new("INSERT INTO steps (run_id, step_key, seq, status, input, attempts, started_at) VALUES (");
        let r = q.text(run_id);
        let vals = [
            r.clone(),
            q.text(key),
            format!("(SELECT COUNT(*) + 1 FROM steps WHERE run_id = {r})"),
            q.text(StepStatus::Running.as_str()),
            q.opt_text(to_json(&input)),
            q.int(1),
            q.int(now_ms()),
        ];
        q.push(&vals.join(", ")).push(") ON CONFLICT (run_id, step_key) DO NOTHING");
        q.build().execute(&self.pool).await?;
        Ok(StepDecision::Execute { attempt: 1, previous_error: None })
    }

    /// Record a step's result. The journal is append-only: completing an
    /// already-completed step keeps the first recorded output.
    /// Outputs are redacted too: a replayed step returns the redacted output.
    pub async fn step_complete(&self, run_id: &str, key: &str, output: Option<Value>) -> Result<Step> {
        let output = self.redactor.opt_json(output);
        let mut q = Q::new("UPDATE steps SET ");
        let (s, o, n, r, k, done) = (
            q.text(StepStatus::Completed.as_str()),
            q.opt_text(to_json(&output)),
            q.int(now_ms()),
            q.text(run_id),
            q.text(key),
            q.text(StepStatus::Completed.as_str()),
        );
        q.push(&format!(
            "status = {s}, output = {o}, error = NULL, completed_at = {n}, lease_until = NULL \
             WHERE run_id = {r} AND step_key = {k} AND status <> {done}"
        ));
        q.build().execute(&self.pool).await?;
        self.touch_run(run_id).await?;
        self.step_get(run_id, key).await
    }

    pub async fn step_fail(&self, run_id: &str, key: &str, error: &str) -> Result<Step> {
        let error = self.redactor.text(error).into_owned();
        let error = error.as_str();
        let mut q = Q::new("UPDATE steps SET ");
        let (s, e, r, k, done) = (
            q.text(StepStatus::Failed.as_str()),
            q.text(error),
            q.text(run_id),
            q.text(key),
            q.text(StepStatus::Completed.as_str()),
        );
        q.push(&format!(
            "status = {s}, error = {e}, lease_until = NULL WHERE run_id = {r} AND step_key = {k} AND status <> {done}"
        ));
        q.build().execute(&self.pool).await?;
        self.touch_run(run_id).await?;
        self.step_get(run_id, key).await
    }

    /// Bump a run's `updated_at` (steps changed), without touching its lease.
    async fn touch_run(&self, run_id: &str) -> Result<()> {
        let mut q = Q::new("UPDATE runs SET ");
        let (n, r) = (q.int(now_ms()), q.text(run_id));
        q.push(&format!("updated_at = {n} WHERE id = {r}"));
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    // ------------------------------------------------------------ delegated runs

    /// Delegate `tasks` as one durable run: each task is a `pending` step (its key is the task name) that a
    /// worker later claims with [`Store::step_claim`]. Returns the existing *running* run with the same
    /// idempotency key (or, without a key, the same name), adding the tasks it lacks.
    pub async fn run_delegate(
        &self,
        project: &str,
        name: &str,
        tasks: Vec<String>,
        idempotency_key: Option<String>,
    ) -> Result<RunWithSteps> {
        let mut keys: Vec<String> = vec![];
        for t in tasks {
            let t = self.redactor.string(t.trim().to_owned());
            if !t.is_empty() && !keys.contains(&t) {
                keys.push(t);
            }
        }
        if keys.is_empty() {
            return Err(Error::Invalid("delegate at least one task".into()));
        }
        let name = self.redactor.string(name.trim().to_owned());
        if name.is_empty() {
            return Err(Error::Invalid("a delegated run needs a name".into()));
        }

        let existing = match &idempotency_key {
            Some(k) => {
                let mut q = Q::new("SELECT id FROM runs WHERE project = ");
                let (p, k) = (q.text(project), q.text(k));
                q.push(&format!("{p} AND idempotency_key = {k}"));
                q.build().fetch_optional(&self.pool).await?
            }
            None => {
                let mut q = Q::new("SELECT id, input FROM runs WHERE project = ");
                let (p, n, s) = (q.text(project), q.text(&name), q.text(RunStatus::Running.as_str()));
                q.push(&format!("{p} AND name = {n} AND status = {s} ORDER BY created_at DESC LIMIT 20"));
                q.build().fetch_all(&self.pool).await?.into_iter().find(|r| {
                    r.try_get::<Option<String>, _>("input")
                        .ok()
                        .flatten()
                        .is_some_and(|i| is_delegated_input(from_json(Some(i)).as_ref()))
                })
            }
        };
        let id = match existing {
            Some(row) => row.try_get::<String, _>("id")?,
            None => {
                let now = now_ms();
                let id = new_id();
                let input = serde_json::json!({ DELEGATED_MARKER: true });
                let mut q = Q::new(
                    "INSERT INTO runs (id, project, name, idempotency_key, status, input, lease_secs, created_at, updated_at) VALUES (",
                );
                let vals = [
                    q.text(&id),
                    q.text(project),
                    q.text(&name),
                    q.opt_text(idempotency_key.clone()),
                    q.text(RunStatus::Running.as_str()),
                    q.text(input.to_string()),
                    q.int(DEFAULT_LEASE_SECS),
                    q.int(now),
                    q.int(now),
                ];
                q.push(&vals.join(", ")).push(")");
                match q.build().execute(&self.pool).await {
                    Ok(_) => id,
                    // lost a race on the idempotency key: the winner's run is the one
                    Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                        return Box::pin(self.run_delegate(project, &name, keys, idempotency_key)).await;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        };

        let run = self.run_find(&id).await?;
        if run.status == RunStatus::Running {
            for key in &keys {
                let mut q = Q::new("INSERT INTO steps (run_id, step_key, seq, status, attempts, started_at) VALUES (");
                let r = q.text(&id);
                let vals = [
                    r.clone(),
                    q.text(key),
                    format!("(SELECT COUNT(*) + 1 FROM steps WHERE run_id = {r})"),
                    q.text(StepStatus::Pending.as_str()),
                    q.int(0),
                    q.int(now_ms()),
                ];
                q.push(&vals.join(", ")).push(") ON CONFLICT (run_id, step_key) DO NOTHING");
                q.build().execute(&self.pool).await?;
            }
            self.touch_run(&id).await?;
        }
        self.run_get(&id).await
    }

    /// A worker claims its step of a delegated run: `Claimed` (pending, failed, stale, or already the caller's),
    /// `AlreadyDone` (never redo it), or `HeldBy` another worker's live lease, which `force` takes anyway.
    pub async fn step_claim(
        &self,
        run_id: &str,
        key: &str,
        owner: &str,
        lease_secs: i64,
        force: bool,
    ) -> Result<ClaimResult> {
        let before = self.step_get(run_id, key).await?;
        if before.status == StepStatus::Completed {
            return Ok(ClaimResult::AlreadyDone { output: before.output });
        }
        let run = self.run_find(run_id).await?;
        if run.status.is_terminal() {
            return Err(Error::Conflict(format!("run {run_id} is already {}", run.status.as_str())));
        }

        let now = now_ms();
        let ours = before.status == StepStatus::Running && before.owner.as_deref() == Some(owner);
        let mut q = Q::new("UPDATE steps SET ");
        let (s, o, n, secs, running, done) = (
            q.text(StepStatus::Running.as_str()),
            q.text(owner),
            q.int(now),
            q.int(lease_secs.max(0)),
            q.text(StepStatus::Running.as_str()),
            q.text(StepStatus::Completed.as_str()),
        );
        // renewing our own lease isn't a new attempt
        q.push(&format!(
            "status = {s}, owner = {o}, lease_until = {n} + {secs} * 1000, \
             attempts = attempts + (CASE WHEN status = {running} AND owner = {o} THEN 0 ELSE 1 END), \
             started_at = (CASE WHEN status = {running} AND owner = {o} THEN started_at ELSE {n} END)"
        ));
        let (r, k) = (q.text(run_id), q.text(key));
        q.push(&format!(" WHERE run_id = {r} AND step_key = {k} AND status <> {done}"));
        if !force {
            q.push(&format!(
                " AND (status <> {running} OR owner IS NULL OR owner = {o} OR lease_until IS NULL OR lease_until <= {n})"
            ));
        }
        if q.build().execute(&self.pool).await?.rows_affected() == 0 {
            // someone else got there first
            let now_step = self.step_get(run_id, key).await?;
            return Ok(match now_step.status {
                StepStatus::Completed => ClaimResult::AlreadyDone { output: now_step.output },
                _ => ClaimResult::HeldBy {
                    owner: now_step.owner.unwrap_or_default(),
                    lease_until: now_step.lease_until.unwrap_or(now),
                },
            });
        }
        self.touch_run(run_id).await?;
        let took_over = before.status == StepStatus::Running && !ours;
        Ok(ClaimResult::Claimed {
            attempt: if ours { before.attempts } else { before.attempts + 1 },
            previous_owner: if took_over { before.owner } else { None },
            stale: took_over && before.lease_until.is_none_or(|l| l <= now),
            previous_error: before.error,
        })
    }

    /// Extend the lease of a step the caller still owns. `false` when it doesn't (taken over, finished, unknown).
    pub async fn step_heartbeat(&self, run_id: &str, key: &str, owner: &str, lease_secs: i64) -> Result<bool> {
        let mut q = Q::new("UPDATE steps SET ");
        let (n, secs) = (q.int(now_ms()), q.int(lease_secs.max(0)));
        let (r, k, o, running) = (q.text(run_id), q.text(key), q.text(owner), q.text(StepStatus::Running.as_str()));
        q.push(&format!(
            "lease_until = {n} + {secs} * 1000 WHERE run_id = {r} AND step_key = {k} AND owner = {o} AND status = {running}"
        ));
        Ok(q.build().execute(&self.pool).await?.rows_affected() > 0)
    }

    async fn step_get(&self, run_id: &str, key: &str) -> Result<Step> {
        let mut q = Q::new("SELECT * FROM steps WHERE run_id = ");
        let (r, k) = (q.text(run_id), q.text(key));
        q.push(&format!("{r} AND step_key = {k}"));
        let row = q
            .build()
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| Error::NotFound(format!("step '{key}' in run {run_id} (call step_begin first)")))?;
        Self::row_to_step(&row)
    }

    pub async fn run_finish(
        &self,
        id: &str,
        outcome: RunOutcome,
        output: Option<Value>,
        error: Option<String>,
    ) -> Result<RunWithSteps> {
        let (output, error) = (self.redactor.opt_json(output), self.redactor.opt(error));
        let mut q = Q::new("UPDATE runs SET ");
        let (s, o, e, n, i, running) = (
            q.text(RunStatus::from(outcome).as_str()),
            q.opt_text(to_json(&output)),
            q.opt_text(error),
            q.int(now_ms()),
            q.text(id),
            q.text(RunStatus::Running.as_str()),
        );
        q.push(&format!(
            "status = {s}, output = {o}, error = {e}, owner = NULL, lease_until = NULL, updated_at = {n} WHERE id = {i} AND status = {running}"
        ));
        if q.build().execute(&self.pool).await?.rows_affected() == 0 {
            let run = self.run_find(id).await?;
            return Err(Error::Conflict(format!("run {id} is already {}", run.status.as_str())));
        }
        self.run_get(id).await
    }

    /// (running, stale) run counts, for metrics.
    pub async fn run_counts(&self) -> Result<(i64, i64)> {
        // delegated runs hold no run-level lease (their steps do), so they can't go stale as a whole
        let mut q = Q::new(&format!(
            "SELECT COUNT(*) AS n, COALESCE(SUM(CASE WHEN (input IS NULL OR input NOT LIKE '%{DELEGATED_MARKER}%') \
             AND (lease_until IS NULL OR lease_until <= "
        ));
        let (now, running) = (q.int(now_ms()), q.text(RunStatus::Running.as_str()));
        q.push(&format!("{now}) THEN 1 ELSE 0 END), 0) AS stale FROM runs WHERE status = {running}"));
        let row = q.build().fetch_one(&self.pool).await?;
        Ok((row.try_get("n")?, row.try_get("stale")?))
    }

    pub async fn run_list(&self, project: Option<&str>, status: Option<RunStatus>, limit: i64) -> Result<Vec<Run>> {
        self.run_list_visible(project, None, status, limit).await
    }

    /// Like [`Self::run_list`], but restricted to `allowed` projects (used by
    /// cross-project listings on a multi-user server).
    pub async fn run_list_visible(
        &self,
        project: Option<&str>,
        allowed: Option<&[String]>,
        status: Option<RunStatus>,
        limit: i64,
    ) -> Result<Vec<Run>> {
        let mut q = Q::new("SELECT * FROM runs WHERE 1=1");
        if let Some(p) = project {
            let p = q.text(p);
            q.push(&format!(" AND project = {p}"));
        }
        if let Some(allowed) = allowed {
            if allowed.is_empty() {
                return Ok(vec![]);
            }
            let ps: Vec<String> = allowed.iter().map(|p| q.text(p.as_str())).collect();
            q.push(&format!(" AND project IN ({})", ps.join(", ")));
        }
        if let Some(s) = status {
            let s = q.text(s.as_str());
            q.push(&format!(" AND status = {s}"));
        }
        let l = q.int(limit.clamp(1, 200));
        q.push(&format!(" ORDER BY updated_at DESC LIMIT {l}"));
        q.build().fetch_all(&self.pool).await?.iter().map(Self::row_to_run).collect()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sqlite_urls_are_portable() {
        use std::path::Path;
        assert_eq!(super::sqlite_url(Path::new("/tmp/x.db")), "sqlite:/tmp/x.db");
        assert_eq!(
            super::sqlite_url(Path::new(r"C:\Users\a\.memdusa\memdusa.db")),
            "sqlite:C:/Users/a/.memdusa/memdusa.db"
        );
    }

    #[test]
    fn search_terms_drop_stopwords_and_short_words() {
        use super::{search_terms, significant_words};
        assert_eq!(search_terms("what color is the sky"), ["color", "sky"]);
        assert_eq!(search_terms("release docker"), ["release", "docker"]);
        // short technical words stay searchable
        assert_eq!(search_terms("CI pipeline for the db"), ["ci", "pipeline", "db"]);
        // queries with no searchable terms match nothing (no fallback to the raw words)
        assert!(search_terms("is it").is_empty());
        assert!(search_terms("do it").is_empty());
        assert!(search_terms("a an").is_empty());
        assert!(search_terms("!!!").is_empty());
        assert!(search_terms("   ").is_empty());
        // `let's` tokenizes to "let" + "s"; both are dropped
        assert_eq!(search_terms("let's ship it"), ["ship"]);
        // full-text queries and near-duplicate detection use the same filter
        assert_eq!(search_terms("the release workflow"), significant_words("the release workflow"));
    }
}
