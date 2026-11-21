//! Retention: keep the database (and the brief) fast as history grows.
//!
//! * Hook activity (file changes, commands, prompts, session start/end,
//!   compactions) older than `activity_days` is **summarized, then deleted**:
//!   each old session becomes one `session_summary` event, so the timeline
//!   keeps its shape.
//! * Superseded checkpoints older than `checkpoint_days` are deleted; the
//!   latest checkpoint of every task is always kept.
//! * Finished runs (and their steps) older than `run_days` are deleted.
//! * Memories, and events agents logged explicitly, are never pruned.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;

use crate::Result;
use crate::model::*;
use crate::store::{Q, Store};

/// Event kinds written automatically by harness hooks.
pub const ACTIVITY_KINDS: &[&str] = &["file_change", "command", "prompt", "session_start", "session_end", "compaction"];
pub const SESSION_SUMMARY: &str = "session_summary";

const DAY_MS: i64 = 86_400_000;
const LAST_PRUNE_KEY: &str = "last_prune_at";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RetentionPolicy {
    /// Days of raw hook activity to keep (0 = keep forever).
    pub activity_days: i64,
    /// Days of superseded checkpoints to keep (0 = keep forever).
    pub checkpoint_days: i64,
    /// Days of finished runs to keep (0 = keep forever).
    pub run_days: i64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self { activity_days: 30, checkpoint_days: 90, run_days: 90 }
    }
}

/// Prune records older than these timestamps (`None` = keep).
#[derive(Debug, Clone, Copy, Default)]
pub struct Cutoffs {
    pub activity: Option<Millis>,
    pub checkpoints: Option<Millis>,
    pub runs: Option<Millis>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PruneReport {
    pub dry_run: bool,
    pub sessions_summarized: usize,
    pub events_deleted: usize,
    pub checkpoints_deleted: usize,
    pub runs_deleted: usize,
    pub steps_deleted: usize,
}

fn cutoff(days: i64) -> Option<Millis> {
    (days > 0).then(|| now_ms() - days * DAY_MS)
}

fn file_list(files: &[String], max: usize) -> String {
    let shown = files.iter().take(max).cloned().collect::<Vec<_>>().join(", ");
    match files.len().saturating_sub(max) {
        0 => shown,
        more => format!("{shown}, +{more} more"),
    }
}

#[derive(Default)]
struct SessionAgg {
    ids: Vec<String>,
    files: Vec<String>,
    commands: usize,
    prompts: Vec<String>,
    sources: HashMap<String, usize>,
    first: Millis,
    last: Millis,
}

impl Store {
    /// Delete `ids` from `table` in chunks (keeps statements a sane size).
    async fn delete_ids(&self, table: &str, column: &str, ids: &[String]) -> Result<usize> {
        let mut n = 0;
        for chunk in ids.chunks(500) {
            let mut q = Q::new(&format!("DELETE FROM {table} WHERE {column} IN ("));
            let ps: Vec<String> = chunk.iter().map(|id| q.text(id)).collect();
            q.push(&ps.join(", ")).push(")");
            n += q.build().execute(&self.pool).await?.rows_affected() as usize;
        }
        Ok(n)
    }

    pub async fn prune(&self, policy: &RetentionPolicy, project: Option<&str>, dry_run: bool) -> Result<PruneReport> {
        let cutoffs = Cutoffs {
            activity: cutoff(policy.activity_days),
            checkpoints: cutoff(policy.checkpoint_days),
            runs: cutoff(policy.run_days),
        };
        self.prune_older_than(cutoffs, project, dry_run).await
    }

