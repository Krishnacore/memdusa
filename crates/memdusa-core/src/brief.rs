//! The hand-off brief: everything a fresh agent (possibly a different
//! harness) needs to continue where the previous one stopped.

use std::fmt::Write;

use crate::Result;
use crate::model::*;
use crate::store::Store;

/// Event kinds written automatically by harness hooks.
pub const FILE_CHANGE: &str = "file_change";
pub const AGENT_MESSAGE: &str = "agent_message";
pub const COMPACT_SUMMARY: &str = "compact_summary";
pub const COMMAND: &str = "command";
pub const STOP_GUARD: &str = "stop_guard";
/// Which memories were already shown to an agent in a session (`data: {"ids": [...]}`).
pub const MEMORY_RECALL: &str = "memory_recall";
/// Bookkeeping events that would only be noise in the brief.
const HIDDEN: &[&str] = &[STOP_GUARD, AGENT_MESSAGE, COMPACT_SUMMARY, MEMORY_RECALL];
/// How much of a compaction summary the brief quotes.
const SUMMARY_CHARS: usize = 1_500;
/// Default character budget of the brief's Memory section.
pub const MEMORY_BUDGET: usize = 4_000;
/// Memories loaded when there is nothing to rank them by.
const RECENT_LIMIT: i64 = 50;
/// Memories considered when ranking by the current focus.
const CANDIDATES: i64 = 200;
/// How much of the focus text is used as the search query.
const FOCUS_CHARS: usize = 2_000;

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct Brief {
    pub project: String,
    pub checkpoints: Vec<Checkpoint>,
    pub active_runs: Vec<RunWithSteps>,
    pub memories: Vec<Memory>,
    pub recent_events: Vec<Event>,
    /// How the caller's code state differs from the latest checkpoint's.
    pub git_warnings: Vec<String>,
    /// The same content rendered as Markdown, ready to paste into a prompt.
    pub markdown: String,
}

/// `user` is the caller (on a multi-user server): it decides whose global
/// memories are included.
/// `current_git` is the caller's code state, compared against the latest
/// checkpoint's to warn about a different branch, commit or machine.
pub async fn build(store: &Store, project: &str, user: Option<&str>, current_git: Option<&GitState>) -> Result<Brief> {
    build_with_budget(store, project, user, current_git, MEMORY_BUDGET).await
}

/// [`build`] with an explicit character budget for the Memory section.
pub async fn build_with_budget(
    store: &Store,
    project: &str,
    user: Option<&str>,
    current_git: Option<&GitState>,
    memory_budget: usize,
) -> Result<Brief> {
    let checkpoints = store.checkpoints_latest(project, 5).await?;
    let mut active_runs = vec![];
    for run in store.run_list(Some(project), Some(RunStatus::Running), 10).await? {
        active_runs.push(store.run_get(&run.id).await?);
    }
    let recent_events = store.events_recent(project, None, 100).await?;
    let focus = focus_text(store, project, checkpoints.first()).await?;
    let (memories, omitted) =
        select_memories(store, project, user, focus.as_deref(), memory_budget, &recent_events).await?;

    let git_warnings = match (checkpoints.first().and_then(|c| c.git.as_ref()), current_git) {
        (Some(saved), Some(current)) => saved.drift(current),
        _ => vec![],
    };
    let markdown = render(project, &checkpoints, &active_runs, &memories, omitted, &recent_events, &git_warnings);
    Ok(Brief { project: project.into(), checkpoints, active_runs, memories, recent_events, git_warnings, markdown })
}

/// What the current work is about: the latest checkpoint's goal, next steps, blockers and files, plus
/// the latest prompt when it is newer than that checkpoint (or there is no checkpoint).
async fn focus_text(store: &Store, project: &str, latest: Option<&Checkpoint>) -> Result<Option<String>> {
    let mut parts: Vec<String> = vec![];
    if let Some(c) = latest {
        parts.push(c.goal.clone());
        parts.extend(c.next.iter().chain(&c.blockers).chain(&c.files).cloned());
    }
    let prompt = store.events_recent_kind(project, None, "prompt", 1).await?.into_iter().next();
    if let Some(e) = prompt.filter(|e| latest.is_none_or(|c| e.created_at > c.created_at)) {
        let full = e.data.as_ref().and_then(|d| d.get("text")).and_then(|t| t.as_str());
        parts.push(full.unwrap_or(&e.content).to_owned());
    }
    let text: String = parts.join(" ").chars().take(FOCUS_CHARS).collect();
    Ok((!text.trim().is_empty()).then_some(text))
}

/// Rendered size of a memory's bullet in the Memory section (what the budget counts).
fn memory_cost(m: &Memory, events: &[Event]) -> usize {
    m.content.chars().count() + stale_note(m, events).chars().count() + 16
}

