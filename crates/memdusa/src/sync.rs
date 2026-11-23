//! Local ⇄ hosted sync. The local SQLite database is the queue: every
//! opted-in project keeps cursors in `sync_state`, pushes local changes first,
//! then pulls remote ones with the last-writer-wins rules from
//! `memdusa-core::sync`.
//!
//! H3a is the one-shot `memdusa sync`. H3b adds sync mode (`login --sync`),
//! the background `--watch` loop, opportunistic sync after hooks, and the
//! service helpers. Sync mode itself is wired in `main.rs`/`hooks.rs`/`mcp.rs`:
//! this module owns the protocol and the watcher.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use clap::Subcommand;
use memdusa_core::Store;
use memdusa_core::model::now_ms;
use memdusa_core::sync::{SYNC_BATCH, SYNC_GLOBALS, SyncActor, SyncRecord, SyncState};

use crate::config::Remote;
use crate::hooks::HookEvent;
use crate::remote::Client;
use crate::{service, watch};

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
    /// Turn sync mode on or off for this login.
    Mode {
        /// `on` or `off`.
        value: String,
    },
    /// Install or remove a service that runs `memdusa sync --watch`.
    Service {
        #[command(subcommand)]
        cmd: service::ServiceCmd,
    },
}

#[derive(Debug, Clone)]
pub struct Options {
    pub cmd: Option<SyncCmd>,
    pub watch: bool,
    pub quiet: bool,
    pub interval: String,
}

pub async fn run(opts: Options, local: &Store, remote: &Remote, origin: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!(opts.watch && opts.quiet), "--watch and --quiet are mutually exclusive");
    anyhow::ensure!(
        !((opts.watch || opts.quiet) && opts.cmd.is_some()),
        "--watch / --quiet cannot be combined with a sync subcommand"
    );
    if opts.watch {
        let interval = if opts.interval.trim().is_empty() {
            watch::DEFAULT_INTERVAL
        } else {
            watch::parse_interval(&opts.interval)?
        };
        return watch_loop(local, remote, origin, interval).await;
    }
    if opts.quiet {
        if !watch::should_run_quiet()? {
            return Ok(());
        }
        let (pushed, pulled) = sync_all_quiet(local, remote, origin).await?;
        watch::note_sync()?;
        if pushed + pulled > 0 {
            println!("sync: pushed {pushed}, pulled {pulled}");
        }
        return Ok(());
    }
    match opts.cmd {
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
        Some(SyncCmd::Mode { value }) => {
            let on = match value.as_str() {
                "on" => true,
                "off" => false,
                other => anyhow::bail!("sync mode expects `on` or `off`, got {other:?}"),
            };
            let url = crate::config::set_sync_mode(on)?;
            println!("sync mode {value} for {url}");
            Ok(())
        }
        Some(SyncCmd::Service { cmd }) => service::run(cmd),
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
            Ok((pushed, pulled)) => {
                println!("  initial sync: pushed {pushed}, pulled {pulled}");
                let _ = watch::note_sync();
            }
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
    } else {
        for s in states {
            let pending = local
                .sync_pending(if s.project == SYNC_GLOBALS { None } else { Some(&s.project) }, s.push_after)
                .await?;
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
    }
    match watch::WatchLock::pid()? {
        Some(pid) => println!("watcher: running (pid {pid})"),
        None => println!("watcher: not running"),
    }
    println!("last sync: {}", watch::last_sync().map_or_else(|| "never".into(), ago));
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
    let _ = watch::note_sync();
    println!("done: pushed {pushed}, pulled {pulled}, {failed} project(s) failed");
    if failed > 0 {
        anyhow::bail!("{failed} project(s) failed to sync");
    }
    Ok(())
}

/// `memdusa status` in sync mode: both sides at a glance.
pub async fn status_report(store: &Store, local_url: &str, remote: &Remote) -> anyhow::Result<()> {
    println!("mode: sync (local database {local_url} → {})", remote.url);
    match Client::new(remote.clone(), Duration::from_secs(5))?.whoami().await {
        Ok(info) => println!("server: reachable as {}", describe(&info)),
        Err(e) => println!("server: NOT reachable ({e})"),
    }
    let states = store.sync_state_list(&remote.url).await?;
    println!(
        "opted-in projects: {}",
        if states.is_empty() {
            "none (run `memdusa sync add <project>`)".into()
        } else {
            states.iter().map(|s| display_project(&s.project)).collect::<Vec<_>>().join(", ")
        }
    );
    let mut pending = 0;
    for s in &states {
        pending +=
            store.sync_pending(if s.project == SYNC_GLOBALS { None } else { Some(&s.project) }, s.push_after).await?;
    }
    println!("pending local changes: {pending}");
    match watch::WatchLock::pid()? {
        Some(pid) => println!("watcher: running (pid {pid})"),
        None => println!("watcher: not running"),
    }
    println!("last sync: {}", watch::last_sync().map_or_else(|| "never".into(), ago));
    Ok(())
}

