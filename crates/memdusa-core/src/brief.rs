//! The hand-off brief: everything a fresh agent (possibly a different
//! harness) needs to continue where the previous one stopped.

use std::collections::HashSet;
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
/// A worker's session started under `MEMDUSA_TASK` (`data: {task, parent, harness, source}`).
pub const DELEGATED_SESSION: &str = "delegated_session";
pub const SESSION_END: &str = "session_end";
/// Bookkeeping events that would only be noise in the brief.
const HIDDEN: &[&str] = &[STOP_GUARD, AGENT_MESSAGE, COMPACT_SUMMARY, MEMORY_RECALL, DELEGATED_SESSION];
/// How far back the coordinator's "Delegated work" section looks, and how many tasks it lists.
const DELEGATED_WINDOW_MS: Millis = 7 * 86_400_000;
const DELEGATED_MAX: usize = 10;
/// How much of a worker's last message the coordinator's brief quotes.
const DELEGATED_MESSAGE_CHARS: usize = 300;
/// Checkpoints of the task shown in a worker's brief.
const TASK_CHECKPOINTS: i64 = 3;
/// Next steps listed per delegated task.
const DELEGATED_NEXT: usize = 3;
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
    /// Recent tasks workers were delegated (coordinator view; empty in a worker's own brief).
    pub delegated: Vec<DelegatedWork>,
    /// How the caller's code state differs from the checkpoint we compared with.
    pub git_warnings: Vec<String>,
    /// Calm note when the newest checkpoint came from a different worktree (no drift warning).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_worktree_note: Option<String>,
    /// The same content rendered as Markdown, ready to paste into a prompt.
    pub markdown: String,
}

/// One task a coordinator delegated to a worker, as the coordinator's brief shows it.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct DelegatedWork {
    pub task: String,
    /// `MEMDUSA_PARENT` of the latest worker session, if it gave one.
    pub parent: Option<String>,
    /// The harness of the latest worker session.
    pub harness: Option<String>,
    /// The latest worker session has started and not ended.
    pub active: bool,
    pub started_at: Millis,
    /// The task's newest checkpoint.
    pub checkpoint: Option<Checkpoint>,
    /// The last thing any worker of this task said (truncated).
    pub last_message: Option<String>,
    /// The delegated run this task is a step of (`run_delegate`); `None` for tasks delegated by env alone.
    pub run_id: Option<String>,
    pub run_name: Option<String>,
    /// The task's step: its status (`pending`, running by someone, stale, done, failed) replaces the
    /// session-based `active` guess.
    pub step: Option<Step>,
}

/// Most delegated runs, and steps per run, the coordinator's brief lists.
const DELEGATED_MAX_RUNS: usize = 5;
const DELEGATED_MAX_STEPS: usize = 20;
/// How much of a finished task's result the coordinator's brief quotes.
const DELEGATED_OUTPUT_CHARS: usize = 300;

fn event_str<'a>(e: &'a Event, key: &str) -> Option<&'a str> {
    e.data.as_ref()?.get(key)?.as_str()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

// ---------------------------------------------------------------- worktrees

