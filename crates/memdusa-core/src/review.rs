//! Memory review: memdusa has no LLM, so consolidation is agent-driven. This module *finds* what needs
//! consolidating (related memories to merge, memories that may be outdated, abandoned todos) and the
//! agent acts on it with `memory_update` / `memory_forget`.

use std::fmt::Write;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::brief::FILE_CHANGE;
use crate::model::*;
use crate::store::{Store, similarity};

/// Pairwise similarity from which two memories count as related (below it they are unrelated;
/// from `NEAR_DUPLICATE` up they were merged on save).
pub const RELATED: f64 = 0.35;
/// Most memories in one related group.
const MAX_GROUP: usize = 6;
/// Most related groups reported.
const MAX_GROUPS: usize = 20;
/// Memories fetched per kind (5 kinds: ~500 in total).
const PER_KIND: i64 = 100;
/// File-change events looked at when checking for stale memories.
const EVENT_WINDOW: i64 = 500;
/// A todo not updated for this long is probably done or abandoned.
pub const OLD_TODO_DAYS: i64 = 30;
const DAY_MS: i64 = 86_400_000;

/// A memory as shown in a review: enough for the agent to act on it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewItem {
    pub id: String,
    pub kind: MemoryKind,
    /// `None` = global.
    pub project: Option<String>,
    pub content: String,
    pub updated_at: Millis,
}