fn describe(info: &serde_json::Value) -> String {
    match info["user"].as_str() {
        Some(u) => u.to_owned(),
        None => "shared token / open server".into(),
    }
}

/// Push+pull every opted-in project, no per-project reporting. Shared by the
/// watcher, `sync --quiet`, and opportunistic sync.
pub(crate) async fn sync_all_quiet(local: &Store, remote: &Remote, origin: &str) -> anyhow::Result<(usize, usize)> {
    let states = local.sync_state_list(&remote.url).await?;
    if states.is_empty() {
        return Ok((0, 0));
    }
    let client = Client::new(remote.clone(), Duration::from_secs(60))?;
    let (mut pushed, mut pulled) = (0, 0);
    for state in states {
        let name = display_project(&state.project).to_owned();
        let key = state.project.clone();
        match sync_one(local, &client, state, origin).await {
            Ok((p, q)) => {
                pushed += p;
                pulled += q;
            }
            Err(e) => {
                let _ = local.sync_state_update(&remote.url, &key, None, None, Some(&format!("{e:#}")), false).await;
                anyhow::bail!("{name}: {e:#}");
            }
        }
    }
    Ok((pushed, pulled))
}

struct Syncer<'a> {
    local: &'a Store,
    remote: &'a Remote,
    origin: &'a str,
}

impl watch::Runner for Syncer<'_> {
    async fn sync_all(&self) -> anyhow::Result<(usize, usize)> {
        let result = sync_all_quiet(self.local, self.remote, self.origin).await;
        if result.is_ok() {
            let _ = watch::note_sync();
        }
        result
    }
}

async fn watch_loop(local: &Store, remote: &Remote, origin: &str, interval: Duration) -> anyhow::Result<()> {
    let _lock = match watch::WatchLock::try_acquire()? {
        Some(lock) => lock,
        None => {
            println!("sync watcher already running (pid {}); nothing to do", watch::WatchLock::pid()?.unwrap_or(0));
            return Ok(());
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(tokio::sync::Notify::new());
    {
        let stop = stop.clone();
        let notify = notify.clone();
        #[cfg(unix)]
        tokio::spawn(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            stop.store(true, Ordering::Relaxed);
            // notify_one keeps a permit when the loop is mid-sync, not sleeping: no lost wakeup
            notify.notify_one();
        });
        #[cfg(not(unix))]
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            stop.store(true, Ordering::Relaxed);
            // notify_one keeps a permit when the loop is mid-sync, not sleeping: no lost wakeup
            notify.notify_one();
        });
    }
    println!("sync watcher on {} every {:?} (Ctrl-C or SIGTERM to stop)", remote.url, interval);
    let runner = Syncer { local, remote, origin };
    let clock = watch::RealClock::new(notify);
    watch::run_loop(&runner, &clock, interval, &remote.url, &stop).await;
    drop(_lock);
    Ok(())
}

/// First hook in a project opts it into sync, like `sync add <project>` but
/// without the blocking initial sync. `MEMDUSA_SYNC_AUTO_ADD=0` disables it.
pub async fn auto_add(local: &Store, remote_url: &str, project: &str, origin: &str) -> anyhow::Result<()> {
    if std::env::var("MEMDUSA_SYNC_AUTO_ADD").is_ok_and(|v| matches!(v.trim(), "0" | "off" | "false" | "no")) {
        return Ok(());
    }
    if local.sync_state_get(remote_url, project).await?.is_some() {
        return Ok(());
    }
    local.sync_state_add(remote_url, project, origin).await?;
    let seeded = local.sync_seed(Some(project)).await?;
    eprintln!("memdusa: auto-added {project} to sync ({seeded} existing record(s) queued)");
    Ok(())
}

/// Spawn a detached `memdusa sync --quiet` after a hook. Never blocks the
/// hook, and never races a watcher or a recent sync.
pub fn maybe_opportunistic(event: HookEvent, files_changed: bool) {
    let wanted = event == HookEvent::SessionEnd || (event == HookEvent::Stop && files_changed);
    if !wanted || !watch::should_run_quiet().unwrap_or(false) {
        return;
    }
    let bin = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("memdusa"));
    let _ = Command::new(bin)
        .args(["sync", "--quiet"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
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