/// Pick the memories to show and how many candidates were left out. Order of priority: preferences, then
/// memories matching the focus (best score first), then the rest by recency; the budget cuts the tail.
/// Without focus text this is the most recent memories, as before.
async fn select_memories(
    store: &Store,
    project: &str,
    user: Option<&str>,
    focus: Option<&str>,
    budget: usize,
    events: &[Event],
) -> Result<(Vec<Memory>, usize)> {
    let query = |query: Option<String>, limit: i64| MemoryQuery {
        query,
        project: Some(project.into()),
        limit: Some(limit),
        user: user.map(String::from),
        ..Default::default()
    };
    let (candidates, scored) = match focus {
        None => (store.memory_search(query(None, RECENT_LIMIT)).await?, vec![]),
        Some(f) => (
            store.memory_search(query(None, CANDIDATES)).await?,
            store.memory_search_ranked(query(Some(f.into()), CANDIDATES)).await?,
        ),
    };
    let mut ordered: Vec<&Memory> = candidates.iter().filter(|m| m.kind == MemoryKind::Preference).collect();
    ordered.extend(scored.iter().filter(|(m, s)| *s > 0.0 && m.kind != MemoryKind::Preference).map(|(m, _)| m));
    for m in &candidates {
        if !ordered.iter().any(|o| o.id == m.id) {
            ordered.push(m);
        }
    }

    let mut shown = vec![];
    let mut used = 0;
    for m in &ordered {
        let cost = memory_cost(m, events);
        if m.kind != MemoryKind::Preference && used + cost > budget {
            break;
        }
        used += cost;
        shown.push((*m).clone());
    }
    let omitted = ordered.len() - shown.len();
    Ok((shown, omitted))
}

