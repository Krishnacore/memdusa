//! One-shot local ⇄ hosted sync (H3a). The local SQLite database is the
//! queue: every opted-in project keeps cursors in `sync_state`, pushes local
//! changes first, then pulls remote ones with the last-writer-wins rules from
//! `memdusa-core::sync`. Background `--watch` (H3b) is deliberately absent.

use std::time::Duration;

use anyhow::Context;
use clap::Subcommand;
use memdusa_core::Store;
use memdusa_core::model::now_ms;
use memdusa_core::sync::{SYNC_BATCH, SYNC_GLOBALS, SyncActor, SyncRecord, SyncState};

use crate::config::Remote;
use crate::remote::Client;

#[derive(Debug, Clone, Subcommand)]
pub enum SyncCmd {
    /// Opt into syncing a project (or all readable projects / your globals).
    /// The first sync does a full push and pull of everything already there.
    Add {
        project: Option<String>,
        /// Every project you can read on the server.
        #[arg(long, conflicts_with = "project")]
        all: bool,
        /// Your private global memories (opt-in, separate from projects).
        #[arg(long, conflicts_with = "project")]
        globals: bool,
    },
    /// Stop syncing a project (local data is kept).
    Remove {
        project: Option<String>,
        #[arg(long, conflicts_with = "project")]
        globals: bool,
    },
    /// List the projects opted into sync for the logged-in server.
    List,
    /// Per-project cursors, pending changes, last error and recent conflicts.
    Status,
}

pub async fn run(cmd: Option<SyncCmd>, local: &Store, remote: &Remote, origin: &str) -> anyhow::Result<()> {
    match cmd {
        Some(SyncCmd::Add { project, all, globals }) => add(local, remote, origin, project, all, globals).await,
        Some(SyncCmd::Remove { project, globals }) => {
            let project = target(project, globals)?;
            local.sync_state_remove(&remote.url, &project).await?;
            println!("stopped syncing {project} (local data kept)");
            Ok(())
        }
        Some(SyncCmd::List) => {
            let states = local.sync_state_list(&remote.url).await?;
            if states.is_empty() {
                println!("no projects opted in; run `memdusa sync add <project>`");
            }
            for s in states {
                println!(
                    "{:<36} pushed {}  pulled {}  last ok {}",
                    display_project(&s.project),
                    s.push_after,
                    s.pull_after,
                    s.last_ok_at.map_or_else(|| "never".into(), ago)
                );
            }
            Ok(())
        }
        Some(SyncCmd::Status) => status(local, remote).await,
        None => one_shot(local, remote, origin).await,
    }
}

fn target(project: Option<String>, globals: bool) -> anyhow::Result<String> {
    if globals { Ok(SYNC_GLOBALS.into()) } else { project.context("pass a project key, or --globals") }
}

fn display_project(project: &str) -> &str {
    if project == SYNC_GLOBALS { "globals" } else { project }
}

