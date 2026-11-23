//! Sync protocol shared by the team server and the local database.
//!
//! The `changes` table is the single source of truth for "what moved": every
//! write to a memory, event or checkpoint appends a row in the same
//! transaction (see [`crate::store::Store`]). The server's `seq` is the pull
//! cursor; the local `seq` is the push cursor. Timestamps are never cursors
//! (clock skew), but they do decide last-writer-wins conflicts.
//!
//! Two rules worth spelling out:
//!
//! * **Pruned rows never come back.** Prune deletes old events/checkpoints
//!   without writing a tombstone (there would be far too many). A pull for an
//!   `upsert` change therefore reads the *current* row; if it is gone, the
//!   change is skipped. Once the cursor has passed it, nothing resurrects it.
//!   `delete` changes (real tombstones) need no row and are always returned.
//! * **Delete wins unless the edit is newer.** A delete tombstone carries the
//!   time of the delete. An edit with a later `updated_at`/`created_at` beats
//!   it; an older concurrent edit is ignored (both when the delete and the
//!   edit arrive). Ties favour the delete.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use sqlx::any::AnyRow;

use crate::model::*;
use crate::store::{MEMORY_COLS, Q, Store, list_from_json};
use crate::{Error, Result};

/// Reserved project key used on the wire for a user's global memories
/// (`project IS NULL` in the database). Not a real project key.
pub const SYNC_GLOBALS: &str = "memdusa:globals";

/// Maximum changes accepted in one push / returned in one pull page.
pub const SYNC_BATCH: i64 = 500;
/// `changes.origin` for rows written while applying pulled changes locally. Push skips them, so the
/// push cursor never has to jump past them (jumping skipped edits other processes made meanwhile).
pub const PULLED_ORIGIN: &str = "pulled";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncKind {
    Memory,
    Event,
    Checkpoint,
}

impl SyncKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Event => "event",
            Self::Checkpoint => "checkpoint",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "memory" => Some(Self::Memory),
            "event" => Some(Self::Event),
            "checkpoint" => Some(Self::Checkpoint),
            _ => None,
        }
    }
    fn table(self) -> &'static str {
        match self {
            Self::Memory => "memories",
            Self::Event => "events",
            Self::Checkpoint => "checkpoints",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOp {
    Upsert,
    Delete,
}

impl SyncOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upsert => "upsert",
            Self::Delete => "delete",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "upsert" => Some(Self::Upsert),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }
}

/// One replicated change, with the record inline for upserts (and no record
/// for tombstones). `record_id` is the record's UUIDv7, globally unique, which
/// is what makes push idempotent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRecord {
    pub kind: SyncKind,
    pub op: SyncOp,
    pub record_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<Value>,
    /// Operation time on the originating side; travels with tombstones so
    /// delete-vs-edit can be decided the same way on both sides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<Millis>,
    /// Client-generated sync id. The server stores it on the change it
    /// creates; the pusher skips those changes when it pulls them back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushBody {
    /// Project key, or [`SYNC_GLOBALS`] for the caller's private memories.
    pub project: String,
    #[serde(default)]
    pub changes: Vec<SyncRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushChangeResult {
    pub kind: SyncKind,
    pub record_id: String,
    /// `applied`, `duplicate`, `revision`, `skipped_deleted`.
    pub outcome: String,
    /// Server `seq` of the change this record produced (none when it didn't).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl PushChangeResult {
    fn new(c: &SyncRecord, outcome: &str, seq: Option<i64>) -> Self {
        Self { kind: c.kind, record_id: c.record_id.clone(), outcome: outcome.into(), seq, detail: None }
    }
    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushResponse {
    pub results: Vec<PushChangeResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullResponse {
    pub changes: Vec<SyncRecord>,
    /// Cursor to pass as `after` next time (past skipped/pruned changes too).
    pub next_cursor: i64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncState {
    pub remote: String,
    pub project: String,
    pub push_after: i64,
    pub pull_after: i64,
    /// This client's id, echoed on changes it pushed to `remote`.
    pub origin: String,
    pub last_ok_at: Option<Millis>,
    pub last_error: Option<String>,
}

/// Where changes are being applied. On a server attribution is checked and
/// agent sources re-qualified; on a local database records become local.
#[derive(Debug, Clone)]
pub enum SyncActor {
    Server(Identity),
    Local,
}

/// Append one change row and return its sequence number. Callers must use the
/// same executor as the data write so the two are atomic.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn change_append<'e, E>(
    exec: E,
    kind: SyncKind,
    op: SyncOp,
    record_id: &str,
    project: Option<&str>,
    user: Option<&str>,
    origin: Option<&str>,
    at: Option<Millis>,
) -> Result<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    let mut q = Q::new("INSERT INTO changes (kind, record_id, project, op, at, origin, user_name) VALUES (");
    let vals = [
        q.text(kind.as_str()),
        q.text(record_id),
        q.opt_text(project.map(str::to_owned)),
        q.text(op.as_str()),
        q.int(at.unwrap_or_else(now_ms)),
        q.opt_text(origin.map(str::to_owned)),
        q.opt_text(user.map(str::to_owned)),
    ];
    q.push(&vals.join(", ")).push(") RETURNING seq");
    Ok(q.build().fetch_one(exec).await?.try_get::<i64, _>("seq")?)
}

/// Pulled changes become local data but are not pending pushes; only writes
/// originating on this side go into the local push log.
#[allow(clippy::too_many_arguments)]
async fn maybe_append<'e, E>(
    enabled: bool,
    exec: E,
    kind: SyncKind,
    op: SyncOp,
    record_id: &str,
    project: Option<&str>,
    user: Option<&str>,
    origin: Option<&str>,
    at: Option<Millis>,
) -> Result<Option<i64>>
where
    E: sqlx::Executor<'e, Database = sqlx::Any>,
{
    if enabled { change_append(exec, kind, op, record_id, project, user, origin, at).await.map(Some) } else { Ok(None) }
}