/// Lexically normalize a path for comparison: forward slashes everywhere, no `.`/`..`.
/// No filesystem access (a path may live on another machine, or no longer exist).
fn norm_path(path: &str) -> String {
    let replaced = path.replace('\\', "/");
    // Windows canonicalize() prefixes paths with `\\?\`.
    let normalized = replaced.strip_prefix("//?/").unwrap_or(&replaced);
    let mut parts: Vec<&str> = vec![];
    for raw in normalized.split('/') {
        match raw {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// Path components are case-insensitive on Windows, case-sensitive elsewhere.
fn same_worktree_path(a: &str, b: &str) -> bool {
    if cfg!(windows) { a.eq_ignore_ascii_case(b) } else { a == b }
}

/// Is `path` the worktree root `root`, or a directory underneath it?
fn under_worktree(path: &str, root: &str) -> bool {
    same_worktree_path(path, root) || path.strip_prefix(root).is_some_and(|r| r.starts_with('/'))
}

/// A worktree path as seen from `current`: `../memdusa-eval2` for a sibling checkout.
fn display_worktree(other: &str, current: Option<&str>) -> String {
    let other = norm_path(other);
    match current {
        None => other,
        Some(c) => {
            let cur = norm_path(c);
            let a: Vec<&str> = cur.split('/').filter(|s| !s.is_empty()).collect();
            let b: Vec<&str> = other.split('/').filter(|s| !s.is_empty()).collect();
            let common = a.iter().zip(&b).take_while(|(x, y)| same_worktree_path(x, y)).count();
            let mut parts = vec!["..".to_owned(); a.len() - common];
            parts.extend(b[common..].iter().map(|s| (*s).to_owned()));
            if parts.is_empty() { ".".to_owned() } else { parts.join("/") }
        }
    }
}

/// Pick the checkpoint the caller's code state should be compared with. Prefer the newest
/// checkpoint saved in *this* worktree on this machine, then the newest on the same branch,
/// then today's behaviour (newest overall) when checkpoints carry no worktree info. When the
/// newest checkpoint is from another worktree, there is no drift to warn about: return a calm
/// note describing it instead. `drift()` stays a pure comparison of two states.
fn select_drift_checkpoint<'a>(pool: &'a [Checkpoint], current: &GitState) -> (Option<&'a Checkpoint>, Option<String>) {
    let Some(wt) = current.worktree.as_deref().map(norm_path) else {
        return (pool.first(), None);
    };
    let same_machine = |g: &GitState| match (&g.host, &current.host) {
        (Some(a), Some(b)) => same_worktree_path(a, b),
        _ => true, // no host recorded: assume the same machine
    };
    if let Some(cp) = pool.iter().find(|c| {
        c.git.as_ref().is_some_and(|g| {
            same_machine(g) && g.worktree.as_deref().is_some_and(|w| same_worktree_path(&norm_path(w), &wt))
        })
    }) {
        return (Some(cp), None);
    }
    if let Some(cp) = pool.iter().find(|c| {
        c.git
            .as_ref()
            .and_then(|g| g.branch.as_deref())
            .zip(current.branch.as_deref())
            .is_some_and(|(a, b)| same_worktree_path(a, b))
    }) {
        return (Some(cp), None);
    }
    if let Some(g) = pool.first().and_then(|c| c.git.as_ref())
        && let Some(other) = g.worktree.as_deref().map(norm_path)
        && !same_worktree_path(&other, &wt)
    {
        let label = display_worktree(&other, Some(&wt));
        let branch = g.branch.as_deref().unwrap_or("(detached)");
        return (None, Some(format!("_Latest checkpoint was saved in another worktree ({label}, branch {branch})._")));
    }
    (pool.first(), None)
}

/// Recent file edits split into this worktree (paths already relative to the hook's cwd) and
/// other worktrees (a friendly label plus the number of distinct files). Edits with no recorded
/// cwd count as this worktree's, so the output is unchanged when nothing records it.
fn files_by_worktree<'a>(events: &'a [Event], worktree: &str) -> (Vec<&'a str>, Vec<(String, usize)>) {
    let current = norm_path(worktree);
    let mut mine: Vec<&str> = vec![];
    let mut elsewhere: Vec<(&'a Event, String)> = vec![];
    for e in events.iter().filter(|e| e.kind == FILE_CHANGE) {
        let from_here = event_str(e, "cwd").map(norm_path).is_none_or(|cwd| under_worktree(&cwd, &current));
        if from_here {
            if !mine.contains(&e.content.as_str()) && mine.len() < 20 {
                mine.push(&e.content);
            }
        } else {
            elsewhere.push((e, norm_path(event_str(e, "cwd").unwrap_or_default())));
        }
    }
    // A hook may fire in a subdirectory; group those edits under the shallowest cwd
    // seen for that worktree so one checkout is never split into several entries.
    let mut roots: Vec<String> = vec![];
    for (_, cwd) in &elsewhere {
        if roots.iter().any(|r| same_worktree_path(r, cwd) || under_worktree(cwd, r)) {
            continue;
        }
        roots.retain(|r| !under_worktree(r, cwd));
        roots.push(cwd.clone());
    }
    let others = roots
        .into_iter()
        .map(|root| {
            let count = elsewhere
                .iter()
                .filter(|(_, cwd)| under_worktree(cwd, &root))
                .map(|(e, _)| e.content.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len();
            (display_worktree(&root, Some(&current)), count)
        })
        .collect();
    (mine, others)
}

/// Every `delegated_session` event of the project, newest first.
async fn delegated_starts(store: &Store, project: &str) -> Result<Vec<Event>> {
    store.events_recent_kind(project, None, DELEGATED_SESSION, 500).await
}

/// Names of every task ever delegated in the project (so a coordinator can tell workers' checkpoints from its own).
pub async fn delegated_task_names(store: &Store, project: &str) -> Result<HashSet<String>> {
    Ok(delegated_starts(store, project).await?.iter().filter_map(|e| event_str(e, "task").map(String::from)).collect())
}

/// The coordinator's view: recent delegated tasks (newest first) with status, checkpoint and last words.
async fn delegated_work(store: &Store, project: &str, starts: &[Event]) -> Result<Vec<DelegatedWork>> {
    let since = now_ms() - DELEGATED_WINDOW_MS;
    let mut runs = vec![];
    for run in store.run_list(Some(project), None, 50).await? {
        if runs.len() >= DELEGATED_MAX_RUNS {
            break;
        }
        if is_delegated_input(run.input.as_ref()) && run.updated_at >= since {
            runs.push(store.run_get(&run.id).await?);
        }
    }
    let recent: Vec<&Event> =
        starts.iter().filter(|e| e.created_at >= since && event_str(e, "task").is_some()).collect();
    if recent.is_empty() && runs.is_empty() {
        return Ok(vec![]);
    }
    let ended: HashSet<String> = store
        .events_recent_kind(project, None, SESSION_END, 500)
        .await?
        .into_iter()
        .filter_map(|e| e.session)
        .collect();
    let messages = store.events_recent_kind(project, None, AGENT_MESSAGE, 500).await?;
    let last_message = |task: &str| {
        let sessions = task_sessions(starts, Some(task));
        messages
            .iter()
            .find(|m| m.session.as_deref().is_some_and(|s| sessions.contains(s)))
            .map(|m| truncate(&m.content, DELEGATED_MESSAGE_CHARS))
    };

    let mut out: Vec<DelegatedWork> = vec![];
    // Tasks of delegated runs: their step says where they stand, even before any worker started.
    for rw in runs {
        for step in rw.steps.into_iter().take(DELEGATED_MAX_STEPS) {
            let latest = starts.iter().find(|e| event_str(e, "task") == Some(step.key.as_str()));
            out.push(DelegatedWork {
                task: step.key.clone(),
                parent: latest.and_then(|e| event_str(e, "parent")).map(String::from),
                harness: latest.and_then(|e| event_str(e, "harness").or(e.source.as_deref())).map(String::from),
                active: step.status == StepStatus::Running && !step.stale,
                started_at: step.started_at,
                checkpoint: store.checkpoints_for_task(project, &step.key, 1).await?.into_iter().next(),
                last_message: last_message(&step.key),
                run_id: Some(rw.run.id.clone()),
                run_name: Some(rw.run.name.clone()),
                step: Some(step),
            });
        }
    }
    let in_runs = out.len();
    for e in recent {
        let task = event_str(e, "task").unwrap_or_default();
        if out.iter().any(|d| d.task == task) {
            continue;
        }
        if out.len() - in_runs >= DELEGATED_MAX {
            break;
        }
        out.push(DelegatedWork {
            task: task.to_owned(),
            parent: event_str(e, "parent").map(String::from),
            harness: event_str(e, "harness").or(e.source.as_deref()).map(String::from),
            active: e.session.as_deref().is_none_or(|s| !ended.contains(s)),
            started_at: e.created_at,
            checkpoint: store.checkpoints_for_task(project, task, 1).await?.into_iter().next(),
            last_message: last_message(task),
            run_id: None,
            run_name: None,
            step: None,
        });
    }
    Ok(out)
}

/// Sessions that were delegated `task` (or any task, for `None`).
fn task_sessions<'a>(starts: &'a [Event], task: Option<&str>) -> HashSet<&'a str> {
    starts
        .iter()
        .filter(|s| task.is_none_or(|t| event_str(s, "task") == Some(t)))
        .filter_map(|s| s.session.as_deref())
        .collect()
}

/// Events of `kind` that count as the previous session's last words: for a worker, those of
/// its task's sessions (whatever the harness); for the coordinator, any that did not come
/// from a delegated worker session.
async fn last_words_candidates(
    store: &Store,
    project: &str,
    task: Option<&str>,
    starts: &[Event],
    recent: &[Event],
    kind: &str,
) -> Result<Vec<Event>> {
    Ok(match task {
        Some(t) => {
            let mine = task_sessions(starts, Some(t));
            store
                .events_recent_kind(project, None, kind, 500)
                .await?
                .into_iter()
                .filter(|e| e.session.as_deref().is_some_and(|s| mine.contains(s)))
                .collect()
        }
        None => {
            let workers = task_sessions(starts, None);
            recent
                .iter()
                .filter(|e| e.kind == kind && !e.session.as_deref().is_some_and(|s| workers.contains(s)))
                .cloned()
                .collect()
        }
    })
}

/// `user` is the caller (on a multi-user server): it decides whose global
/// memories are included.
/// `current_git` is the caller's code state, compared against the latest
/// checkpoint's to warn about a different branch, commit or machine.
/// `task` (a worker's `MEMDUSA_TASK`) makes the brief about that delegated task alone; without it the
/// brief is the coordinator's and lists the delegated work.
pub async fn build(
    store: &Store,
    project: &str,
    user: Option<&str>,
    current_git: Option<&GitState>,
    task: Option<&str>,
) -> Result<Brief> {
    build_with_budget(store, project, user, current_git, task, MEMORY_BUDGET).await
}

/// [`build`] with an explicit character budget for the Memory section.
pub async fn build_with_budget(
    store: &Store,
    project: &str,
    user: Option<&str>,
    current_git: Option<&GitState>,
    task: Option<&str>,
    memory_budget: usize,
) -> Result<Brief> {
    let starts = delegated_starts(store, project).await?;
    let delegated = if task.is_none() { delegated_work(store, project, &starts).await? } else { vec![] };
    let checkpoints = match task {
        // A worker's brief is its task: other workstreams would only compete for "where things stand".
        Some(t) => store.checkpoints_for_task(project, t, TASK_CHECKPOINTS).await?,
        None => {
            // Delegated tasks have their own section; keep them from crowding out the coordinator's checkpoints.
            let mut cps = store.checkpoints_latest(project, 5 + delegated.len() as i64).await?;
            cps.retain(|c| c.task.as_deref().is_none_or(|t| !delegated.iter().any(|d| d.task == t)));
            cps.truncate(5);
            cps
        }
    };
    let mut active_runs = vec![];
    for run in store.run_list(Some(project), Some(RunStatus::Running), 10).await? {
        // delegated runs are the coordinator's "Delegated work", not a run to take over
        if !is_delegated_input(run.input.as_ref()) {
            active_runs.push(store.run_get(&run.id).await?);
        }
    }
    let mut recent_events = store.events_recent(project, None, 100).await?;
    if let Some(t) = task {
        // A worker's activity is its task's: other workers' prompts and edits are noise (git events have no session).
        let mine = task_sessions(&starts, Some(t));
        recent_events.retain(|e| e.session.as_deref().is_none_or(|s| mine.contains(s)));
    }
    let focus = focus_text(store, project, checkpoints.first()).await?;
    let (memories, omitted) =
        select_memories(store, project, user, focus.as_deref(), memory_budget, &recent_events).await?;

    // Drift is compared with the checkpoint from the caller's own worktree when there is one.
    // Task-scoped worker briefs still select from their task alone; older checkpoints of that
    // task are kept so a worker can match the worktree it is resuming in.
    let drift_pool = match current_git {
        Some(c) if c.worktree.is_some() => Some(match task {
            Some(t) => store.checkpoints_for_task(project, t, 100).await?,
            None => store.checkpoints_recent(project, 100).await?,
        }),
        _ => None,
    };
    let (drift_cp, other_worktree_note) = match &drift_pool {
        Some(pool) => match current_git {
            Some(c) => select_drift_checkpoint(pool, c),
            None => (checkpoints.first(), None),
        },
        None => (checkpoints.first(), None),
    };
    let git_warnings = match (drift_cp.and_then(|c| c.git.as_ref()), current_git) {
        (Some(saved), Some(current)) => saved.drift(current),
        _ => vec![],
    };
    // A checkpoint the agent wrote already says where things stand: only show what came after it.
    // Automatic fallback checkpoints are written at session end, after the last words, so they don't count.
    let since = checkpoints
        .iter()
        .find(|c| !c.source.as_deref().is_some_and(|s| s.ends_with(" (auto)")))
        .map_or(0, |c| c.created_at);
    let latest = |events: Vec<Event>| events.into_iter().filter(|e| e.created_at > since).max_by_key(|e| e.created_at);
    let message = latest(last_words_candidates(store, project, task, &starts, &recent_events, AGENT_MESSAGE).await?);
    let summary = latest(last_words_candidates(store, project, task, &starts, &recent_events, COMPACT_SUMMARY).await?);

    let markdown = render(&Parts {
        project,
        task,
        checkpoints: &checkpoints,
        runs: &active_runs,
        memories: &memories,
        omitted,
        events: &recent_events,
        git_warnings: &git_warnings,
        other_worktree_note: other_worktree_note.as_deref(),
        current_worktree: current_git.and_then(|g| g.worktree.as_deref()),
        delegated: &delegated,
        message: message.as_ref(),
        summary: summary.as_ref(),
    });
    Ok(Brief {
        project: project.into(),
        checkpoints,
        active_runs,
        memories,
        recent_events,
        delegated,
        git_warnings,
        other_worktree_note,
        markdown,
    })
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
        // a single memory larger than the budget is still shown, so the section is never empty
        if m.kind != MemoryKind::Preference && used + cost > budget && !shown.is_empty() {
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

/// What `delegate` tells the coordinator: the run and, per task, the environment to launch its worker with.
pub fn delegate_report(rw: &RunWithSteps) -> String {
    let mut o = format!("Delegated run `{}` ({}): {} task(s).\n", rw.run.id, rw.run.name, rw.steps.len());
    let _ = writeln!(
        o,
        "Launch one worker per task with its environment (any harness; `MEMDUSA_PARENT=<you>` is optional):\n"
    );
    for s in &rw.steps {
        let state = match s.status {
            StepStatus::Pending => String::new(),
            _ => format!("   # {}", step_status(s)),
        };
        let _ = writeln!(o, "- {}{state}", worker_env_line(&rw.run.id, &s.key));
    }
    let _ = writeln!(
        o,
        "\nA worker claims its task when its session starts and renews the lease while it works; a crashed or \
         rate-limited worker's task turns stale and a replacement launched with the same line takes it over. Workers \
         finish with `task_complete`. `resume` shows every task's status."
    );
    o
}

/// The notice at the top of a worker's SessionStart brief about its claim on its step of a delegated run.
pub fn claim_notice(task: &str, claim: &ClaimResult) -> String {
    match claim {
        ClaimResult::Claimed { previous_owner, stale, previous_error, .. } => {
            let mut s = format!("**You claimed the delegated task `{task}`.** ");
            match (previous_owner, stale) {
                (Some(p), true) => s.push_str(&format!(
                    "The previous attempt by {p} went stale (its lease expired), so you took it over: check the \
                     checkpoint and the files below before redoing anything. "
                )),
                (Some(p), false) => {
                    s.push_str(&format!("You took it over from {p}, whose lease was still live (forced). "))
                }
                _ => {}
            }
            if let Some(e) = previous_error {
                s.push_str(&format!("An earlier attempt failed: {e}. "));
            }
            s.push_str(
                "Your lease renews while you work. When the task is finished call the memdusa `task_complete` tool \
                 (or `memdusa task done`) with a short summary; if you can't finish it, call `task_fail` with the reason.",
            );
            s
        }
        ClaimResult::AlreadyDone { output } => {
            let result = output
                .as_ref()
                .map(|o| o.as_str().map(String::from).unwrap_or_else(|| o.to_string()))
                .map(|t| {
                    format!(" Recorded result: {}", truncate(&t.split_whitespace().collect::<Vec<_>>().join(" "), 500))
                })
                .unwrap_or_default();
            format!(
                "**The delegated task `{task}` is already done.**{result}\nDo NOT redo it: stop here, or confirm with the \
                 user what they want before doing any more work on it."
            )
        }
        ClaimResult::HeldBy { owner, lease_until } => {
            let secs = (lease_until - now_ms()).max(0) / 1000;
            format!(
                "**The delegated task `{task}` is held by another live worker: {owner}** (lease renewed for another \
                 {}m). Do NOT duplicate its work: coordinate with the user or the coordinator first. If that worker is gone \
                 its lease expires, the task shows as stale, and a new session on it takes over.",
                (secs + 59) / 60
            )
        }
    }
}

/// Where a delegated task's step stands, for the coordinator.
fn step_status(s: &Step) -> String {
    let owner = s.owner.as_deref().unwrap_or("?");
    match s.status {
        StepStatus::Pending => "pending".into(),
        StepStatus::Running if s.stale => {
            let expired = s.lease_until.map(|t| format!(", lease expired {}", ago(t))).unwrap_or_default();
            format!("stale (owner {owner}{expired})")
        }
        StepStatus::Running => format!("running by {owner}"),
        StepStatus::Completed => "done".into(),
        StepStatus::Failed => format!("failed: {}", s.error.as_deref().unwrap_or("no reason given")),
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

/// Everything [`render`] shows.
struct Parts<'a> {
    project: &'a str,
    task: Option<&'a str>,
    checkpoints: &'a [Checkpoint],
    runs: &'a [RunWithSteps],
    memories: &'a [Memory],
    omitted: usize,
    events: &'a [Event],
    git_warnings: &'a [String],
    other_worktree_note: Option<&'a str>,
    /// The caller's worktree root, when known: files are grouped by it.
    current_worktree: Option<&'a str>,
    delegated: &'a [DelegatedWork],
    message: Option<&'a Event>,
    summary: Option<&'a Event>,
}

fn render(parts: &Parts) -> String {
    let Parts {
        project,
        task,
        checkpoints,
        runs,
        memories,
        omitted,
        events,
        git_warnings,
        other_worktree_note,
        current_worktree,
        delegated,
        message,
        summary,
    } = *parts;
    let mut o = String::new();
    let _ = writeln!(o, "# Resume brief: {project}\n");
    if let Some(t) = task {
        let _ = writeln!(
            o,
            "_You are a worker on the delegated task **{t}**: this brief covers that task only (its checkpoints and \
             the last words of its previous workers)._\n"
        );
    }
    if !git_warnings.is_empty() {
        let _ = writeln!(o, "## ⚠ Code state differs from the last checkpoint");
        for w in git_warnings {
            let _ = writeln!(o, "- {w}");
        }
        let _ = writeln!(o, "\nVerify the actual state of the code before trusting the notes below.\n");
    }
    if let Some(note) = other_worktree_note {
        let _ = writeln!(o, "{note}\n");
    }

    if checkpoints.is_empty() && runs.is_empty() && memories.is_empty() && events.is_empty() && delegated.is_empty() {
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

    if !delegated.is_empty() {
        let _ = writeln!(o, "## Delegated work");
        // Tasks of delegated runs come first (grouped by run); the run's name is a sub-heading when there are several.
        let mut run_ids: Vec<&str> = vec![];
        for id in delegated.iter().filter_map(|d| d.run_id.as_deref()) {
            if !run_ids.contains(&id) {
                run_ids.push(id);
            }
        }
        let headings = run_ids.len() > 1;
        let mut heading: Option<Option<&str>> = None;
        for d in delegated {
            if headings && heading != Some(d.run_id.as_deref()) {
                heading = Some(d.run_id.as_deref());
                let title = d.run_name.as_deref().unwrap_or("Tasks without a run");
                let _ = writeln!(o, "\n### {title}");
            }
            let parent = d.parent.as_deref().map(|p| format!(" (for {p})")).unwrap_or_default();
            if let Some(step) = &d.step {
                let _ = writeln!(o, "- **{}**{parent}: {}", d.task, step_status(step));
                if step.status == StepStatus::Completed
                    && let Some(out) = step.output.as_ref()
                {
                    let text = out.as_str().map(String::from).unwrap_or_else(|| out.to_string());
                    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    let _ = writeln!(o, "  - result: {}", truncate(&text, DELEGATED_OUTPUT_CHARS));
                }
            } else {
                let status = if d.active { "active" } else { "ended" };
                let who = d.harness.as_deref().map(|h| format!(" by {h}")).unwrap_or_default();
                let _ = writeln!(o, "- **{}**{who}{parent}: {status}, started {}", d.task, ago(d.started_at));
            }
            if let Some(c) = &d.checkpoint {
                let _ = writeln!(o, "  - goal: {} (checkpoint {})", c.goal, ago(c.created_at));
                if !c.next.is_empty() {
                    let next: Vec<&str> = c.next.iter().take(DELEGATED_NEXT).map(String::as_str).collect();
                    let more = c.next.len().saturating_sub(DELEGATED_NEXT);
                    let more = if more > 0 { format!(" (+{more} more)") } else { String::new() };
                    let _ = writeln!(o, "  - next: {}{more}", next.join("; "));
                }
            }
            if let Some(m) = &d.last_message {
                let _ = writeln!(o, "  - last message: {}", m.split_whitespace().collect::<Vec<_>>().join(" "));
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
        // brief stays readable. With a worktree, show this one's files first and
        // summarize the other workers' checkouts instead of mixing them in.
        let (files, other_worktrees) = match current_worktree {
            Some(wt) => files_by_worktree(events, wt),
            None => {
                let mut files: Vec<&str> = vec![];
                for e in events.iter().filter(|e| e.kind == FILE_CHANGE) {
                    if !files.contains(&e.content.as_str()) && files.len() < 20 {
                        files.push(&e.content);
                    }
                }
                (files, vec![])
            }
        };
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
        if !other_worktrees.is_empty() {
            let summary: Vec<String> =
                other_worktrees.iter().map(|(label, n)| format!("{label} ({n} files)")).collect();
            let _ = writeln!(o, "Other worktrees: {}\n", summary.join(", "));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cp(worktree: Option<&str>, branch: &str, host: &str, at: Millis) -> Checkpoint {
        Checkpoint {
            id: new_id(),
            project: "p".into(),
            task: None,
            goal: format!("goal {at}"),
            done: vec![],
            next: vec![],
            blockers: vec![],
            files: vec![],
            notes: None,
            source: None,
            git: Some(GitState {
                branch: Some(branch.into()),
                head: Some("a1b2c3d4e5".into()),
                host: Some(host.into()),
                worktree: worktree.map(String::from),
                ..Default::default()
            }),
            created_at: at,
        }
    }

    fn caller() -> GitState {
        GitState {
            branch: Some("feat/b".into()),
            head: Some("fffffff0000".into()),
            host: Some("mbp".into()),
            worktree: Some("/work/wtb".into()),
            ..Default::default()
        }
    }

    #[test]
    fn drift_checkpoint_prefers_same_worktree_then_branch() {
        let current = caller();
        let other = cp(Some("/work/wta"), "feat/a", "mbp", 300);
        let same_wt = cp(Some("/work/wtb"), "feat/old", "mbp", 200);
        let same_branch = cp(Some("/work/wtc"), "feat/b", "mbp", 100);

        let pool = [other.clone(), same_wt.clone(), same_branch.clone()];
        let (chosen, note) = select_drift_checkpoint(&pool, &current);
        assert_eq!(chosen.map(|c| &c.id), Some(&same_wt.id), "same worktree wins even if older");
        assert!(note.is_none());

        let pool = [other, same_branch.clone()];
        let (chosen, note) = select_drift_checkpoint(&pool, &current);
        assert_eq!(chosen.map(|c| &c.id), Some(&same_branch.id), "then the same branch");
        assert!(note.is_none());

        // A different machine's copy of the same path is not our worktree: fall through to branch.
        let remote = cp(Some("/work/wtb"), "feat/old", "desktop", 300);
        let pool = [remote, same_branch.clone()];
        let (chosen, _) = select_drift_checkpoint(&pool, &current);
        assert_eq!(chosen.map(|c| &c.id), Some(&same_branch.id), "different machine is not the same worktree");
    }

    #[test]
    fn drift_checkpoint_notes_other_worktree_and_keeps_legacy_behaviour() {
        let current = GitState { worktree: Some("C:\\work\\wtb".into()), ..caller() };
        let other = cp(Some("C:/work/wta"), "feat/eval", "mbp", 300);
        let pool = [other];
        let (chosen, note) = select_drift_checkpoint(&pool, &current);
        assert!(chosen.is_none(), "no warning against another worker's branch");
        assert_eq!(
            note.as_deref(),
            Some("_Latest checkpoint was saved in another worktree (../wta, branch feat/eval)._"),
            "{note:?}"
        );

        let legacy = cp(None, "feat/a", "mbp", 300);
        let pool = [legacy.clone()];
        let (chosen, note) = select_drift_checkpoint(&pool, &current);
        assert_eq!(chosen.map(|c| &c.id), Some(&legacy.id), "no worktree info: today's behaviour");
        assert!(note.is_none());

        let no_wt_caller = GitState { worktree: None, ..current };
        let pool = [legacy.clone()];
        let (chosen, note) = select_drift_checkpoint(&pool, &no_wt_caller);
        assert_eq!(chosen.map(|c| &c.id), Some(&legacy.id));
        assert!(note.is_none());
    }

    #[test]
    fn files_are_grouped_by_worktree() {
        let event = |content: &str, cwd: Option<&str>| Event {
            id: new_id(),
            project: "p".into(),
            session: None,
            source: None,
            kind: FILE_CHANGE.into(),
            content: content.into(),
            data: cwd.map(|c| serde_json::json!({ "cwd": c })),
            created_at: 1,
        };
        let events = vec![
            event("b.rs", Some("C:\\work\\wtb")),
            event("a.rs", Some("C:/work/wta")),
            event("a2.rs", Some("C:\\work\\wta\\sub")),
            event("legacy.rs", None),
        ];
        let (mine, others) = files_by_worktree(&events, "C:/work/wtb");
        assert_eq!(mine, vec!["b.rs", "legacy.rs"], "no-cwd edits stay in this worktree");
        assert_eq!(others, vec![("../wta".to_owned(), 2)], "paths shown relative to the caller");
        assert_eq!(display_worktree("/work/wta", Some("/work/wtb")), "../wta");
        assert_eq!(display_worktree("/work/wtb", Some("/work/wtb")), ".");
    }
}