    /// Like [`Store::prune`], with explicit timestamps instead of day counts.
    pub async fn prune_older_than(
        &self,
        cutoffs: Cutoffs,
        project: Option<&str>,
        dry_run: bool,
    ) -> Result<PruneReport> {
        let mut report = PruneReport { dry_run, ..Default::default() };
        let project_filter = |q: &mut Q| {
            if let Some(p) = project {
                let p = q.text(p);
                q.push(&format!(" AND project = {p}"));
            }
        };

        // -------- hook activity -> one summary per session
        if let Some(before) = cutoffs.activity {
            let mut q =
                Q::new("SELECT id, project, session, source, kind, content, created_at FROM events WHERE kind IN (");
            let kinds: Vec<String> = ACTIVITY_KINDS.iter().map(|k| q.text(*k)).collect();
            let t = q.int(before);
            q.push(&format!("{}) AND created_at < {t}", kinds.join(", ")));
            project_filter(&mut q);
            q.push(" ORDER BY created_at");
            let rows = q.build().fetch_all(&self.pool).await?;

            let mut sessions: BTreeMap<(String, Option<String>), SessionAgg> = BTreeMap::new();
            for r in &rows {
                let key = (r.try_get::<String, _>("project")?, r.try_get::<Option<String>, _>("session")?);
                let agg = sessions.entry(key).or_default();
                let (kind, content, at): (String, String, i64) =
                    (r.try_get("kind")?, r.try_get("content")?, r.try_get("created_at")?);
                agg.ids.push(r.try_get("id")?);
                if agg.first == 0 {
                    agg.first = at;
                }
                agg.last = at;
                if let Some(src) = r.try_get::<Option<String>, _>("source")? {
                    *agg.sources.entry(src).or_default() += 1;
                }
                match kind.as_str() {
                    "file_change" if !agg.files.contains(&content) => agg.files.push(content),
                    "command" => agg.commands += 1,
                    "prompt" => agg.prompts.push(content),
                    _ => {}
                }
            }
            report.sessions_summarized = sessions.len();
            report.events_deleted = rows.len();

            if !dry_run {
                for ((proj, session), agg) in &sessions {
                    let source = agg.sources.iter().max_by_key(|(_, n)| **n).map(|(s, _)| s.clone());
                    let label = session.as_deref().map(|s| format!("Session `{}`", &s[..s.len().min(8)]));
                    let mut parts = vec![format!(
                        "{} file(s) changed{}",
                        agg.files.len(),
                        if agg.files.is_empty() { String::new() } else { format!(" ({})", file_list(&agg.files, 6)) }
                    )];
                    parts.push(format!("{} command(s)", agg.commands));
                    parts.push(format!("{} prompt(s)", agg.prompts.len()));
                    let mut content = format!("{}: {}", label.unwrap_or_else(|| "Activity".into()), parts.join(", "));
                    if let Some(first) = agg.prompts.first() {
                        content
                            .push_str(&format!(". First request: \"{}\"", first.chars().take(160).collect::<String>()));
                    }
                    self.event_append(NewEvent {
                        project: proj.clone(),
                        session: session.clone(),
                        source,
                        kind: SESSION_SUMMARY.into(),
                        content,
                        data: Some(json!({
                            "files": agg.files.iter().take(50).collect::<Vec<_>>(),
                            "commands": agg.commands,
                            "prompts": agg.prompts.len(),
                            "from": agg.first,
                            "to": agg.last,
                        })),
                        created_at: Some(agg.last),
                    })
                    .await?;
                }
                let ids: Vec<String> = sessions.values().flat_map(|a| a.ids.iter().cloned()).collect();
                self.delete_ids("events", "id", &ids).await?;
            }
        }

        // -------- superseded checkpoints (the latest per task always stays)
        if let Some(before) = cutoffs.checkpoints {
            let mut q = Q::new("SELECT id, project, task, created_at FROM checkpoints WHERE 1=1");
            project_filter(&mut q);
            let rows = q.build().fetch_all(&self.pool).await?;
            let mut latest: HashMap<(String, Option<String>), (i64, String)> = HashMap::new();
            let mut all = vec![];
            for r in &rows {
                let (id, at): (String, i64) = (r.try_get("id")?, r.try_get("created_at")?);
                let key = (r.try_get::<String, _>("project")?, r.try_get::<Option<String>, _>("task")?);
                let e = latest.entry(key.clone()).or_insert((at, id.clone()));
                if (at, &id) > (e.0, &e.1) {
                    *e = (at, id.clone());
                }
                all.push((key, id, at));
            }
            let doomed: Vec<String> = all
                .into_iter()
                .filter(|(key, id, at)| *at < before && latest.get(key).is_some_and(|(_, l)| l != id))
                .map(|(_, id, _)| id)
                .collect();
            report.checkpoints_deleted = doomed.len();
            if !dry_run {
                self.delete_ids("checkpoints", "id", &doomed).await?;
            }
        }

        // -------- finished runs
        if let Some(before) = cutoffs.runs {
            let mut q = Q::new("SELECT id FROM runs WHERE status <> ");
            let (s, t) = (q.text(RunStatus::Running.as_str()), q.int(before));
            q.push(&format!("{s} AND updated_at < {t}"));
            project_filter(&mut q);
            let ids: Vec<String> = q
                .build()
                .fetch_all(&self.pool)
                .await?
                .iter()
                .map(|r| r.try_get("id"))
                .collect::<std::result::Result<_, _>>()?;
            report.runs_deleted = ids.len();
            if dry_run {
                for chunk in ids.chunks(500) {
                    let mut q = Q::new("SELECT COUNT(*) AS n FROM steps WHERE run_id IN (");
                    let ps: Vec<String> = chunk.iter().map(|id| q.text(id)).collect();
                    q.push(&ps.join(", ")).push(")");
                    report.steps_deleted += q.build().fetch_one(&self.pool).await?.try_get::<i64, _>("n")? as usize;
                }
            } else {
                report.steps_deleted = self.delete_ids("steps", "run_id", &ids).await?;
                self.delete_ids("runs", "id", &ids).await?;
            }
        }

        if !dry_run && project.is_none() {
            let mut q = Q::new("INSERT INTO memdusa_meta (key, value) VALUES (");
            let (k, v) = (q.text(LAST_PRUNE_KEY), q.text(now_ms().to_string()));
            q.push(&format!("{k}, {v}) ON CONFLICT (key) DO UPDATE SET value = excluded.value"));
            q.build().execute(&self.pool).await?;
        }
        Ok(report)
    }

    /// Prune everything if the last full prune is older than `every_ms`.
    /// Cheap to call often (one indexed read when nothing is due).
    pub async fn maybe_prune(&self, policy: &RetentionPolicy, every_ms: i64) -> Result<Option<PruneReport>> {
        let mut q = Q::new("SELECT value FROM memdusa_meta WHERE key = ");
        let k = q.text(LAST_PRUNE_KEY);
        q.push(&k);
        let last: i64 = q
            .build()
            .fetch_optional(&self.pool)
            .await?
            .and_then(|r| r.try_get::<String, _>("value").ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if now_ms() - last < every_ms {
            return Ok(None);
        }
        self.prune(policy, None, false).await.map(Some)
    }
}