fn project_key(project: Option<&str>) -> Option<&str> {
    match project {
        Some(SYNC_GLOBALS) => None,
        p => p,
    }
}

fn version_wins(
    a_updated: Millis,
    a_id: &str,
    a_content: &str,
    b_updated: Millis,
    b_id: &str,
    b_content: &str,
) -> bool {
    // Total order: later timestamp wins; the larger id breaks a tie, and the
    // larger content breaks the (same id, same millisecond) offline race.
    (a_updated, a_id, a_content) > (b_updated, b_id, b_content)
}

fn qualify_source(source: Option<String>, id: &Identity) -> Option<String> {
    match (id.user.as_deref(), source) {
        (Some(u), Some(s)) if s == u || s.starts_with(&format!("{u}/")) => Some(s),
        (Some(u), Some(s)) => Some(format!("{u}/{s}")),
        (Some(u), None) => Some(u.to_owned()),
        (None, s) => s,
    }
}

fn check_attribution(record: &Value, id: &Identity) -> Result<()> {
    if let Some(user) = record.get("user").and_then(Value::as_str)
        && id.user.as_deref() != Some(user)
    {
        return Err(Error::Forbidden(format!("cannot push a record attributed to '{user}'")));
    }
    if let (Some(user), Some(source)) = (id.user.as_deref(), record.get("source").and_then(Value::as_str))
        && source.contains('/')
        && source != user
        && !source.starts_with(&format!("{user}/"))
    {
        return Err(Error::Forbidden(format!("cannot push a source attributed to '{source}'")));
    }
    Ok(())
}

fn record_project(record: Option<&Value>) -> Result<Option<String>> {
    match record.and_then(|r| r.get("project")) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(p)) => Ok(Some(p.clone())),
        _ => Err(Error::Invalid("record project must be a string or null".into())),
    }
}

fn record_ts(record: Option<&Value>, kind: SyncKind) -> Millis {
    let field = if kind == SyncKind::Memory { "updated_at" } else { "created_at" };
    record
        .and_then(|r| r.get(field))
        .and_then(Value::as_i64)
        .or_else(|| record.and_then(|r| r.get("created_at")).and_then(Value::as_i64))
        .unwrap_or_else(now_ms)
}

impl Store {
    /// Apply a batch of pushed or pulled changes atomically, returning one
    /// result per change. Validation happens before any write: a batch with a
    /// record attributed to another user is rejected in full.
    pub async fn sync_apply(
        &self,
        project: Option<&str>,
        changes: &[SyncRecord],
        actor: &SyncActor,
        log_origin: Option<&str>,
    ) -> Result<Vec<PushChangeResult>> {
        let expected = project_key(project);
        if changes.len() > SYNC_BATCH as usize {
            return Err(Error::Invalid(format!("at most {SYNC_BATCH} changes per push")));
        }
        for c in changes {
            if c.op == SyncOp::Upsert {
                if record_project(c.record.as_ref())?.as_deref() != expected {
                    return Err(Error::Invalid(format!("record {} belongs to a different project", c.record_id)));
                }
                if let SyncActor::Server(id) = actor
                    && let Some(record) = &c.record
                {
                    check_attribution(record, id)?;
                }
            } else {
                // A tombstone has no inline record: check the row it targets so
                // a write grant to project A can't delete project B (or another
                // user's private memory) just by knowing its id.
                let mut q = Q::new(&format!(
                    "SELECT project{} FROM {} WHERE id = ",
                    if c.kind == SyncKind::Memory { ", user_name" } else { "" },
                    c.kind.table()
                ));
                let id_p = q.text(&c.record_id);
                q.push(&id_p);
                if let Some(row) = q.build().fetch_optional(&self.pool).await? {
                    let target: Option<String> = row.try_get("project")?;
                    if target.as_deref() != expected {
                        return Err(Error::Forbidden(format!(
                            "cannot delete record {} from another project",
                            c.record_id
                        )));
                    }
                    let owner: Option<String> =
                        if c.kind == SyncKind::Memory { row.try_get("user_name")? } else { None };
                    if let (SyncActor::Server(id), Some(u)) = (actor, owner.as_deref())
                        && id.user.as_deref() != Some(u)
                    {
                        return Err(Error::Forbidden(format!("cannot delete a record attributed to '{u}'")));
                    }
                }
            }
        }

        let mut tx = self.pool.begin().await?;
        let mut results = Vec::with_capacity(changes.len());
        for c in changes {
            results.push(self.apply_one(&mut tx, expected, c, actor, log_origin).await?);
        }
        tx.commit().await?;
        Ok(results)
    }

