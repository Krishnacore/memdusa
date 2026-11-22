//! Export / import as JSON Lines: backups that work on any backend, and
//! moving data between databases (e.g. a local SQLite history onto a hosted
//! Postgres server). Import keeps ids and timestamps and skips records that
//! already exist, so it is idempotent and safe to re-run.
//!
//! Users and API keys are not exported: credentials don't travel.

use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::model::*;
use crate::store::{Q, Store};
use crate::{Error, Result};

pub const FORMAT: &str = "memdusa-export";
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    Header { format: String, version: u32, exported_at: Millis },
    Memory(Memory),
    Event(Event),
    Checkpoint(Checkpoint),
    Run(RunWithSteps),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportReport {
    pub memories: usize,
    pub events: usize,
    pub checkpoints: usize,
    pub runs: usize,
    pub steps: usize,
    /// Records that already existed (same id) and were left untouched.
    pub skipped: usize,
}

/// Parse JSON Lines into records, with line numbers in errors.
pub fn parse_jsonl(text: &str) -> Result<Vec<Record>> {
    let mut out = vec![];
    for (i, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        let r: Record = serde_json::from_str(line)
            .map_err(|e| Error::Invalid(format!("line {}: not a memdusa record: {e}", i + 1)))?;
        if let Record::Header { format, version, .. } = &r
            && (format != FORMAT || *version > VERSION)
        {
            return Err(Error::Invalid(format!("unsupported export format {format} v{version}")));
        }
        out.push(r);
    }
    Ok(out)
}

pub fn to_jsonl(records: &[Record]) -> String {
    records.iter().filter_map(|r| serde_json::to_string(r).ok()).map(|l| l + "\n").collect()
}

fn json(v: &impl Serialize) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

impl Store {
    /// Everything (or one project's data, without global memories).
    pub async fn export(&self, project: Option<&str>) -> Result<Vec<Record>> {
        let mut out = vec![Record::Header { format: FORMAT.into(), version: VERSION, exported_at: now_ms() }];
        let filter = |q: &mut Q| {
            if let Some(p) = project {
                let p = q.text(p);
                q.push(&format!(" WHERE project = {p}"));
            }
        };

        // explicit columns: Postgres' `search` tsvector column isn't readable through `sqlx::Any`
        let mut q = Q::new(&format!("SELECT {} FROM memories", crate::store::MEMORY_COLS));
        filter(&mut q);
        q.push(" ORDER BY created_at");
        for r in q.build().fetch_all(&self.pool).await? {
            out.push(Record::Memory(Memory {
                id: r.try_get("id")?,
                project: r.try_get("project")?,
                kind: MemoryKind::parse(&r.try_get::<String, _>("kind")?),
                content: r.try_get("content")?,
                tags: serde_json::from_str(&r.try_get::<String, _>("tags")?).unwrap_or_default(),
                source: r.try_get("source")?,
                user: r.try_get("user_name")?,
                created_at: r.try_get("created_at")?,
                updated_at: r.try_get("updated_at")?,
                merged: false,
            }));
        }

        let mut q = Q::new("SELECT * FROM events");
        filter(&mut q);
        q.push(" ORDER BY created_at");
        for r in q.build().fetch_all(&self.pool).await? {
            out.push(Record::Event(Event {
                id: r.try_get("id")?,
                project: r.try_get("project")?,
                session: r.try_get("session")?,
                source: r.try_get("source")?,
                kind: r.try_get("kind")?,
                content: r.try_get("content")?,
                data: r.try_get::<Option<String>, _>("data")?.and_then(|s| serde_json::from_str(&s).ok()),
                created_at: r.try_get("created_at")?,
            }));
        }

        let mut q = Q::new("SELECT * FROM checkpoints");
        filter(&mut q);
        q.push(" ORDER BY created_at");
        let list = |r: &sqlx::any::AnyRow, c: &str| -> Result<Vec<String>> {
            Ok(serde_json::from_str(&r.try_get::<String, _>(c)?).unwrap_or_default())
        };
        for r in q.build().fetch_all(&self.pool).await? {
            out.push(Record::Checkpoint(Checkpoint {
                id: r.try_get("id")?,
                project: r.try_get("project")?,
                task: r.try_get("task")?,
                goal: r.try_get("goal")?,
                done: list(&r, "done")?,
                next: list(&r, "next")?,
                blockers: list(&r, "blockers")?,
                files: list(&r, "files")?,
                notes: r.try_get("notes")?,
                source: r.try_get("source")?,
                git: r.try_get::<Option<String>, _>("git")?.and_then(|s| serde_json::from_str(&s).ok()),
                created_at: r.try_get("created_at")?,
            }));
        }

        let mut q = Q::new("SELECT id FROM runs");
        filter(&mut q);
        q.push(" ORDER BY created_at");
        for r in q.build().fetch_all(&self.pool).await? {
            out.push(Record::Run(self.run_get(&r.try_get::<String, _>("id")?).await?));
        }
        Ok(out)
    }