fn ago(ts: Millis) -> String {
    let s = (now_ms() - ts).max(0) / 1000;
    match s {
        0..60 => format!("{s}s ago"),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

fn bullets(out: &mut String, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let _ = writeln!(out, "**{title}:**");
    for i in items {
        let _ = writeln!(out, "- {i}");
    }
    out.push('\n');
}

const PATH_EXTENSIONS: &[&str] =
    &["rs", "py", "ts", "tsx", "js", "go", "java", "rb", "md", "toml", "json", "yaml", "yml", "sql"];

/// Path-like tokens of a text: contain `/` or end in a common source extension.
fn paths_in(text: &str) -> Vec<String> {
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

/// ` _(may be outdated: <path> changed <age>)_` when a `file_change` event touched a file the
/// memory mentions after the memory was last updated.
fn stale_note(m: &Memory, events: &[Event]) -> String {
    let hit = paths_in(&m.content).into_iter().find_map(|p| {
        events
            .iter()
            .filter(|e| e.kind == FILE_CHANGE && e.created_at > m.updated_at && same_path(&p, &e.content))
            .map(|e| e.created_at)
            .max()
            .map(|at| (p, at))
    });
    hit.map(|(p, at)| format!(" _(may be outdated: {p} changed {})_", ago(at))).unwrap_or_default()
}

/// Memories grouped by kind, as Markdown (the body of the brief's Memory section).
/// Memories that mention a file changed since they were written get a staleness flag.
fn memory_groups(memories: &[Memory], events: &[Event]) -> String {
    let mut o = String::new();
    for kind in [MemoryKind::Preference, MemoryKind::Decision, MemoryKind::Fact, MemoryKind::Todo, MemoryKind::Note] {
        let group: Vec<_> = memories.iter().filter(|m| m.kind == kind).collect();
        if group.is_empty() {
            continue;
        }
        let _ = writeln!(o, "**{}s:**", kind.as_str());
        for m in group {
            let scope = if m.project.is_none() { " _(global)_" } else { "" };
            let _ = writeln!(o, "- {}{scope}{}", m.content, stale_note(m, events));
        }
        o.push('\n');
    }
    o
}

/// A standalone Markdown document of a project's memories (plus visible globals).
pub fn memories_markdown(project: &str, memories: &[Memory]) -> String {
    let mut o = format!("# Memories: {project}\n\n");
    if memories.is_empty() {
        o.push_str("_No memories saved yet._\n");
    } else {
        o.push_str(&memory_groups(memories, &[]));
    }
    o
}

fn render(
    project: &str,
    checkpoints: &[Checkpoint],
    runs: &[RunWithSteps],
    memories: &[Memory],
    omitted: usize,
    events: &[Event],
    git_warnings: &[String],
) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "# Resume brief: {project}\n");
    if !git_warnings.is_empty() {
        let _ = writeln!(o, "## ⚠ Code state differs from the last checkpoint");
        for w in git_warnings {
            let _ = writeln!(o, "- {w}");
        }
        let _ = writeln!(o, "\nVerify the actual state of the code before trusting the notes below.\n");
    }

    if checkpoints.is_empty() && runs.is_empty() && memories.is_empty() && events.is_empty() {
        let _ = writeln!(o, "_Nothing recorded yet for this project. Start fresh, and save checkpoints as you go._");
        return o;
    }

    if !checkpoints.is_empty() {
        let _ = writeln!(o, "## Where things stand");
        for c in checkpoints {
            let task = c.task.as_deref().map(|t| format!(" [{t}]")).unwrap_or_default();
            let by = c.source.as_deref().map(|s| format!(" by {s}")).unwrap_or_default();
            let _ = writeln!(o, "\n### Goal{task}: {}\n_checkpoint {}{by}_\n", c.goal, ago(c.created_at));
            if let Some(g) = &c.git {
                let _ = writeln!(o, "**Code:** {}\n", g.summary());
            }
            bullets(&mut o, "Done", &c.done);
            bullets(&mut o, "Next", &c.next);
            bullets(&mut o, "Blockers", &c.blockers);
            bullets(&mut o, "Key files", &c.files);
            if let Some(n) = &c.notes {
                let _ = writeln!(o, "**Notes:** {n}");
            }
        }
        o.push('\n');
    }

    // A checkpoint the agent wrote already says where things stand: only show what came after it.
    // Automatic fallback checkpoints are written at session end, after the last words, so they don't count.
    let since = checkpoints
        .iter()
        .find(|c| !c.source.as_deref().is_some_and(|s| s.ends_with(" (auto)")))
        .map_or(0, |c| c.created_at);
    let latest =
        |kind: &str| events.iter().filter(|e| e.kind == kind && e.created_at > since).max_by_key(|e| e.created_at);
    let (message, summary) = (latest(AGENT_MESSAGE), latest(COMPACT_SUMMARY));
    if message.is_some() || summary.is_some() {
        let _ = writeln!(o, "## Last words from the previous session");
        let quote = |t: &str| t.lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n");
        if let Some(m) = message {
            let by = m.source.as_deref().map(|s| format!(" by {s}")).unwrap_or_default();
            let _ = writeln!(o, "\n_last message{by}, {}_\n\n{}", ago(m.created_at), quote(&m.content));
        }
        if let Some(s) = summary {
            let text = if s.content.chars().count() > SUMMARY_CHARS {
                let mut t: String = s.content.chars().take(SUMMARY_CHARS).collect();
                t.push('…');
                t
            } else {
                s.content.clone()
            };
            let by = s.source.as_deref().map(|s| format!(" by {s}")).unwrap_or_default();
            let _ = writeln!(o, "\n_compaction summary{by}, {}_\n\n{}", ago(s.created_at), quote(&text));
        }
        o.push('\n');
    }

    if !runs.is_empty() {
        let _ = writeln!(o, "## Unfinished durable runs");
        for r in runs {
            let done = r.steps.iter().filter(|s| s.status == StepStatus::Completed).count();
            let state = if r.run.stale {
                "STALE (owner gone, claim it with run_claim)".to_string()
            } else {
                format!("leased by {}", r.run.owner.as_deref().unwrap_or("?"))
            };
            let _ = writeln!(o, "- **{}** (`{}`): {done}/{} steps done, {state}", r.run.name, r.run.id, r.steps.len());
            for s in r.steps.iter().filter(|s| s.status != StepStatus::Completed) {
                let err = s.error.as_deref().map(|e| format!(" (error: {e})")).unwrap_or_default();
                let _ = writeln!(o, "  - step `{}` is {}{err}", s.key, s.status.as_str());
            }
        }
        o.push('\n');
    }

    if !memories.is_empty() {
        let _ = writeln!(o, "## Memory");
        o.push_str(&memory_groups(memories, events));
        if omitted > 0 {
            let _ = writeln!(
                o,
                "_{omitted} more memories not shown: use the memory_search tool or `memdusa recall <query>` to see them._"
            );
        }
        o.push('\n');
    }

    if !events.is_empty() {
        // Hooks journal every edit and shell command; collapse those so the
        // brief stays readable.
        let mut files: Vec<&str> = vec![];
        for e in events.iter().filter(|e| e.kind == FILE_CHANGE) {
            if !files.contains(&e.content.as_str()) && files.len() < 20 {
                files.push(&e.content);
            }
        }
        let commands: Vec<_> = events.iter().filter(|e| e.kind == COMMAND).take(5).collect();
        let others: Vec<_> = events
            .iter()
            .filter(|e| e.kind != FILE_CHANGE && e.kind != COMMAND && !HIDDEN.contains(&e.kind.as_str()))
            .take(12)
            .collect();

        let _ = writeln!(o, "## Recent activity");
        if !files.is_empty() {
            let _ = writeln!(o, "**Files changed recently:** {}\n", files.join(", "));
        }
        if !commands.is_empty() {
            let _ = writeln!(o, "**Last commands:**");
            for e in commands {
                let _ = writeln!(o, "- `{}` ({})", e.content, ago(e.created_at));
            }
            o.push('\n');
        }
        if !others.is_empty() {
            let _ = writeln!(o, "**Events (newest first):**");
            for e in others {
                let by = e.source.as_deref().map(|s| format!(" [{s}]")).unwrap_or_default();
                let _ = writeln!(o, "- {} {}{by}: {}", ago(e.created_at), e.kind, e.content);
            }
        }
    }
    o
}