    async fn apply_one(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        project: Option<&str>,
        c: &SyncRecord,
        actor: &SyncActor,
        log_origin: Option<&str>,
    ) -> Result<PushChangeResult> {
        let ts = record_ts(c.record.as_ref(), c.kind);
        let at = c.at.unwrap_or(ts);
        if self.delete_suppressed(tx, c.kind, &c.record_id, ts).await? {
            return Ok(PushChangeResult::new(c, "skipped_deleted", None).detail("a newer delete won"));
        }
        // A server push carries the pusher's id so the echo can be skipped on
        // pull; locally applied pulls are just local data (no origin) and do
        // not become pending pushes themselves.
        let (origin, log_changes) = match actor {
            SyncActor::Server(_) => (c.origin.as_deref().or(log_origin), true),
            // Pulled deletes must leave tombstones in the local log, otherwise
            // an older edit later in the same page could resurrect the row.
            // The client tags them PULLED_ORIGIN so push skips them (moving the
            // push cursor past them instead lost other processes' edits).
            SyncActor::Local => (log_origin, true),
        };
        match c.op {
            SyncOp::Delete => self.apply_delete(tx, project, c, actor, origin, at, log_changes).await,
            SyncOp::Upsert => match c.kind {
                SyncKind::Memory => self.apply_memory(tx, c, actor, origin, at, log_changes).await,
                SyncKind::Event => self.apply_event(tx, project, c, actor, origin, at, log_changes).await,
                SyncKind::Checkpoint => self.apply_checkpoint(tx, project, c, actor, origin, at, log_changes).await,
            },
        }
    }

