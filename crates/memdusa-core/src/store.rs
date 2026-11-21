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
}

// ---------------------------------------------------------------- tiny query builder

pub(crate) enum Arg {
    Text(Option<String>),
    Int(i64),
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
    pub(crate) fn build(&self) -> sqlx::query::Query<'_, sqlx::Any, sqlx::any::AnyArguments> {
        // every dynamic part is a `$N` placeholder; values are always bound
        let mut q = sqlx::query(sqlx::AssertSqlSafe(self.sql.as_str()));
        for a in &self.args {
            q = match a {
                Arg::Text(v) => q.bind(v.clone()),
                Arg::Int(v) => q.bind(*v),
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

/// Ordered, append-only list of migrations. Never edit a released entry:
/// add a new one (e.g. `ALTER TABLE ... ADD COLUMN ...`). Statements must be
/// valid on both SQLite and Postgres.
const MIGRATIONS: &[&[&str]] = &[
    // 1: initial schema
    &[
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
    ],
    // 2: users, API keys, per-user global memories
    &[
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
    ],
    // 3: code state on checkpoints
    &["ALTER TABLE checkpoints ADD COLUMN git TEXT"],
];

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

        let store = Self { pool, backend, redactor: Arc::new(Redactor::new(&[])?) };
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
            for stmt in *stmts {
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

    fn row_to_memory(r: &AnyRow) -> Result<Memory> {
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

        let mut q = Q::new("SELECT * FROM memories WHERE ");
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
        Ok(mem)
    }

    pub async fn memory_search(&self, f: MemoryQuery) -> Result<Vec<Memory>> {
        let mut q = Q::new("SELECT * FROM memories WHERE ");
        let globals = Self::global_visible(&mut q, f.user.as_deref());
        match f.project {
            Some(p) => {
                let p = q.text(p);
                q.push(&format!("(project = {p} OR {globals})"))
            }
            None => q.push(&format!("(project IS NOT NULL OR {globals})")),
        };
        if let Some(k) = f.kind {
            let k = q.text(k.as_str());
            q.push(&format!(" AND kind = {k}"));
        }
        if let Some(text) = f.query.as_deref() {
            // every word must match content or tags
            for word in text.split_whitespace().take(8) {
                let w = q.text(format!("%{}%", word.to_lowercase()));
                q.push(&format!(" AND (LOWER(content) LIKE {w} OR LOWER(tags) LIKE {w})"));
            }
        }
        let limit = q.int(f.limit.unwrap_or(20).clamp(1, 200));
        q.push(&format!(" ORDER BY updated_at DESC LIMIT {limit}"));
        q.build().fetch_all(&self.pool).await?.iter().map(Self::row_to_memory).collect()
    }

    pub async fn memory_forget(&self, id: &str) -> Result<()> {
        let mut q = Q::new("DELETE FROM memories WHERE id = ");
        let id_p = q.text(id);
        q.push(&id_p);
        if q.build().execute(&self.pool).await?.rows_affected() == 0 {
            return Err(Error::NotFound(format!("memory {id}")));
        }
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
        let mut q = Q::new("SELECT * FROM events WHERE project = ");
        let p = q.text(project);
        q.push(&p);
        if let Some(s) = session {
            let s = q.text(s);
            q.push(&format!(" AND session = {s}"));
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
            out.push(Checkpoint {
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
            });
            if out.len() as i64 >= limit {
                break;
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------ durable runs

    fn row_to_run(r: &AnyRow) -> Result<Run> {
        let status = RunStatus::parse(&r.try_get::<String, _>("status")?);
        let lease_until: Option<i64> = r.try_get("lease_until")?;
        Ok(Run {
            id: r.try_get("id")?,
            project: r.try_get("project")?,
            name: r.try_get("name")?,
            idempotency_key: r.try_get("idempotency_key")?,
            status,
            input: from_json(r.try_get("input")?),
            output: from_json(r.try_get("output")?),
            error: r.try_get("error")?,
            owner: r.try_get("owner")?,
            lease_until,
            created_at: r.try_get("created_at")?,
            updated_at: r.try_get("updated_at")?,
            stale: status == RunStatus::Running && lease_until.is_none_or(|l| l <= now_ms()),
        })
    }

    fn row_to_step(r: &AnyRow) -> Result<Step> {
        Ok(Step {
            run_id: r.try_get("run_id")?,
            key: r.try_get("step_key")?,
            seq: r.try_get("seq")?,
            status: StepStatus::parse(&r.try_get::<String, _>("status")?),
            input: from_json(r.try_get("input")?),
            output: from_json(r.try_get("output")?),
            error: r.try_get("error")?,
            attempts: r.try_get("attempts")?,
            started_at: r.try_get("started_at")?,
            completed_at: r.try_get("completed_at")?,
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
            "status = {s}, output = {o}, error = NULL, completed_at = {n} WHERE run_id = {r} AND step_key = {k} AND status <> {done}"
        ));
        q.build().execute(&self.pool).await?;
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
        q.push(&format!("status = {s}, error = {e} WHERE run_id = {r} AND step_key = {k} AND status <> {done}"));
        q.build().execute(&self.pool).await?;
        self.step_get(run_id, key).await
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

    pub async fn run_list(&self, project: Option<&str>, status: Option<RunStatus>, limit: i64) -> Result<Vec<Run>> {
        let mut q = Q::new("SELECT * FROM runs WHERE 1=1");
        if let Some(p) = project {
            let p = q.text(p);
            q.push(&format!(" AND project = {p}"));
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