    /// Insert records, keeping their ids and timestamps. Existing ids (and
    /// other unique clashes, like a run's idempotency key) are skipped.
    /// Text is redacted again on the way in.
    pub async fn import(&self, records: Vec<Record>) -> Result<ImportReport> {
        let mut rep = ImportReport::default();
        let red = self.redactor();
        let mut tx = self.pool.begin().await?;
        for rec in records {
            let (sql, args): (&str, Vec<Option<String>>) = match rec {
                Record::Header { .. } => continue,
                Record::Memory(m) => (
                    "INSERT INTO memories (id, project, kind, content, tags, source, user_name, created_at, updated_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT DO NOTHING",
                    vec![
                        Some(m.id),
                        m.project,
                        Some(m.kind.as_str().into()),
                        Some(red.string(m.content)),
                        Some(json(&red.list(m.tags))),
                        m.source,
                        m.user,
                        Some(m.created_at.to_string()),
                        Some(m.updated_at.to_string()),
                    ],
                ),
                Record::Event(e) => (
                    "INSERT INTO events (id, project, session, source, kind, content, data, created_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT DO NOTHING",
                    vec![
                        Some(e.id),
                        Some(e.project),
                        e.session,
                        e.source,
                        Some(e.kind),
                        Some(red.string(e.content)),
                        red.opt_json(e.data).map(|d| json(&d)),
                        Some(e.created_at.to_string()),
                    ],
                ),
                Record::Checkpoint(c) => (
                    "INSERT INTO checkpoints (id, project, task, goal, done, next, blockers, files, notes, source, git, created_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) ON CONFLICT DO NOTHING",
                    vec![
                        Some(c.id),
                        Some(c.project),
                        c.task,
                        Some(red.string(c.goal)),
                        Some(json(&red.list(c.done))),
                        Some(json(&red.list(c.next))),
                        Some(json(&red.list(c.blockers))),
                        Some(json(&c.files)),
                        red.opt(c.notes),
                        c.source,
                        c.git.map(|g| json(&g)),
                        Some(c.created_at.to_string()),
                    ],
                ),
                Record::Run(rw) => {
                    let r = rw.run;
                    let mut q = Q::new(
                        "INSERT INTO runs (id, project, name, idempotency_key, status, input, output, error, owner, \
                         lease_until, lease_secs, created_at, updated_at) VALUES (",
                    );
                    let vals = [
                        q.text(&r.id),
                        q.text(&r.project),
                        q.text(red.string(r.name)),
                        q.opt_text(r.idempotency_key),
                        q.text(r.status.as_str()),
                        q.opt_text(red.opt_json(r.input).map(|v| json(&v))),
                        q.opt_text(red.opt_json(r.output).map(|v| json(&v))),
                        q.opt_text(red.opt(r.error)),
                        q.opt_text(r.owner),
                        r.lease_until.map_or("NULL".into(), |t| q.int(t)),
                        q.int(600),
                        q.int(r.created_at),
                        q.int(r.updated_at),
                    ];
                    q.push(&vals.join(", ")).push(") ON CONFLICT DO NOTHING");
                    if q.build().execute(&mut *tx).await?.rows_affected() == 0 {
                        rep.skipped += 1;
                        continue; // don't graft steps onto a different existing run
                    }
                    rep.runs += 1;
                    for s in rw.steps {
                        let mut q = Q::new(
                            "INSERT INTO steps (run_id, step_key, seq, status, input, output, error, attempts, started_at, \
                             completed_at) VALUES (",
                        );
                        let vals = [
                            q.text(&s.run_id),
                            q.text(&s.key),
                            q.int(s.seq),
                            q.text(s.status.as_str()),
                            q.opt_text(red.opt_json(s.input).map(|v| json(&v))),
                            q.opt_text(red.opt_json(s.output).map(|v| json(&v))),
                            q.opt_text(red.opt(s.error)),
                            q.int(s.attempts),
                            q.int(s.started_at),
                            s.completed_at.map_or("NULL".into(), |t| q.int(t)),
                        ];
                        q.push(&vals.join(", ")).push(") ON CONFLICT DO NOTHING");
                        rep.steps += q.build().execute(&mut *tx).await?.rows_affected() as usize;
                    }
                    continue;
                }
            };
            // timestamps travel as text in `args`; bind them as integers
            let int_cols: &[usize] = match sql.split_whitespace().nth(2) {
                Some("memories") => &[7, 8],
                Some("events") => &[7],
                Some("checkpoints") => &[11],
                _ => &[],
            };
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
            for (i, a) in args.into_iter().enumerate() {
                query = if int_cols.contains(&i) {
                    query.bind(a.and_then(|v| v.parse::<i64>().ok()))
                } else {
                    query.bind(a)
                };
            }
            let inserted = query.execute(&mut *tx).await?.rows_affected() > 0;
            let counter = match sql.split_whitespace().nth(2) {
                Some("memories") => &mut rep.memories,
                Some("events") => &mut rep.events,
                _ => &mut rep.checkpoints,
            };
            if inserted {
                *counter += 1;
            } else {
                rep.skipped += 1;
            }
        }
        tx.commit().await?;
        Ok(rep)
    }

    /// Consistent online copy of a SQLite database (`VACUUM INTO`).
    pub async fn backup_sqlite(&self, path: &str) -> Result<()> {
        if self.backend() != crate::Backend::Sqlite {
            return Err(Error::Invalid("backup is for SQLite; for Postgres use pg_dump (or `memdusa export`)".into()));
        }
        if std::path::Path::new(path).exists() {
            return Err(Error::Conflict(format!("{path} already exists")));
        }
        sqlx::query("VACUUM INTO $1").bind(path.to_owned()).execute(&self.pool).await?;
        Ok(())
    }
}