fn ago(ms: i64) -> String {
    let secs = (now_ms() - ms).max(0) / 1000;
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

async fn add(
    local: &Store,
    remote: &Remote,
    origin: &str,
    project: Option<String>,
    all: bool,
    globals: bool,
) -> anyhow::Result<()> {
    let client = Client::new(remote.clone(), Duration::from_secs(60))?;
    let mut projects = vec![];
    if all {
        projects.extend(client.sync_projects().await.context("listing readable projects")?);
    }
    if globals {
        projects.push(SYNC_GLOBALS.into());
    }
    if let Some(p) = project {
        projects.push(p);
    }
    anyhow::ensure!(!projects.is_empty(), "pass a project, --all or --globals");

    for project in projects {
        let state = local.sync_state_add(&remote.url, &project, origin).await?;
        let seeded = local.sync_seed(if project == SYNC_GLOBALS { None } else { Some(&project) }).await?;
        println!("syncing {} ({} existing record(s) queued)", display_project(&project), seeded);
        // Opt-in must survive an offline server: the cursors stay at zero and
        // the next `memdusa sync` retries the initial full sync.
        let key = project.clone();
        match sync_one(local, &client, state, origin).await {
            Ok((pushed, pulled)) => println!("  initial sync: pushed {pushed}, pulled {pulled}"),
            Err(e) => {
                println!("  warning: initial sync failed ({e}); run `memdusa sync` to retry");
                let _ = local.sync_state_update(&remote.url, &key, None, None, Some(&format!("{e:#}")), false).await;
            }
        }
    }
    Ok(())
}

async fn status(local: &Store, remote: &Remote) -> anyhow::Result<()> {
    let states = local.sync_state_list(&remote.url).await?;
    if states.is_empty() {
        println!("no projects opted in; run `memdusa sync add <project>`");
        return Ok(());
    }
    for s in states {
        let pending =
            local.sync_pending(if s.project == SYNC_GLOBALS { None } else { Some(&s.project) }, s.push_after).await?;
        let conflicts =
            local.sync_conflicts(if s.project == SYNC_GLOBALS { None } else { Some(&s.project) }, 5).await?;
        println!("{}", display_project(&s.project));
        println!("  remote: {}", remote.url);
        println!("  last ok: {}", s.last_ok_at.map_or_else(|| "never".into(), ago));
        println!("  pending local changes: {pending}");
        println!("  push cursor {} / pull cursor {}", s.push_after, s.pull_after);
        match &s.last_error {
            Some(e) => println!("  last error: {e}"),
            None => println!("  last error: none"),
        }
        if conflicts.is_empty() {
            println!("  recent conflicts: none");
        } else {
            println!("  recent conflicts:");
            for m in conflicts {
                let text: String = m.content.chars().take(70).collect();
                println!("    {} — {}", m.id, text);
            }
        }
    }
    Ok(())
}

async fn one_shot(local: &Store, remote: &Remote, origin: &str) -> anyhow::Result<()> {
    let states = local.sync_state_list(&remote.url).await?;
    anyhow::ensure!(!states.is_empty(), "no projects opted in for {}; run `memdusa sync add <project>`", remote.url);
    let client = Client::new(remote.clone(), Duration::from_secs(60))?;
    let mut pushed = 0;
    let mut pulled = 0;
    let mut failed = 0;
    for state in states {
        let name = display_project(&state.project).to_owned();
        let key = state.project.clone();
        match sync_one(local, &client, state, origin).await {
            Ok((p, q)) => {
                pushed += p;
                pulled += q;
                println!("{name}: pushed {p}, pulled {q}");
            }
            Err(e) => {
                failed += 1;
                if let Err(se) =
                    local.sync_state_update(&remote.url, &key, None, None, Some(&format!("{e:#}")), false).await
                {
                    eprintln!("{name}: could not record sync error: {se:#}");
                }
                eprintln!("{name}: {e:#}");
            }
        }
    }
    println!("done: pushed {pushed}, pulled {pulled}, {failed} project(s) failed");
    if failed > 0 {
        anyhow::bail!("{failed} project(s) failed to sync");
    }
    Ok(())
}

/// Push local changes after the push cursor, then pull after the pull cursor.
/// Cursors are persisted after each successful page so a failure never loses
/// progress; the next run resumes exactly where this one stopped.
async fn sync_one(
    local: &Store,
    client: &Client,
    mut state: SyncState,
    origin: &str,
) -> anyhow::Result<(usize, usize)> {
    let project = state.project.as_str();
    let mut pushed = 0;
    let mut pulled = 0;

    loop {
        let (mut changes, last_seq) = local.sync_local_changes(Some(project), state.push_after, SYNC_BATCH).await?;
        if last_seq == state.push_after {
            break;
        }
        // Mark every pushed change with this client's id: the server stores it
        // and the pusher skips its own echo when it pulls the changes back.
        for c in &mut changes {
            c.origin = Some(origin.to_owned());
        }
        if !changes.is_empty() {
            client.sync_push(project, &changes).await?;
            pushed += changes.len();
        }
        state.push_after = last_seq;
        local.sync_state_update(&state.remote, project, Some(last_seq), None, None, false).await?;
        if (changes.len() as i64) < SYNC_BATCH {
            break;
        }
    }

    loop {
        let page = client.sync_pull(project, state.pull_after, SYNC_BATCH).await?;
        let incoming: Vec<SyncRecord> =
            page.changes.into_iter().filter(|c| c.origin.as_deref() != Some(origin)).collect();
        if !incoming.is_empty() {
            local.sync_apply(Some(project), &incoming, &SyncActor::Local, None).await?;
            pulled += incoming.len();
        }
        // Applying a pulled page appends local changes (needed for delete
        // tombstones). Advance the push cursor past them: they came from the
        // server and must not be echoed back as new local edits.
        let local_seq = local.sync_last_seq().await?;
        let pushed_to = local_seq.max(state.push_after);
        state.push_after = pushed_to;
        state.pull_after = page.next_cursor;
        local.sync_state_update(&state.remote, project, Some(pushed_to), Some(page.next_cursor), None, false).await?;
        if !page.has_more {
            break;
        }
    }

    local.sync_state_update(&state.remote, project, None, None, None, true).await?;
    Ok((pushed, pulled))
}
