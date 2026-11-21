//! The hand-off brief: everything a fresh agent (possibly a different
//! harness) needs to continue where the previous one stopped.

use std::fmt::Write;

use crate::Result;
use crate::model::*;
use crate::store::Store;

/// Event kinds written automatically by harness hooks.
pub const FILE_CHANGE: &str = "file_change";
pub const COMMAND: &str = "command";

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct Brief {
    pub project: String,
    pub checkpoints: Vec<Checkpoint>,
    pub active_runs: Vec<RunWithSteps>,
    pub memories: Vec<Memory>,
    pub recent_events: Vec<Event>,
    /// The same content rendered as Markdown, ready to paste into a prompt.
    pub markdown: String,
}

/// `user` is the caller (on a multi-user server): it decides whose global
/// memories are included.
pub async fn build(store: &Store, project: &str, user: Option<&str>) -> Result<Brief> {
    let checkpoints = store.checkpoints_latest(project, 5).await?;
    let mut active_runs = vec![];
    for run in store.run_list(Some(project), Some(RunStatus::Running), 10).await? {
        active_runs.push(store.run_get(&run.id).await?);
    }
    let memories = store
        .memory_search(MemoryQuery {
            project: Some(project.into()),
            limit: Some(50),
            user: user.map(String::from),
            ..Default::default()
        })
        .await?;
    let recent_events = store.events_recent(project, None, 100).await?;

    let markdown = render(project, &checkpoints, &active_runs, &memories, &recent_events);
    Ok(Brief { project: project.into(), checkpoints, active_runs, memories, recent_events, markdown })
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

fn render(
    project: &str,
    checkpoints: &[Checkpoint],
    runs: &[RunWithSteps],
    memories: &[Memory],
    events: &[Event],
) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "# Resume brief: {project}\n");

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
        for kind in [MemoryKind::Preference, MemoryKind::Decision, MemoryKind::Fact, MemoryKind::Todo, MemoryKind::Note]
        {
            let group: Vec<_> = memories.iter().filter(|m| m.kind == kind).collect();
            if group.is_empty() {
                continue;
            }
            let _ = writeln!(o, "**{}s:**", kind.as_str());
            for m in group {
                let scope = if m.project.is_none() { " _(global)_" } else { "" };
                let _ = writeln!(o, "- {}{scope}", m.content);
            }
            o.push('\n');
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
        let others: Vec<_> = events.iter().filter(|e| e.kind != FILE_CHANGE && e.kind != COMMAND).take(12).collect();

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