impl From<&Memory> for ReviewItem {
    fn from(m: &Memory) -> Self {
        Self {
            id: m.id.clone(),
            kind: m.kind,
            project: m.project.clone(),
            content: m.content.clone(),
            updated_at: m.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StaleItem {
    #[serde(flatten)]
    pub memory: ReviewItem,
    /// The file the memory mentions that changed after the memory was last updated.
    pub path: String,
    pub changed_at: Millis,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MemoryReview {
    /// Groups of related memories (same kind and scope) that could become one.
    pub related: Vec<Vec<ReviewItem>>,
    /// Memories that mention a file changed after they were last updated.
    pub stale: Vec<StaleItem>,
    /// Todos not updated for [`OLD_TODO_DAYS`]+ days.
    pub old: Vec<ReviewItem>,
}

impl MemoryReview {
    /// Number of memories needing attention (a memory in several lists counts in each).
    pub fn total(&self) -> usize {
        self.related.iter().map(Vec::len).sum::<usize>() + self.stale.len() + self.old.len()
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// The review as Markdown, with ids, for the agent.
    pub fn markdown(&self, project: &str, now: Millis) -> String {
        let mut o = format!("# Memory review: {project}\n\n");
        if self.is_empty() {
            o.push_str("_Nothing to review: no related, outdated or abandoned memories found._\n");
            return o;
        }
        let item = |m: &ReviewItem| {
            let scope = if m.project.is_none() { " (global)" } else { "" };
            format!("`{}` [{}{scope}] {}", m.id, m.kind.as_str(), m.content)
        };
        if !self.related.is_empty() {
            o.push_str(
                "## Related memories\nMerge each group into one: `memory_update` the best one with the combined \
                 content, then `memory_forget` the rest.\n\n",
            );
            for (i, g) in self.related.iter().enumerate() {
                let _ = writeln!(o, "**Group {}:**", i + 1);
                for m in g {
                    let _ = writeln!(o, "- {}", item(m));
                }
                o.push('\n');
            }
        }
        if !self.stale.is_empty() {
            o.push_str(
                "## May be outdated\nThese mention a file that changed after the memory was written. Check them \
                 against the code, then `memory_update` or `memory_forget`.\n\n",
            );
            for s in &self.stale {
                let _ = writeln!(o, "- {} _({} changed {})_", item(&s.memory), s.path, ago(now, s.changed_at));
            }
            o.push('\n');
        }
        if !self.old.is_empty() {
            let _ = writeln!(
                o,
                "## Old todos\nNot updated for {OLD_TODO_DAYS}+ days. Done or abandoned? `memory_forget` them, or \
                 `memory_update` if still open.\n"
            );
            for m in &self.old {
                let _ = writeln!(o, "- {} _(updated {})_", item(m), ago(now, m.updated_at));
            }
            o.push('\n');
        }
        o
    }
}

fn ago(now: Millis, ts: Millis) -> String {
    let s = (now - ts).max(0) / 1000;
    match s {
        0..60 => format!("{s}s ago"),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

const PATH_EXTENSIONS: &[&str] =
    &["rs", "py", "ts", "tsx", "js", "go", "java", "rb", "md", "toml", "json", "yaml", "yml", "sql"];

/// Path-like tokens of a text: contain `/` or end in a common source extension.
pub(crate) fn paths_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for raw in text.split_whitespace() {
        let t = raw
            .trim_matches(|c: char| !(c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-')))
            .trim_start_matches("./");
        let t = t.trim_end_matches('.');
        if t.is_empty() || t.contains("://") || t.ends_with('/') {
            continue;
        }
        let ext = t.rsplit_once('.').map(|(_, e)| e).filter(|e| PATH_EXTENSIONS.contains(e));
        if (t.contains('/') || ext.is_some()) && !out.iter().any(|p| p == t) {
            out.push(t.to_owned());
        }
    }
    out
}

/// A changed file `changed` is the memory's `path` when equal or one is a path suffix of the other.
fn same_path(path: &str, changed: &str) -> bool {
    let (a, b) = (path.trim_start_matches('/'), changed.trim_start_matches('/'));
    a == b || changed.ends_with(&format!("/{a}")) || path.ends_with(&format!("/{b}"))
}

/// A file the memory mentions that a `file_change` event touched after the memory was last updated,
/// with when it last changed.
pub(crate) fn stale_hit(m: &Memory, events: &[Event]) -> Option<(String, Millis)> {
    paths_in(&m.content).into_iter().find_map(|p| {
        events
            .iter()
            .filter(|e| e.kind == FILE_CHANGE && e.created_at > m.updated_at && same_path(&p, &e.content))
            .map(|e| e.created_at)
            .max()
            .map(|at| (p, at))
    })
}

/// Single-link groups of related memories: same kind and scope, pairwise similarity in
/// `RELATED..NEAR_DUPLICATE` links two memories; groups of 2+ are returned, capped in size and number.
fn related_groups(memories: &[Memory]) -> Vec<Vec<ReviewItem>> {
    let n = memories.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut i: usize) -> usize {
        while p[i] != i {
            p[i] = p[p[i]];
            i = p[i];
        }
        i
    }
    for i in 0..n {
        for j in i + 1..n {
            let (a, b) = (&memories[i], &memories[j]);
            if a.kind != b.kind || a.project != b.project || (a.project.is_none() && a.user != b.user) {
                continue;
            }
            let s = similarity(&a.content, &b.content);
            if (RELATED..crate::store::NEAR_DUPLICATE).contains(&s) {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                parent[ri] = rj;
            }
        }
    }
    let mut groups: Vec<Vec<usize>> = vec![];
    let mut root_group: std::collections::HashMap<usize, usize> = Default::default();
    for i in 0..n {
        let r = find(&mut parent, i);
        let g = *root_group.entry(r).or_insert_with(|| {
            groups.push(vec![]);
            groups.len() - 1
        });
        groups[g].push(i);
    }
    groups
        .into_iter()
        .filter(|g| g.len() >= 2)
        .take(MAX_GROUPS)
        .map(|g| g.into_iter().take(MAX_GROUP).map(|i| ReviewItem::from(&memories[i])).collect())
        .collect()
}

/// Review `memories` against recent `file_change` `events` at time `now`. Memories are expected
/// newest first (as the store returns them).
pub fn review(memories: &[Memory], events: &[Event], now: Millis) -> MemoryReview {
    let stale = memories
        .iter()
        .filter_map(|m| {
            stale_hit(m, events).map(|(path, changed_at)| StaleItem { memory: ReviewItem::from(m), path, changed_at })
        })
        .collect();
    let old = memories
        .iter()
        .filter(|m| m.kind == MemoryKind::Todo && now - m.updated_at >= OLD_TODO_DAYS * DAY_MS)
        .map(ReviewItem::from)
        .collect();
    MemoryReview { related: related_groups(memories), stale, old }
}

impl Store {
    /// Review a project's memories plus the globals visible to `user`.
    pub async fn memory_review(&self, project: &str, user: Option<&str>) -> Result<MemoryReview> {
        self.memory_review_at(project, user, now_ms()).await
    }

    /// [`Store::memory_review`] as of `now` (for tests).
    pub async fn memory_review_at(&self, project: &str, user: Option<&str>, now: Millis) -> Result<MemoryReview> {
        let mut memories = vec![];
        for kind in [MemoryKind::Preference, MemoryKind::Decision, MemoryKind::Fact, MemoryKind::Todo, MemoryKind::Note]
        {
            memories.extend(
                self.memory_search(MemoryQuery {
                    project: Some(project.into()),
                    kind: Some(kind),
                    limit: Some(PER_KIND),
                    user: user.map(String::from),
                    ..Default::default()
                })
                .await?,
            );
        }
        let events = self.events_recent_kind(project, None, FILE_CHANGE, EVENT_WINDOW).await?;
        Ok(review(&memories, &events, now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(id: &str, kind: MemoryKind, project: Option<&str>, content: &str, updated_at: Millis) -> Memory {
        Memory {
            id: id.into(),
            project: project.map(String::from),
            kind,
            content: content.into(),
            tags: vec![],
            source: None,
            user: None,
            created_at: updated_at,
            updated_at,
            merged: false,
        }
    }

    #[test]
    fn groups_are_single_link_capped_and_same_kind_and_scope() {
        let p = Some("p");
        let ms: Vec<Memory> = (0..8)
            .map(|i| mem(&format!("a{i}"), MemoryKind::Fact, p, &format!("deploy pipeline config staging step{i}"), 1))
            .collect();
        let g = related_groups(&ms);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].len(), MAX_GROUP);

        let mixed = vec![
            mem("x", MemoryKind::Fact, p, "deploy pipeline uses github actions workflow", 1),
            mem("y", MemoryKind::Note, p, "deploy pipeline uses github actions runners", 1),
            mem("z", MemoryKind::Fact, None, "deploy pipeline uses github actions secrets", 1),
        ];
        assert!(related_groups(&mixed).is_empty());
    }
}