    /// A tombstone at or after a record's own timestamp suppresses it. This is
    /// how an old offline edit is stopped from resurrecting a deleted row.
    async fn delete_suppressed(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        kind: SyncKind,
        id: &str,
        ts: Millis,
    ) -> Result<bool> {
        let mut q = Q::new("SELECT MAX(at) AS t FROM changes WHERE kind = ");
        let (k, i, op) = (q.text(kind.as_str()), q.text(id), q.text(SyncOp::Delete.as_str()));
        q.push(&format!("{k} AND record_id = {i} AND op = {op}"));
        let t: Option<Millis> = q.build().fetch_one(&mut **tx).await?.try_get("t")?;
        Ok(t.is_some_and(|t| t >= ts))
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_delete(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        project: Option<&str>,
        c: &SyncRecord,
        actor: &SyncActor,
        log_origin: Option<&str>,
        at: Millis,
        log_changes: bool,
    ) -> Result<PushChangeResult> {
        // Delete wins unless the record on this side was edited *later* than
        // the delete. Timestamps travel with the tombstone so both sides decide
        // the same way.
        let ts_col = if c.kind == SyncKind::Memory { "updated_at" } else { "created_at" };
        let mut q = Q::new(&format!("SELECT {ts_col} AS t FROM {} WHERE id = ", c.kind.table()));
        let id = q.text(&c.record_id);
        q.push(&id);
        let t: Option<Millis> =
            q.build().fetch_optional(&mut **tx).await?.map(|r| r.try_get::<Millis, _>("t")).transpose()?;
        if t.is_some_and(|t| t > at) {
            return Ok(PushChangeResult::new(c, "kept_newer", None).detail("a newer edit survived the delete"));
        }

        let mut q = Q::new("DELETE FROM ");
        q.push(c.kind.table()).push(" WHERE id = ");
        let id = q.text(&c.record_id);
        q.push(&id);
        let deleted = q.build().execute(&mut **tx).await?.rows_affected() > 0;
        if !deleted {
            return Ok(PushChangeResult::new(c, "duplicate", None));
        }
        let user = if c.kind == SyncKind::Memory {
            let mut q = Q::new("SELECT user_name FROM changes WHERE kind = ");
            let (k, i) = (q.text(SyncKind::Memory.as_str()), q.text(&c.record_id));
            q.push(&format!("{k} AND record_id = {i} AND op = "));
            let upsert = q.text(SyncOp::Upsert.as_str());
            q.push(&upsert).push(" ORDER BY seq DESC LIMIT 1");
            q.build()
                .fetch_optional(&mut **tx)
                .await?
                .and_then(|r| r.try_get::<Option<String>, _>("user_name").ok().flatten())
        } else {
            None
        };
        // Locally applied records become local; on the server the actor's own
        // attribution is already on the row we just deleted.
        let _ = actor;
        let seq = maybe_append(
            log_changes,
            &mut **tx,
            c.kind,
            SyncOp::Delete,
            &c.record_id,
            project,
            user.as_deref(),
            log_origin,
            Some(at),
        )
        .await?;
        Ok(PushChangeResult::new(c, "applied", seq))
    }

    async fn apply_memory(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        c: &SyncRecord,
        actor: &SyncActor,
        log_origin: Option<&str>,
        at: Millis,
        log_changes: bool,
    ) -> Result<PushChangeResult> {
        let red = self.redactor();
        let mut m: Memory = serde_json::from_value(
            c.record.clone().ok_or_else(|| Error::Invalid("memory upsert needs a record".into()))?,
        )
        .map_err(|e| Error::Invalid(format!("bad memory record: {e}")))?;
        m.content = red.string(m.content.trim().to_owned());
        if m.content.is_empty() {
            return Err(Error::Invalid("memory content is empty".into()));
        }
        m.tags = red.list(m.tags);
        match actor {
            SyncActor::Server(id) => {
                m.user = id.user.clone();
                m.source = qualify_source(m.source, id);
            }
            SyncActor::Local => m.user = None,
        }

        let mut q = Q::new(&format!("SELECT {MEMORY_COLS} FROM memories WHERE id = "));
        let id = q.text(&m.id);
        q.push(&id);
        let old = q.build().fetch_optional(&mut **tx).await?.map(|r| Store::row_to_memory(&r)).transpose()?;
        let wins =
            old.as_ref().is_none_or(|o| version_wins(m.updated_at, &m.id, &m.content, o.updated_at, &o.id, &o.content));
        if !wins {
            // The incoming version lost: keep *it*, not the current winner.
            self.insert_revision(tx, &m, &m.id).await?;
            return Ok(PushChangeResult::new(c, "revision", None).detail("kept the newer local version"));
        }
        if let Some(o) = &old {
            self.insert_revision(tx, o, &m.id).await?;
        }

        if old.is_some() {
            let mut q = Q::new("UPDATE memories SET ");
            let (ct, k, t, s, u, upd, idp) = (
                q.text(&m.content),
                q.text(m.kind.as_str()),
                q.text(serde_json::to_string(&m.tags).unwrap_or_else(|_| "[]".into())),
                q.opt_text(m.source.clone()),
                q.opt_text(m.user.clone()),
                q.int(m.updated_at),
                q.text(&m.id),
            );
            q.push(&format!(
                "content = {ct}, kind = {k}, tags = {t}, source = {s}, user_name = {u}, updated_at = {upd} WHERE id = {idp}"
            ));
            q.build().execute(&mut **tx).await?;
        } else {
            let mut q = Q::new(
                "INSERT INTO memories (id, project, kind, content, tags, source, user_name, created_at, updated_at) VALUES (",
            );
            let vals = [
                q.text(&m.id),
                q.opt_text(m.project.clone()),
                q.text(m.kind.as_str()),
                q.text(&m.content),
                q.text(serde_json::to_string(&m.tags).unwrap_or_else(|_| "[]".into())),
                q.opt_text(m.source.clone()),
                q.opt_text(m.user.clone()),
                q.int(m.created_at),
                q.int(m.updated_at),
            ];
            q.push(&vals.join(", ")).push(")");
            q.build().execute(&mut **tx).await?;
        }
        let seq = maybe_append(
            log_changes,
            &mut **tx,
            SyncKind::Memory,
            SyncOp::Upsert,
            &m.id,
            m.project.as_deref(),
            m.user.as_deref(),
            log_origin,
            Some(at),
        )
        .await?;
        if matches!(actor, SyncActor::Local) {
            self.embed_memory_best_effort(&m.id, &m.content, &m.tags).await;
        }
        Ok(PushChangeResult::new(c, "applied", seq))
    }

    /// Keep a losing version for `memdusa memories revisions <id>`.
    async fn insert_revision(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        m: &Memory,
        memory_id: &str,
    ) -> Result<()> {
        let mut q = Q::new("SELECT COUNT(*) AS n FROM memory_revisions WHERE memory_id = ");
        let mid = q.text(memory_id);
        let u = q.int(m.updated_at);
        let c = q.text(&m.content);
        q.push(&format!("{mid} AND updated_at = {u} AND content = {c}"));
        if q.build().fetch_one(&mut **tx).await?.try_get::<i64, _>("n")? > 0 {
            return Ok(());
        }
        let mut q = Q::new(
            "INSERT INTO memory_revisions (id, memory_id, project, kind, content, tags, source, user_name, created_at, updated_at, superseded_at) VALUES (",
        );
        let vals = [
            q.text(new_id()),
            q.text(memory_id),
            q.opt_text(m.project.clone()),
            q.text(m.kind.as_str()),
            q.text(&m.content),
            q.text(serde_json::to_string(&m.tags).unwrap_or_else(|_| "[]".into())),
            q.opt_text(m.source.clone()),
            q.opt_text(m.user.clone()),
            q.int(m.created_at),
            q.int(m.updated_at),
            q.int(now_ms()),
        ];
        q.push(&vals.join(", ")).push(")");
        q.build().execute(&mut **tx).await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_event(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        project: Option<&str>,
        c: &SyncRecord,
        actor: &SyncActor,
        log_origin: Option<&str>,
        at: Millis,
        log_changes: bool,
    ) -> Result<PushChangeResult> {
        let red = self.redactor();
        let mut e: Event = serde_json::from_value(
            c.record.clone().ok_or_else(|| Error::Invalid("event upsert needs a record".into()))?,
        )
        .map_err(|err| Error::Invalid(format!("bad event record: {err}")))?;
        e.content = red.string(e.content);
        e.data = red.opt_json(e.data);
        if let SyncActor::Server(id) = actor {
            e.source = qualify_source(e.source, id);
        }
        let mut q =
            Q::new("INSERT INTO events (id, project, session, source, kind, content, data, created_at) VALUES (");
        let vals = [
            q.text(&e.id),
            q.text(&e.project),
            q.opt_text(e.session.clone()),
            q.opt_text(e.source.clone()),
            q.text(&e.kind),
            q.text(&e.content),
            q.opt_text(e.data.as_ref().map(|v| v.to_string())),
            q.int(e.created_at),
        ];
        q.push(&vals.join(", ")).push(") ON CONFLICT (id) DO NOTHING");
        if q.build().execute(&mut **tx).await?.rows_affected() == 0 {
            return Ok(PushChangeResult::new(c, "duplicate", None));
        }
        let seq = maybe_append(
            log_changes,
            &mut **tx,
            SyncKind::Event,
            SyncOp::Upsert,
            &e.id,
            project,
            None,
            log_origin,
            Some(at),
        )
        .await?;
        Ok(PushChangeResult::new(c, "applied", seq))
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_checkpoint(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        project: Option<&str>,
        c: &SyncRecord,
        actor: &SyncActor,
        log_origin: Option<&str>,
        at: Millis,
        log_changes: bool,
    ) -> Result<PushChangeResult> {
        let red = self.redactor();
        let mut cp: Checkpoint = serde_json::from_value(
            c.record.clone().ok_or_else(|| Error::Invalid("checkpoint upsert needs a record".into()))?,
        )
        .map_err(|err| Error::Invalid(format!("bad checkpoint record: {err}")))?;
        cp.goal = red.string(cp.goal);
        cp.done = red.list(cp.done);
        cp.next = red.list(cp.next);
        cp.blockers = red.list(cp.blockers);
        cp.notes = red.opt(cp.notes);
        if let SyncActor::Server(id) = actor {
            cp.source = qualify_source(cp.source, id);
        }
        let json = |v: &[String]| serde_json::to_string(v).unwrap_or_else(|_| "[]".into());
        let mut q = Q::new(
            "INSERT INTO checkpoints (id, project, task, goal, done, next, blockers, files, notes, source, git, created_at) VALUES (",
        );
        let vals = [
            q.text(&cp.id),
            q.text(&cp.project),
            q.opt_text(cp.task.clone()),
            q.text(&cp.goal),
            q.text(json(&cp.done)),
            q.text(json(&cp.next)),
            q.text(json(&cp.blockers)),
            q.text(json(&cp.files)),
            q.opt_text(cp.notes.clone()),
            q.opt_text(cp.source.clone()),
            q.opt_text(cp.git.as_ref().and_then(|g| serde_json::to_string(g).ok())),
            q.int(cp.created_at),
        ];
        q.push(&vals.join(", ")).push(") ON CONFLICT (id) DO NOTHING");
        if q.build().execute(&mut **tx).await?.rows_affected() == 0 {
            return Ok(PushChangeResult::new(c, "duplicate", None));
        }
        let seq = maybe_append(
            log_changes,
            &mut **tx,
            SyncKind::Checkpoint,
            SyncOp::Upsert,
            &cp.id,
            project,
            None,
            log_origin,
            Some(at),
        )
        .await?;
        Ok(PushChangeResult::new(c, "applied", seq))
    }

    /// Local changes after a push cursor, in order. Upserts whose row was
    /// pruned in the meantime are skipped (but still advance the cursor).
    pub async fn sync_local_changes(
        &self,
        project: Option<&str>,
        after: i64,
        limit: i64,
    ) -> Result<(Vec<SyncRecord>, i64)> {
        let limit = limit.clamp(1, SYNC_BATCH);
        let mut q = Q::new("SELECT seq, kind, record_id, project, op, at, origin, user_name FROM changes WHERE seq > ");
        let a = q.int(after);
        q.push(&a);
        match project {
            Some(SYNC_GLOBALS) => {
                q.push(" AND project IS NULL");
            }
            Some(p) => {
                let p = q.text(p);
                q.push(&format!(" AND project = {p}"));
            }
            None => {}
        }
        let l = q.int(limit);
        q.push(&format!(" ORDER BY seq ASC LIMIT {l}"));
        let rows = q.build().fetch_all(&self.pool).await?;
        let mut out = vec![];
        let mut last_seq = after;
        for r in &rows {
            let seq: i64 = r.try_get("seq")?;
            last_seq = seq;
            let kind = r.try_get::<String, _>("kind")?;
            let Some(kind) = SyncKind::parse(&kind) else { continue };
            let Some(op) = SyncOp::parse(&r.try_get::<String, _>("op")?) else { continue };
            let id: String = r.try_get("record_id")?;
            let at: Millis = r.try_get("at")?;
            let origin: Option<String> = r.try_get("origin")?;
            if origin.as_deref() == Some(PULLED_ORIGIN) {
                continue; // applied from a pull: never echo it back (the cursor still advances)
            }
            let record = if op == SyncOp::Delete {
                None
            } else {
                match self.sync_record_by_id(kind, &id).await? {
                    Some(v) => Some(v),
                    None => continue, // pruned: never push a row that no longer exists
                }
            };
            out.push(SyncRecord { kind, op, record_id: id, record, at: Some(at), origin });
        }
        Ok((out, last_seq))
    }

    /// Changes after `after`, with inline records, for `GET /v1/sync/pull`.
    pub async fn sync_pull(
        &self,
        project: Option<&str>,
        after: i64,
        limit: i64,
        actor: &Identity,
    ) -> Result<PullResponse> {
        let limit = limit.clamp(1, SYNC_BATCH);
        let mut q = Q::new("SELECT seq, kind, record_id, op, at, origin FROM changes WHERE seq > ");
        let a = q.int(after);
        q.push(&a);
        match project {
            Some(SYNC_GLOBALS) => {
                // Globals are private: only the caller's (or ownerless local) ones.
                let u = q.text(actor.user.as_deref().unwrap_or(""));
                q.push(&format!(" AND project IS NULL AND (user_name = {u} OR user_name IS NULL)"));
            }
            Some(p) => {
                let p = q.text(p);
                q.push(&format!(" AND project = {p}"));
            }
            None => {}
        }
        let l = q.int(limit + 1);
        q.push(&format!(" ORDER BY seq ASC LIMIT {l}"));
        let rows = q.build().fetch_all(&self.pool).await?;
        let has_more = rows.len() as i64 > limit;
        let mut out = vec![];
        let mut last_seq = after;
        for r in rows.iter().take(limit as usize) {
            let seq: i64 = r.try_get("seq")?;
            last_seq = seq;
            let Some(kind) = SyncKind::parse(&r.try_get::<String, _>("kind")?) else { continue };
            let Some(op) = SyncOp::parse(&r.try_get::<String, _>("op")?) else { continue };
            let id: String = r.try_get("record_id")?;
            let at: Millis = r.try_get("at")?;
            let origin: Option<String> = r.try_get("origin")?;
            let record = if op == SyncOp::Delete {
                None
            } else {
                // Pruned rows have no tombstone: skip the upsert instead of
                // resurrecting the record on a peer that pulls from zero.
                match self.sync_record_by_id(kind, &id).await? {
                    Some(v) => Some(v),
                    None => continue,
                }
            };
            out.push(SyncRecord { kind, op, record_id: id, record, at: Some(at), origin });
        }
        Ok(PullResponse { changes: out, next_cursor: last_seq, has_more })
    }

    /// Serialize one current data row for a pull/push record.
    pub(crate) async fn sync_record_by_id(&self, kind: SyncKind, id: &str) -> Result<Option<Value>> {
        let sql = match kind {
            SyncKind::Memory => format!("SELECT {MEMORY_COLS} FROM memories WHERE id = "),
            _ => format!("SELECT * FROM {} WHERE id = ", kind.table()),
        };
        let mut q = Q::new(&sql);
        let id_p = q.text(id);
        q.push(&id_p);
        let row = q.build().fetch_optional(&self.pool).await?;
        let Some(r) = row else { return Ok(None) };
        let value = match kind {
            SyncKind::Memory => {
                serde_json::to_value(Store::row_to_memory(&r)?).map_err(|e| Error::Invalid(e.to_string()))?
            }
            SyncKind::Event => serde_json::to_value(Event {
                id: r.try_get("id")?,
                project: r.try_get("project")?,
                session: r.try_get("session")?,
                source: r.try_get("source")?,
                kind: r.try_get("kind")?,
                content: r.try_get("content")?,
                data: r.try_get::<Option<String>, _>("data")?.and_then(|s| serde_json::from_str(&s).ok()),
                created_at: r.try_get("created_at")?,
            })
            .map_err(|e| Error::Invalid(e.to_string()))?,
            SyncKind::Checkpoint => serde_json::to_value(Checkpoint {
                id: r.try_get("id")?,
                project: r.try_get("project")?,
                task: r.try_get("task")?,
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
            .map_err(|e| Error::Invalid(e.to_string()))?,
        };
        Ok(Some(value))
    }

    /// Add upsert changes for rows that predate the sync migration. Called
    /// once when a project is opted in; existing changes are left alone.
    pub async fn sync_seed(&self, project: Option<&str>) -> Result<usize> {
        let mut total = 0;
        for kind in [SyncKind::Memory, SyncKind::Event, SyncKind::Checkpoint] {
            let ts_col = if kind == SyncKind::Memory { "updated_at" } else { "created_at" };
            let mut q = Q::new(&format!(
                "INSERT INTO changes (kind, record_id, project, op, at, origin, user_name) \
                 SELECT {}, id, project, 'upsert', {}, NULL, {} FROM {} WHERE 1=1 AND NOT EXISTS \
                 (SELECT 1 FROM changes c WHERE c.kind = {} AND c.record_id = {}.id)",
                q_lit(kind.as_str()),
                ts_col,
                if kind == SyncKind::Memory { "user_name" } else { "NULL" },
                kind.table(),
                q_lit(kind.as_str()),
                kind.table(),
            ));
            match project {
                Some(SYNC_GLOBALS) => {
                    q.push(" AND project IS NULL");
                }
                Some(p) => {
                    let p = q.text(p);
                    q.push(&format!(" AND project = {p}"));
                }
                None => {}
            }
            total += q.build().execute(&self.pool).await?.rows_affected() as usize;
        }
        Ok(total)
    }

    // ------------------------------------------------------------ sync state

    pub async fn sync_state_get(&self, remote: &str, project: &str) -> Result<Option<SyncState>> {
        let mut q = Q::new("SELECT * FROM sync_state WHERE remote = ");
        let r = q.text(remote);
        let p = q.text(project);
        q.push(&format!("{r} AND project = {p}"));
        q.build().fetch_optional(&self.pool).await?.map(|r| state_from_row(&r)).transpose()
    }

    pub async fn sync_state_list(&self, remote: &str) -> Result<Vec<SyncState>> {
        let mut q = Q::new("SELECT * FROM sync_state WHERE remote = ");
        let r = q.text(remote);
        q.push(&r).push(" ORDER BY project");
        q.build().fetch_all(&self.pool).await?.iter().map(state_from_row).collect()
    }

    pub async fn sync_state_add(&self, remote: &str, project: &str, origin: &str) -> Result<SyncState> {
        let mut q = Q::new("INSERT INTO sync_state (remote, project, push_after, pull_after, origin) VALUES (");
        let vals = [q.text(remote), q.text(project), q.int(0), q.int(0), q.text(origin)];
        q.push(&vals.join(", ")).push(") ON CONFLICT (remote, project) DO NOTHING");
        q.build().execute(&self.pool).await?;
        Ok(self.sync_state_get(remote, project).await?.expect("sync state just inserted"))
    }

    pub async fn sync_state_remove(&self, remote: &str, project: &str) -> Result<()> {
        let mut q = Q::new("DELETE FROM sync_state WHERE remote = ");
        let (r, p) = (q.text(remote), q.text(project));
        q.push(&format!("{r} AND project = {p}"));
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    /// Advance cursors and record the outcome of a run. `None` fields are left
    /// untouched; `ok` clears the last error and stamps `last_ok_at`.
    #[allow(clippy::too_many_arguments)]
    pub async fn sync_state_update(
        &self,
        remote: &str,
        project: &str,
        push_after: Option<i64>,
        pull_after: Option<i64>,
        error: Option<&str>,
        ok: bool,
    ) -> Result<()> {
        let mut q = Q::new("UPDATE sync_state SET ");
        let mut sets = vec![];
        if let Some(v) = push_after {
            sets.push(format!("push_after = {}", q.int(v)));
        }
        if let Some(v) = pull_after {
            sets.push(format!("pull_after = {}", q.int(v)));
        }
        if ok {
            sets.push(format!("last_ok_at = {}", q.int(now_ms())));
            sets.push("last_error = NULL".into());
        } else if let Some(e) = error {
            sets.push(format!("last_error = {}", q.text(e)));
        }
        if sets.is_empty() {
            return Ok(());
        }
        let (r, p) = (q.text(remote), q.text(project));
        q.push(&sets.join(", ")).push(&format!(" WHERE remote = {r} AND project = {p}"));
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    /// Highest change sequence currently in the local log (0 when empty).
    pub async fn sync_last_seq(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COALESCE(MAX(seq), 0) AS n FROM changes").fetch_one(&self.pool).await?;
        Ok(row.try_get::<i64, _>("n")?)
    }

    /// Pending local changes for `memdusa sync status`.
    pub async fn sync_pending(&self, project: Option<&str>, after: i64) -> Result<i64> {
        let mut q = Q::new("SELECT COUNT(*) AS n FROM changes WHERE seq > ");
        let a = q.int(after);
        q.push(&a);
        // changes applied from a pull are never pushed, so they aren't pending either
        let pulled = q.text(PULLED_ORIGIN);
        q.push(&format!(" AND (origin IS NULL OR origin <> {pulled})"));
        match project {
            Some(SYNC_GLOBALS) => {
                q.push(" AND project IS NULL");
            }
            Some(p) => {
                let p = q.text(p);
                q.push(&format!(" AND project = {p}"));
            }
            None => {}
        }
        Ok(q.build().fetch_one(&self.pool).await?.try_get("n")?)
    }

    /// Recent LWW losers for `memdusa sync status`.
    pub async fn sync_conflicts(&self, project: Option<&str>, limit: i64) -> Result<Vec<Memory>> {
        let mut q = Q::new(
            "SELECT id, project, kind, content, tags, source, user_name, created_at, updated_at \
                            FROM memory_revisions WHERE 1=1",
        );
        match project {
            Some(SYNC_GLOBALS) => {
                q.push(" AND project IS NULL");
            }
            Some(p) => {
                let p = q.text(p);
                q.push(&format!(" AND project = {p}"));
            }
            None => {}
        }
        let l = q.int(limit.clamp(1, 50));
        q.push(&format!(" ORDER BY superseded_at DESC LIMIT {l}"));
        q.build().fetch_all(&self.pool).await?.iter().map(Store::row_to_memory).collect()
    }

    /// Versions stored for one memory by `memdusa memories revisions <id>`.
    pub async fn memory_revisions(&self, id: &str) -> Result<Vec<Memory>> {
        let mut q = Q::new(
            "SELECT id, project, kind, content, tags, source, user_name, created_at, updated_at \
                            FROM memory_revisions WHERE memory_id = ",
        );
        let id_p = q.text(id);
        q.push(&id_p).push(" ORDER BY superseded_at DESC, updated_at DESC LIMIT 50");
        q.build().fetch_all(&self.pool).await?.iter().map(Store::row_to_memory).collect()
    }
}

fn q_lit(s: &str) -> String {
    // Called only with our own static enum strings.
    format!("'{s}'")
}

fn state_from_row(r: &AnyRow) -> Result<SyncState> {
    Ok(SyncState {
        remote: r.try_get("remote")?,
        project: r.try_get("project")?,
        push_after: r.try_get("push_after")?,
        pull_after: r.try_get("pull_after")?,
        origin: r.try_get("origin")?,
        last_ok_at: r.try_get("last_ok_at")?,
        last_error: r.try_get("last_error")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_project_and_outcomes_round_trip() {
        assert_eq!(SYNC_GLOBALS, "memdusa:globals");
        for s in ["memory", "event", "checkpoint"] {
            assert_eq!(SyncKind::parse(s).unwrap().as_str(), s);
        }
        for s in ["upsert", "delete"] {
            assert_eq!(SyncOp::parse(s).unwrap().as_str(), s);
        }
        assert!(version_wins(2, "a", "", 1, "b", ""));
        assert!(!version_wins(1, "b", "", 2, "a", ""));
    }
}
