mod adapters;
mod cli_auth;
mod config;
mod embedder;
mod hermes;
mod hermes_yaml;
mod hooks;
mod http;
mod install;
#[cfg(feature = "local-embed")]
mod local_embed;
mod mcp;
mod oauth;
mod oauth_upstream;
#[cfg(test)]
mod oauth_upstream_tests;
mod ops;
mod project;
mod proxy;
mod remote;
mod self_update;
mod spool;
mod transcripts;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use memdusa_core::model::{
    self, ApiKey, AuditEvent, AuditQuery, CreatedKey, DefaultPolicy, MemoryKind, MemoryQuery, NewMemory, ProjectGrant,
    Role, User,
};
use memdusa_core::redact::Redactor;
use memdusa_core::retention::RetentionPolicy;
use memdusa_core::transfer;
use memdusa_core::{Store, brief};
use rmcp::ServiceExt;

#[derive(Parser)]
#[command(name = "memdusa", version, about = "Universal memory & durable execution for AI agents and harnesses")]
struct Cli {
    /// Use this database directly (sqlite://path/to.db or postgres://...), ignoring any `memdusa login` server.
    /// [default: the logged-in server if any, else ~/.memdusa/memdusa.db]
    #[arg(long, global = true, env = "MEMDUSA_DATABASE_URL")]
    database_url: Option<String>,

    /// Project key. Default: normalized git remote (github.com/org/repo), else git root, else cwd.
    #[arg(long, global = true, env = "MEMDUSA_PROJECT")]
    project: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the MCP server over stdio (what a local harness launches).
    Mcp {
        /// Name of the agent/harness, recorded as author and lease owner.
        #[arg(long, env = "MEMDUSA_AGENT", default_value = "agent")]
        agent: String,
    },
    /// Run the HTTP server: REST API at /v1 and MCP (streamable HTTP) at /mcp.
    Serve {
        #[arg(long, env = "MEMDUSA_ADDR", default_value = "127.0.0.1:7077")]
        addr: String,
        /// Require `Authorization: Bearer <token>` on every request.
        #[arg(long, env = "MEMDUSA_TOKEN")]
        token: Option<String>,
        /// Host header values accepted by /mcp (DNS-rebinding protection). Default: localhost only.
        #[arg(long = "allowed-host", env = "MEMDUSA_ALLOWED_HOSTS", value_delimiter = ',')]
        allowed_hosts: Vec<String>,
        /// Don't prune old data automatically (otherwise once a day, see `memdusa prune`).
        #[arg(long, env = "MEMDUSA_NO_AUTO_PRUNE")]
        no_auto_prune: bool,
        /// Requests per second per user (or IP), with bursts up to 10x. 0 = no limit.
        #[arg(long, env = "MEMDUSA_RATE_LIMIT", default_value_t = 50.0)]
        rate_limit: f64,
        /// Don't serve Prometheus metrics at /metrics.
        #[arg(long, env = "MEMDUSA_NO_METRICS")]
        no_metrics: bool,
    },
    /// Summarize old hook activity and delete superseded checkpoints and finished runs.
    Prune {
        /// Show what would be pruned without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Only this project (default: all).
        #[arg(long = "only-project")]
        only_project: Option<String>,
        #[command(flatten)]
        policy: RetentionArgs,
    },
    /// Print the hand-off brief for the current project.
    Resume {
        /// Print JSON instead of Markdown.
        #[arg(long)]
        json: bool,
    },
    /// Coordinator: delegate tasks as one durable run and print the env line to launch each worker with.
    Delegate {
        /// Name of the piece of work, e.g. "windows release".
        name: String,
        /// One short, stable name per task (becomes the worker's MEMDUSA_TASK).
        #[arg(required = true)]
        tasks: Vec<String>,
        /// Reuse the run with this idempotency key (default: the running run with the same name).
        #[arg(long)]
        key: Option<String>,
    },
    /// Worker: finish the task this agent was launched with (MEMDUSA_RUN + MEMDUSA_TASK).
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Save a memory from the command line.
    Remember {
        content: String,
        #[arg(long, default_value = "fact")]
        kind: String,
        #[arg(long)]
        global: bool,
    },
    /// Search memories.
    Recall { query: Option<String> },
    /// List memories that need consolidating: related ones to merge, possibly outdated ones, old todos.
    Review,
    /// Compute embeddings for memories that lack one for the current model (semantic search, MEMDUSA_EMBED_URL).
    Embed {
        /// Re-embed every memory, not just the missing or outdated ones.
        #[arg(long)]
        all: bool,
        /// Download the local embedding model first (once, into <MEMDUSA_HOME>/models/).
        /// Only needed with MEMDUSA_EMBED_URL=local; hooks never download automatically.
        #[arg(long)]
        download_model: bool,
        /// Only this project (default: all).
        #[arg(long = "only-project")]
        only_project: Option<String>,
    },
    /// Print the detected project key.
    Project,
    /// Use a remote memdusa server for hooks, MCP and the CLI on this machine.
    Login {
        /// Server URL, e.g. https://memdusa.example.com
        url: String,
        /// Bearer token (as set with `memdusa serve --token`).
        #[arg(long, env = "MEMDUSA_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Sign in with a browser: authorization code + PKCE on a loopback redirect.
        #[arg(long)]
        browser: bool,
        /// Sign in without a browser (SSH/headless) using a short user code.
        #[arg(long, conflicts_with = "browser")]
        device: bool,
    },
    /// Forget the remote server and go back to the local database.
    Logout,
    /// Show whether this machine uses a local database or a remote server, and check it.
    Status,
    /// Send hook events queued while the server was unreachable.
    Sync,
    /// Write all data as JSON Lines (memories, events, checkpoints, runs; not users or keys).
    Export {
        /// Only this project (default: everything).
        #[arg(long = "only-project")]
        only_project: Option<String>,
        /// Output file (default: stdout).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Load a `memdusa export` file. Idempotent: existing records are skipped.
    Import {
        /// File to read, or `-` for stdin.
        file: String,
    },
    /// Consistent online copy of the local SQLite database.
    Backup {
        /// Destination file (must not exist).
        path: PathBuf,
    },
    /// Manage users (admin). Acts on the local database, or on the logged-in server.
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// Manage API keys: your own, or (as admin, with --user) anyone's.
    Key {
        #[command(subcommand)]
        cmd: KeyCmd,
    },
    /// Grant a user access to a project key or a prefix ending in `/*` (admin).
    Grant {
        user: String,
        pattern: String,
        #[arg(long, default_value = "write")]
        role: String,
    },
    /// List project grants (admin).
    Grants {
        #[arg(long)]
        user: Option<String>,
    },
    /// Revoke a project grant by id (admin).
    RevokeGrant { id: String },
    /// List or revoke OAuth sign-in sessions (remote mode).
    Sessions {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// Show the audit log (admin).
    Audit {
        /// Events at or after this unix-millis timestamp, or `7d` / `24h` / `2w` ago.
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        actor: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: i64,
    },
    /// Handle a Claude Code hook event (JSON on stdin). Installed by `memdusa install`.
    Hook {
        event: hooks::HookEvent,
        /// Which harness is calling (decides the payload and output format).
        #[arg(long, value_enum, default_value = "claude-code")]
        harness: adapters::Harness,
        /// Agent name for attribution. Default: the harness name.
        #[arg(long, env = "MEMDUSA_AGENT")]
        agent: Option<String>,
        /// Don't journal user prompts.
        #[arg(long, env = "MEMDUSA_NO_PROMPTS")]
        no_prompts: bool,
        /// Ask for a checkpoint on Stop when files changed and the last one is older than N minutes (0 = off).
        #[arg(long, env = "MEMDUSA_STOP_GUARD_MINS", default_value_t = 15)]
        stop_guard_mins: i64,
    },
    /// Record a git hook event. Installed by `memdusa install git`.
    GitHook {
        #[arg(value_enum)]
        hook: install::GitHookKind,
        /// Arguments git passes to the hook (checkout: prev, new, flag; merge: squash).
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Import saved agent sessions (work done before memdusa was installed, or without hooks).
    ImportTranscript {
        #[arg(value_enum)]
        harness: transcripts::TranscriptHarness,
        /// Transcript files.
        files: Vec<PathBuf>,
        /// The most recent session for the current directory's repo.
        #[arg(long)]
        latest: bool,
        /// Import sessions that were already imported (or recorded by hooks).
        #[arg(long)]
        force: bool,
        /// Print what would be imported without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Wire memdusa into a harness: installs its hooks and registers the MCP server.
    Install {
        #[arg(value_enum)]
        harness: adapters::Harness,
        #[arg(long, value_enum, default_value = "user")]
        scope: install::Scope,
        /// Settings file to edit instead of the scope's default.
        #[arg(long)]
        settings: Option<PathBuf>,
        /// Command to invoke memdusa with. Default: absolute path of this binary.
        #[arg(long)]
        bin: Option<String>,
        /// Only install hooks; don't register the MCP server.
        #[arg(long)]
        no_mcp: bool,
        /// Print the resulting settings without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove memdusa's hooks (and MCP registration) from a harness.
    Uninstall {
        #[arg(value_enum)]
        harness: adapters::Harness,
        #[arg(long, value_enum, default_value = "user")]
        scope: install::Scope,
        #[arg(long)]
        settings: Option<PathBuf>,
        #[arg(long)]
        no_mcp: bool,
    },
    /// Update memdusa from the latest GitHub release (or a specific version).
    SelfUpdate {
        /// Version to install, e.g. v0.1.1 (default: the latest release).
        #[arg(long)]
        version: Option<String>,
        /// Only report whether an update is available; don't install anything.
        #[arg(long)]
        check: bool,
    },
}

/// Retention windows in days (0 = keep forever).
#[derive(clap::Args, Clone, Copy)]
struct RetentionArgs {
    /// Days of raw hook activity to keep before summarizing it.
    #[arg(long, env = "MEMDUSA_RETENTION_ACTIVITY_DAYS", default_value_t = 30)]
    activity_days: i64,
    /// Days to keep superseded checkpoints (the latest per task is always kept).
    #[arg(long, env = "MEMDUSA_RETENTION_CHECKPOINT_DAYS", default_value_t = 90)]
    checkpoint_days: i64,
    /// Days to keep finished runs.
    #[arg(long, env = "MEMDUSA_RETENTION_RUN_DAYS", default_value_t = 90)]
    run_days: i64,
}

impl From<RetentionArgs> for RetentionPolicy {
    fn from(a: RetentionArgs) -> Self {
        Self { activity_days: a.activity_days, checkpoint_days: a.checkpoint_days, run_days: a.run_days }
    }
}

/// Retention policy for automatic pruning (env overrides, else defaults).
fn retention_from_env() -> RetentionPolicy {
    let days = |var: &str, default: i64| std::env::var(var).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default);
    let d = RetentionPolicy::default();
    RetentionPolicy {
        activity_days: days("MEMDUSA_RETENTION_ACTIVITY_DAYS", d.activity_days),
        checkpoint_days: days("MEMDUSA_RETENTION_CHECKPOINT_DAYS", d.checkpoint_days),
        run_days: days("MEMDUSA_RETENTION_RUN_DAYS", d.run_days),
    }
}

fn read_input(file: &str) -> anyhow::Result<String> {
    if file == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
        Ok(s)
    } else {
        std::fs::read_to_string(file).with_context(|| format!("reading {file}"))
    }
}

fn write_export(text: &str, output: Option<PathBuf>) -> anyhow::Result<()> {
    match output {
        Some(p) => {
            std::fs::write(&p, text)?;
            eprintln!("exported {} record(s) to {}", text.lines().count().saturating_sub(1), p.display());
        }
        None => print!("{text}"),
    }
    Ok(())
}

/// What `remember` did: a new memory, a near-duplicate updated in place, or an identical one refreshed.
fn saved_line(m: &model::Memory) -> String {
    if m.merged {
        format!("updated existing {} (near-duplicate, newer wording kept)", m.id)
    } else if m.updated_at != m.created_at {
        format!("already saved {} (refreshed)", m.id)
    } else {
        format!("saved {}", m.id)
    }
}

fn print_import(r: &transfer::ImportReport) {
    println!(
        "imported {} memories, {} events, {} checkpoints, {} runs ({} steps); skipped {} existing",
        r.memories, r.events, r.checkpoints, r.runs, r.steps, r.skipped
    );
}

fn print_prune(r: &memdusa_core::retention::PruneReport) {
    let verb = if r.dry_run { "would" } else { "did" };
    println!(
        "{verb} summarize {} session(s) ({} activity events), delete {} checkpoint(s), {} run(s) ({} steps), {} audit event(s)",
        r.sessions_summarized,
        r.events_deleted,
        r.checkpoints_deleted,
        r.runs_deleted,
        r.steps_deleted,
        r.audit_events_deleted
    );
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Mark the task done (it is never redone: a worker started on it later is told so).
    Done {
        /// What was done and where to find it; shown to the coordinator.
        #[arg(long)]
        summary: Option<String>,
    },
    /// Give the task up; a replacement worker can claim it again.
    Fail {
        /// Why it can't be finished.
        reason: String,
    },
}

impl TaskCmd {
    /// The (run, task) from the worker's environment, and the outcome to record.
    fn outcome(self) -> anyhow::Result<(String, String, Result<Option<serde_json::Value>, String>)> {
        let run = env_nonempty("MEMDUSA_RUN").context("MEMDUSA_RUN is not set: this isn't a delegated worker")?;
        let task = env_nonempty("MEMDUSA_TASK").context("MEMDUSA_TASK is not set: this isn't a delegated worker")?;
        let outcome = match self {
            Self::Done { summary } => {
                Ok(summary.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).map(serde_json::Value::String))
            }
            Self::Fail { reason } => Err(reason),
        };
        Ok((run, task, outcome))
    }
}

#[derive(Subcommand)]
enum UserCmd {
    /// Create a user and their first API key (printed once).
    Add {
        name: String,
        #[arg(long)]
        admin: bool,
        /// Label for the first key.
        #[arg(long, default_value = "default")]
        key_name: String,
    },
    List,
    /// Disable a user: all their keys stop working immediately.
    Disable {
        name: String,
    },
    Enable {
        name: String,
    },
    /// Link an external identity (lazy: binds on first sign-in by login/email).
    /// Example: github:octocat, github:12345, google:alice@acme.com
    Link {
        user: String,
        identity: String,
    },
    /// Remove a linked or pending external identity.
    Unlink {
        user: String,
        identity: String,
    },
    /// List a user's linked external identities.
    Identities {
        user: String,
    },
}

#[derive(Subcommand)]
enum KeyCmd {
    /// Create a key (printed once). Remote: your own unless --user. Local: --user is required.
    Create {
        #[arg(long)]
        user: Option<String>,
        /// Label, e.g. "laptop" or "ci".
        #[arg(long, default_value = "default")]
        name: String,
    },
    /// List keys. Remote: your own unless --user. Local: everyone's unless --user.
    List {
        #[arg(long)]
        user: Option<String>,
    },
    /// Revoke a key by id or by its prefix (e.g. mdk_1a2b3c4d).
    Revoke { key: String },
}

#[derive(Subcommand)]
enum SessionCmd {
    /// List active sign-in sessions (your own, or everyone's with --user).
    List {
        #[arg(long)]
        user: Option<String>,
    },
    /// Revoke a session by id (revokes its whole grant chain).
    Revoke { id: String },
}

/// Stack for the main thread and the runtime's workers. Windows gives the main thread only 1 MB
/// (8 MB on macOS/Linux), and the CLI's async state machine outgrew that ("thread 'main' has
/// overflowed its stack" in CI), so run everything on threads with an explicit, uniform stack.
const STACK_SIZE: usize = 8 * 1024 * 1024;

fn main() -> anyhow::Result<()> {
    let worker = std::thread::Builder::new().name("memdusa".into()).stack_size(STACK_SIZE).spawn(|| {
        tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(STACK_SIZE).build()?.block_on(run())
    })?;
    worker.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // stdout is the MCP channel in stdio mode: logs must go to stderr.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "memdusa=info,warn".into());
    let logs = tracing_subscriber::fmt().with_writer(std::io::stderr).with_env_filter(filter);
    if std::env::var("MEMDUSA_LOG_FORMAT").is_ok_and(|f| f.eq_ignore_ascii_case("json")) {
        logs.json().init();
    } else {
        logs.init();
    }

    match cli.cmd {
        Cmd::Install { harness, scope, settings, bin, no_mcp, dry_run } => {
            return install::run(install::InstallOptions {
                harness,
                scope,
                settings,
                bin,
                database_url: cli.database_url.clone(),
                mcp: !no_mcp,
                dry_run,
                uninstall: false,
            });
        }
        Cmd::Uninstall { harness, scope, settings, no_mcp } => {
            return install::run(install::InstallOptions {
                harness,
                scope,
                settings,
                bin: Some("memdusa".into()),
                database_url: None,
                mcp: !no_mcp,
                dry_run: false,
                uninstall: true,
            });
        }
        Cmd::Hook { event, harness, agent, no_prompts, stop_guard_mins } => {
            // A hook must never break the user's session: log and exit 0.
            let agent = agent.unwrap_or_else(|| harness.agent().into());
            // MEMDUSA_RECALL=off (or 0/false/no) stops memories being injected on each prompt.
            let recall = !std::env::var("MEMDUSA_RECALL")
                .is_ok_and(|v| matches!(v.trim().to_lowercase().as_str(), "off" | "0" | "false" | "no"));
            let cfg = hooks::HookConfig { agent, journal_prompts: !no_prompts, stop_guard_mins, recall };
            // Finish well inside the harness's hook timeout instead of stalling the session.
            let budget = harness.events().iter().filter(|h| h.event == event).map(|h| h.timeout_secs).max();
            let deadline = Duration::from_millis(budget.unwrap_or(event.timeout_secs()) * 1000 - 500);
            let result =
                tokio::time::timeout(deadline, run_hook(event, harness, cfg, cli.project, cli.database_url, deadline))
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out after {deadline:?}")));
            match result {
                Ok(Some(out)) => println!("{out}"),
                Ok(None) => {}
                Err(e) => eprintln!("memdusa hook {}: {e:#}", event.arg()),
            }
            return Ok(());
        }
        Cmd::GitHook { hook, args } => {
            // Git hooks must never fail a git operation or print to stdout.
            let deadline = Duration::from_millis(2500);
            let result =
                tokio::time::timeout(deadline, run_git_hook(hook, &args, cli.project, cli.database_url, deadline))
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out after {deadline:?}")));
            if let Err(e) = result {
                eprintln!("memdusa git-hook {}: {e:#}", hook.name());
            }
            return Ok(());
        }
        Cmd::SelfUpdate { version, check } => {
            return self_update::run(version, check).await;
        }
        Cmd::Login { url, token, browser, device } => {
            if browser || device {
                let timeout = std::env::var("MEMDUSA_LOGIN_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .filter(|v| *v > 0)
                    .map(Duration::from_secs)
                    .unwrap_or(Duration::from_secs(300));
                let tokens = if browser {
                    cli_auth::browser_login(
                        &url,
                        &cli_auth::BrowserOptions { opener: cli_auth::BrowserOpener::System, timeout },
                    )
                    .await?
                } else {
                    cli_auth::device_login(&url, &cli_auth::DeviceOptions { timeout, on_code: None }).await?
                };
                print_login(&config::Remote::new(&url, None)?.url, &tokens);
                return Ok(());
            }
            let r = config::Remote::new(&url, token)?;
            let info = remote::Client::new(r.clone(), Duration::from_secs(10))?
                .whoami()
                .await
                .context("could not log in (check the URL and token)")?;
            let path = config::save_remote(Some(r.clone()))?;
            println!(
                "logged in to {} as {} (server v{}, {})",
                r.url,
                describe_identity(&info),
                info["version"].as_str().unwrap_or("?"),
                info["backend"].as_str().unwrap_or("?")
            );
            println!("saved to {}", path.display());
            return Ok(());
        }
        Cmd::Logout => {
            if let Some(r) = config::remote()?
                && r.oauth.is_some()
            {
                cli_auth::logout(&r.url).await?;
            }
            config::save_remote(None)?;
            println!("logged out; using the local database");
            return Ok(());
        }
        _ => {}
    }

    let project = cli.project.clone().or_else(project::detect_cwd);
    if let Cmd::Project = cli.cmd {
        println!("{}", project.as_deref().unwrap_or("<none>"));
        return Ok(());
    }

    // `serve` always owns a database; everything else follows the configured mode.
    let mode = match &cli.cmd {
        Cmd::Serve { .. } => Mode::Local(cli.database_url.clone().map_or_else(default_db_url, Ok)?),
        _ => Mode::detect(cli.database_url.clone())?,
    };
    // An explicit model download is the only path allowed to touch the network for model files.
    let allow_download = matches!(&cli.cmd, Cmd::Embed { download_model: true, .. });
    let store = match &mode {
        Mode::Remote(r) => {
            if allow_download {
                println!("local embedding models are installed where memdusa serve runs, not on this client");
            }
            return run_remote(cli.cmd, r, project, cli.project).await;
        }
        Mode::Local(url) => open_store(url, allow_download).await.with_context(|| format!("connecting to {url}"))?,
    };

    match cli.cmd {
        Cmd::Status => {
            let Mode::Local(url) = &mode else { unreachable!() };
            println!("mode: local database {url} (schema ok, {} users)", store.user_count().await?);
            let s = store.embedding_stats().await?;
            println!("{}", semantic_line(s.model.as_deref(), s.embedded, s.total));
            if let Some(hint) = embedder::local_status_hint() {
                println!("{hint}");
            }
            println!("project: {}", project.as_deref().unwrap_or("<none>"));
        }
        Cmd::Embed { all, only_project, .. } => {
            print_backfill(&store.embeddings_backfill(all, only_project.as_deref()).await?)
        }
        Cmd::Sync => println!("local mode: nothing to sync (the queue is only used with a remote server)"),
        Cmd::ImportTranscript { harness, files, latest, force, dry_run } => {
            let opts = transcripts::ImportOptions { harness, files, latest, force, dry_run, project: cli.project };
            transcripts::run(opts, transcripts::Backend::Local(&store)).await?;
        }
        Cmd::Export { only_project, output } => {
            write_export(&transfer::to_jsonl(&store.export(only_project.as_deref()).await?), output)?
        }
        Cmd::Import { file } => print_import(&store.import(transfer::parse_jsonl(&read_input(&file)?)?).await?),
        Cmd::Backup { path } => {
            store.backup_sqlite(&path.display().to_string()).await?;
            println!("backed up to {}", path.display());
        }
        Cmd::User { cmd } => match cmd {
            UserCmd::Add { name, admin, key_name } => {
                let user = store.user_create(&name, admin).await?;
                let key = store.key_create(&user.name, &key_name).await?;
                print_user(&user);
                print_created(&key);
            }
            UserCmd::List => store.user_list().await?.iter().for_each(print_user),
            UserCmd::Disable { name } => print_user(&store.user_set_disabled(&name, true).await?),
            UserCmd::Enable { name } => print_user(&store.user_set_disabled(&name, false).await?),
            UserCmd::Link { user, identity } => {
                let (provider, handle) = oauth_upstream::parse_link(&identity)?;
                let (subject, login, email) = if oauth_upstream::is_stable_subject(&handle) {
                    (handle.clone(), None, None)
                } else if provider == oauth_upstream::GITHUB {
                    (format!("{}{}", oauth_upstream::PENDING_PREFIX, handle), Some(handle.clone()), None)
                } else {
                    (format!("{}{}", oauth_upstream::PENDING_PREFIX, handle), None, Some(handle.clone()))
                };
                let linked =
                    store.identity_insert(&user, &provider, &subject, email.as_deref(), login.as_deref()).await?;
                store
                    .audit(
                        memdusa_core::model::NewAuditEvent::ok("root", "oauth.identity_link")
                            .target(&user)
                            .detail(serde_json::json!({"provider": provider, "handle": handle})),
                    )
                    .await?;
                print_identity(&linked);
            }
            UserCmd::Unlink { user, identity } => {
                let (provider, handle) = oauth_upstream::parse_link(&identity)?;
                let n = store.identity_unlink(&user, &provider, &handle).await?;
                store
                    .audit(
                        memdusa_core::model::NewAuditEvent::ok("root", "oauth.identity_unlink")
                            .target(&user)
                            .detail(serde_json::json!({"provider": provider, "handle": handle, "removed": n})),
                    )
                    .await?;
                println!("unlinked {n} identity/identities from {user}");
            }
            UserCmd::Identities { user } => {
                for i in store.identity_list(&user).await? {
                    print_identity(&i);
                }
            }
        },
        Cmd::Key { cmd } => match cmd {
            KeyCmd::Create { user, name } => {
                let user = user.context("local mode: pass --user <name>")?;
                print_created(&store.key_create(&user, &name).await?);
            }
            KeyCmd::List { user } => store.key_list(user.as_deref()).await?.iter().for_each(print_key),
            KeyCmd::Revoke { key } => print_key(&store.key_revoke(&key, None).await?),
        },
        Cmd::Grant { user, pattern, role } => {
            let role = Role::parse(&role).context("--role must be read, write or none")?;
            let g = store.grant_create(&pattern, &user, role, "root").await?;
            store
                .audit(memdusa_core::model::NewAuditEvent::ok("root", "grant.create").project(&g.pattern).target(&g.id))
                .await?;
            print_grant(&g);
        }
        Cmd::Grants { user } => store.grants_for_user(user.as_deref()).await?.iter().for_each(print_grant),
        Cmd::RevokeGrant { id } => {
            let g = store.grant_delete(&id).await?;
            store
                .audit(memdusa_core::model::NewAuditEvent::ok("root", "grant.delete").project(&g.pattern).target(&g.id))
                .await?;
            print_grant(&g);
        }
        Cmd::Sessions { cmd } => match cmd {
            SessionCmd::List { user } => {
                store.oauth_sessions_list(user.as_deref()).await?.iter().for_each(print_session)
            }
            SessionCmd::Revoke { id } => {
                let n = store.oauth_session_revoke(&id, None, model::now_ms()).await?;
                println!("revoked {n} session token(s)");
            }
        },
        Cmd::Audit { since, actor, project, limit } => {
            let q = audit_query(since, actor, project, limit)?;
            store.audit_query(q).await?.iter().for_each(print_audit);
        }
        Cmd::Mcp { agent } => {
            tracing::info!(?project, agent, "memdusa MCP server on stdio");
            let server = mcp::MemdusaMcp::new(
                store,
                mcp::Defaults {
                    project,
                    agent,
                    cwd: std::env::current_dir().ok(),
                    task: env_nonempty("MEMDUSA_TASK"),
                    run: env_nonempty("MEMDUSA_RUN"),
                },
            );
            let running = server.serve(rmcp::transport::stdio()).await?;
            running.waiting().await?;
        }
        Cmd::Serve { addr, token, allowed_hosts, no_auto_prune, rate_limit, no_metrics } => {
            let auto_prune = (!no_auto_prune).then(retention_from_env);
            let oauth = oauth::OAuthConfig::from_env().context("invalid MEMDUSA_PUBLIC_URL")?;
            if let Some(c) = &oauth {
                tracing::info!("OAuth sign-in enabled (issuer {})", c.issuer);
                for (provider, url) in c.callback_urls() {
                    tracing::info!("register this {provider} OAuth callback URL: {url}");
                }
            }
            let opts = http::HttpOptions {
                addr,
                token,
                allowed_hosts,
                auto_prune,
                rate_limit,
                metrics: !no_metrics,
                default_access: DefaultPolicy::from_env(),
                oauth,
            };
            http::serve(store, opts).await?;
        }
        Cmd::Prune { dry_run, only_project, policy } => {
            print_prune(&store.prune(&policy.into(), only_project.as_deref(), dry_run).await?);
        }
        Cmd::Resume { json } => {
            let project = project.context("could not detect project; pass --project")?;
            let task = env_nonempty("MEMDUSA_TASK");
            let git = project::git_state_for(&project);
            let b = brief::build(&store, &project, None, git.as_ref(), task.as_deref()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&b)?);
            } else {
                print!("{}", b.markdown);
            }
        }
        Cmd::Delegate { name, tasks, key } => {
            let project = project.context("could not detect project; pass --project")?;
            print!("{}", brief::delegate_report(&store.run_delegate(&project, &name, tasks, key).await?));
        }
        Cmd::Task { cmd } => {
            let (run, task, outcome) = cmd.outcome()?;
            match outcome {
                Ok(output) => store.step_complete(&run, &task, output).await?,
                Err(reason) => store.step_fail(&run, &task, &reason).await?,
            };
            println!("task {task}: recorded");
        }
        Cmd::Remember { content, kind, global } => {
            let project = if global {
                None
            } else {
                Some(project.context("could not detect project; pass --project or --global")?)
            };
            let m = store
                .memory_save(NewMemory {
                    project,
                    kind: MemoryKind::parse(&kind),
                    content,
                    tags: vec![],
                    source: Some("cli".into()),
                    user: None,
                })
                .await?;
            println!("{}", saved_line(&m));
        }
        Cmd::Recall { query } => {
            for m in store.memory_search(MemoryQuery { query, project, ..Default::default() }).await? {
                let scope = m.project.as_deref().unwrap_or("global");
                println!("[{}] ({}) {}  — {}", m.kind.as_str(), scope, m.content, m.id);
            }
        }
        Cmd::Review => {
            let project = project.context("could not detect project; pass --project")?;
            let review = store.memory_review(&project, None).await?;
            print!("{}", review.markdown(&project, memdusa_core::model::now_ms()));
        }
        Cmd::Project
        | Cmd::Hook { .. }
        | Cmd::GitHook { .. }
        | Cmd::Install { .. }
        | Cmd::Uninstall { .. }
        | Cmd::SelfUpdate { .. }
        | Cmd::Login { .. }
        | Cmd::Logout => unreachable!(),
    }
    Ok(())
}

/// Where this machine's state lives.
enum Mode {
    Local(String),
    Remote(config::Remote),
}

impl Mode {
    /// An explicit database URL wins; then a logged-in server; then the local default.
    fn detect(database_url: Option<String>) -> anyhow::Result<Self> {
        if let Some(url) = database_url {
            return Ok(Self::Local(url));
        }
        if let Some(r) = config::remote()? {
            return Ok(Self::Remote(r));
        }
        Ok(Self::Local(default_db_url()?))
    }
}

async fn run_remote(
    cmd: Cmd,
    r: &config::Remote,
    project: Option<String>,
    explicit_project: Option<String>,
) -> anyhow::Result<()> {
    // Re-read config each time: a previous call (or another hook process) may
    // have rotated the refresh token since this command started.
    let client = || remote::Client::from_config(Duration::from_secs(15));
    match cmd {
        Cmd::Status => {
            for line in status_lines(r, project.as_deref()).await? {
                println!("{line}");
            }
        }
        Cmd::ImportTranscript { harness, files, latest, force, dry_run } => {
            let client = client()?;
            let opts = transcripts::ImportOptions { harness, files, latest, force, dry_run, project: explicit_project };
            transcripts::run(opts, transcripts::Backend::Remote(&client)).await?;
        }
        Cmd::Prune { dry_run, only_project, policy } => {
            print_prune(&client()?.prune(&policy.into(), only_project.as_deref(), dry_run).await?);
        }
        Cmd::Export { only_project, output } => {
            write_export(&client()?.export(only_project.as_deref()).await?, output)?
        }
        Cmd::Import { file } => {
            // in chunks, so a big history doesn't hit the request size limit
            let text = read_input(&file)?;
            transfer::parse_jsonl(&text)?; // validate everything before sending anything
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            let mut total = transfer::ImportReport::default();
            let c = client()?;
            for chunk in lines.chunks(2000) {
                let r = c.import(&(chunk.join("\n") + "\n")).await?;
                total.memories += r.memories;
                total.events += r.events;
                total.checkpoints += r.checkpoints;
                total.runs += r.runs;
                total.steps += r.steps;
                total.skipped += r.skipped;
            }
            print_import(&total);
        }
        Cmd::Embed { all, only_project, .. } => {
            print_backfill(&client()?.embeddings_backfill(all, only_project.as_deref()).await?)
        }
        Cmd::Backup { .. } => anyhow::bail!("backup copies a local SQLite file; against a server use `memdusa export`"),
        Cmd::Sync => {
            let spool = spool::Spool::open()?;
            let n = spool.flush(&client()?, &r.url).await?;
            println!("synced {n} event(s); {} still queued", spool.len());
        }
        Cmd::User { cmd } => {
            let c = client()?;
            match cmd {
                UserCmd::Add { name, admin, key_name } => {
                    let r = c.user_add(&name, admin, &key_name).await?;
                    print_user(&r.user);
                    print_created(&r.key);
                }
                UserCmd::List => c.user_list().await?.iter().for_each(print_user),
                UserCmd::Disable { name } => print_user(&c.user_set_disabled(&name, true).await?),
                UserCmd::Enable { name } => print_user(&c.user_set_disabled(&name, false).await?),
                UserCmd::Link { user, identity } => {
                    let (provider, handle) = oauth_upstream::parse_link(&identity)?;
                    print_identity(&c.identity_link(&user, &provider, &handle).await?);
                }
                UserCmd::Unlink { user, identity } => {
                    let (provider, handle) = oauth_upstream::parse_link(&identity)?;
                    let r = c.identity_unlink(&user, &provider, &handle).await?;
                    println!("unlinked {} identity/identities from {user}", r["unlinked"]);
                }
                UserCmd::Identities { user } => {
                    for i in c.identity_list(&user).await? {
                        print_identity(&i);
                    }
                }
            }
        }
        Cmd::Key { cmd } => {
            let c = client()?;
            match cmd {
                KeyCmd::Create { user, name } => print_created(&c.key_create(user.as_deref(), &name).await?),
                KeyCmd::List { user } => c.key_list(user.as_deref()).await?.iter().for_each(print_key),
                KeyCmd::Revoke { key } => print_key(&c.key_revoke(&key).await?),
            }
        }
        Cmd::Grant { user, pattern, role } => {
            let role = Role::parse(&role).context("--role must be read, write or none")?;
            print_grant(&client()?.grant_create(&user, &pattern, role).await?);
        }
        Cmd::Grants { user } => client()?.grants(user.as_deref()).await?.iter().for_each(print_grant),
        Cmd::RevokeGrant { id } => print_grant(&client()?.grant_revoke(&id).await?),
        Cmd::Sessions { cmd } => {
            let c = client()?;
            match cmd {
                SessionCmd::List { user } => c.sessions(user.as_deref()).await?.iter().for_each(print_session),
                SessionCmd::Revoke { id } => {
                    let r = c.session_revoke(&id).await?;
                    println!("revoked {} session token(s)", r["revoked"].as_i64().unwrap_or(0));
                }
            }
        }
        Cmd::Audit { since, actor, project, limit } => {
            let q = audit_query(since, actor, project, limit)?;
            client()?.audit(&q).await?.iter().for_each(print_audit);
        }
        Cmd::Mcp { agent } => proxy::run(r, project, &agent).await?,
        Cmd::Resume { json } => {
            let project = project.context("could not detect project; pass --project")?;
            let task = env_nonempty("MEMDUSA_TASK");
            let git = project::git_state_for(&project);
            let b = client()?.resume(&project, git.as_ref(), task.as_deref()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&b)?);
            } else {
                print!("{}", b["markdown"].as_str().unwrap_or_default());
            }
        }
        Cmd::Delegate { name, tasks, key } => {
            let project = project.context("could not detect project; pass --project")?;
            let run = client()?.delegate(&project, &name, &tasks, key.as_deref()).await?;
            print!("{}", brief::delegate_report(&run));
        }
        Cmd::Task { cmd } => {
            let (run, task, outcome) = cmd.outcome()?;
            client()?.task_finish(&run, &task, outcome).await?;
            println!("task {task}: recorded");
        }
        Cmd::Remember { content, kind, global } => {
            let project = if global {
                None
            } else {
                Some(project.context("could not detect project; pass --project or --global")?)
            };
            let m = NewMemory {
                project,
                kind: MemoryKind::parse(&kind),
                content,
                tags: vec![],
                source: Some("cli".into()),
                user: None, // the server sets it from the key
            };
            println!("{}", saved_line(&client()?.memory_save(&m).await?));
        }
        Cmd::Recall { query } => {
            for m in client()?.memory_search(&MemoryQuery { query, project, ..Default::default() }).await? {
                let scope = m.project.as_deref().unwrap_or("global");
                println!("[{}] ({}) {}  — {}", m.kind.as_str(), scope, m.content, m.id);
            }
        }
        Cmd::Review => {
            let project = project.context("could not detect project; pass --project")?;
            let review = client()?.memory_review(&project).await?;
            print!("{}", review.markdown(&project, memdusa_core::model::now_ms()));
        }
        Cmd::SelfUpdate { .. } => unreachable!("handled before mode selection"),
        _ => unreachable!("handled before mode selection"),
    }
    Ok(())
}

/// An environment variable's trimmed value, if set and not blank.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// Collect facts about a git hook event and send them through the hook pipeline.
/// The git hook wrapper always exits 0, so any error here only reaches stderr.
async fn run_git_hook(
    kind: install::GitHookKind,
    args: &[String],
    project: Option<String>,
    database_url: Option<String>,
    deadline: Duration,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let Some(input) = git_hook_input(kind, args, &cwd)? else { return Ok(()) };
    let agent =
        std::env::var("MEMDUSA_AGENT").ok().filter(|a| !a.trim().is_empty()).unwrap_or_else(|| "git".to_owned());
    let config = hooks::HookConfig { agent, journal_prompts: true, stop_guard_mins: 0, recall: false };
    run_hook_input(hooks::HookEvent::Git, adapters::Harness::Git, config, input, project, database_url, deadline)
        .await?;
    Ok(())
}

/// Facts for one git hook, or `None` when the hook should record nothing
/// (a file checkout, or a branch switch that stayed on the same branch).
fn git_hook_input(
    kind: install::GitHookKind,
    args: &[String],
    dir: &Path,
) -> anyhow::Result<Option<serde_json::Value>> {
    let root = git(dir, &["rev-parse", "--show-toplevel"])?;
    let head_sha = || git(dir, &["rev-parse", "HEAD"]);
    let short_sha =
        |sha: &str| git(dir, &["rev-parse", "--short", sha]).unwrap_or_else(|_| sha.chars().take(7).collect());
    let branch = |short: &str| {
        git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
            .ok()
            .filter(|b| !b.is_empty() && b != "HEAD")
            .unwrap_or_else(|| short.to_owned())
    };

    let (content, data) = match kind {
        install::GitHookKind::Commit => {
            let full = head_sha()?;
            let short = short_sha(&full);
            let subject = git(dir, &["log", "-1", "--pretty=%s"])?;
            let branch = branch(&short);
            let changed = git(dir, &["diff-tree", "--root", "--no-commit-id", "--name-only", "-r", "HEAD"])?;
            let files = changed.lines().filter(|l| !l.trim().is_empty()).count();
            let content = format!("{short} on {branch}: {subject} ({files} files)");
            (
                content,
                serde_json::json!({
                    "sha": full, "branch": branch, "subject": subject, "files": files
                }),
            )
        }
        install::GitHookKind::Checkout => {
            let (Some(prev), Some(new), Some(flag)) = (args.first(), args.get(1), args.get(2)) else {
                return Ok(None);
            };
            // flag 1 = branch checkout; 0 is a file checkout, which is not an event.
            if flag != "1" {
                return Ok(None);
            }
            let to = branch(&short_sha(new));
            let from = git(dir, &["rev-parse", "--abbrev-ref", "@{-1}"])
                .ok()
                .filter(|b| !b.is_empty() && b != "HEAD")
                .unwrap_or_else(|| short_sha(prev));
            // `git checkout -b x` keeps HEAD but changes the branch; `git checkout main`
            // while already on main changes neither.
            if prev == new && from == to {
                return Ok(None);
            }
            let content = format!("switched from {from} to {to}");
            (
                content,
                serde_json::json!({
                    "from": from, "to": to, "prev": prev, "new": new
                }),
            )
        }
        install::GitHookKind::Merge => {
            let squash = args.first().is_some_and(|a| a == "1");
            let full = head_sha()?;
            let short = short_sha(&full);
            let subject = git(dir, &["log", "-1", "--pretty=%s"])?;
            let branch = branch(&short);
            let content = format!("merged into {branch}: {subject}");
            (
                content,
                serde_json::json!({
                    "branch": branch, "sha": full, "squash": squash
                }),
            )
        }
    };

    Ok(Some(serde_json::json!({
        "cwd": root,
        "git_event": kind.event(),
        "content": content,
        "data": data,
    })))
}

/// Run git in `dir`; stdout is trimmed. Non-zero exit is an error (with stderr).
fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().context("running git")?;
    if !out.status.success() {
        anyhow::bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

async fn run_hook(
    event: hooks::HookEvent,
    harness: adapters::Harness,
    config: hooks::HookConfig,
    project: Option<String>,
    database_url: Option<String>,
    deadline: Duration,
) -> anyhow::Result<Option<String>> {
    let mut raw = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw)?;
    let native: serde_json::Value =
        if raw.trim().is_empty() { serde_json::json!({}) } else { serde_json::from_str(&raw)? };
    run_hook_input(event, harness, config, native, project, database_url, deadline).await
}

/// The rest of [`run_hook`], on an already-built payload. `memdusa git-hook`
/// reuses it so git events go through exactly the same pipeline.
async fn run_hook_input(
    event: hooks::HookEvent,
    harness: adapters::Harness,
    config: hooks::HookConfig,
    native: serde_json::Value,
    project: Option<String>,
    database_url: Option<String>,
    deadline: Duration,
) -> anyhow::Result<Option<String>> {
    // Translate the harness's payload into memdusa's canonical one (or ignore this event).
    let Some(mut input) = harness.normalize(event, native) else { return Ok(None) };

    // Hermes runs hooks from its install dir: take the session's real workspace from its state.db.
    if harness == adapters::Harness::Hermes
        && let Some(home) = hermes::home()
    {
        let sid = input.get("session_id").and_then(|v| v.as_str()).map(String::from);
        let payload_cwd = input.get("cwd").and_then(|v| v.as_str()).map(String::from);
        let resolved = hermes::resolve_cwd(&home, sid.as_deref(), payload_cwd.as_deref()).await;
        if let Some(obj) = input.as_object_mut() {
            match resolved {
                Some(c) => obj.insert("cwd".into(), c.into()),
                None => obj.remove("cwd"),
            };
        }
    }

    // The project is always detected here, on the machine that has the repo.
    let project_dir = ["CLAUDE_PROJECT_DIR", "CURSOR_PROJECT_DIR", "GEMINI_PROJECT_DIR"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|d| !d.is_empty()));
    if input.get("cwd").and_then(|v| v.as_str()).is_none()
        && let (Some(obj), Some(dir)) = (input.as_object_mut(), project_dir)
    {
        obj.insert("cwd".into(), dir.into());
    }
    let cwd = input.get("cwd").and_then(|v| v.as_str()).map(String::from);
    let project = project
        .or_else(|| cwd.as_deref().map(|d| project::detect(std::path::Path::new(d))))
        .or_else(project::detect_cwd)
        .context("could not determine project")?;
    // Code state only for events that use it, and only if the hook's directory is this project's repo.
    let git = cwd
        .as_deref()
        .map(std::path::Path::new)
        .filter(|d| event.needs_git() && project::detect(d) == project)
        .and_then(project::git_state);
    let mut req = hooks::HookRequest {
        project,
        config,
        input,
        git,
        at: Some(model::now_ms()),
        task: env_nonempty("MEMDUSA_TASK"),
        parent: env_nonempty("MEMDUSA_PARENT"),
        run: env_nonempty("MEMDUSA_RUN"),
    };

    let resp = match Mode::detect(database_url)? {
        Mode::Remote(r) => {
            // Redact on this machine too, so secrets never leave it (the server redacts again).
            req.input = redactor_from_env()?.json(req.input);
            remote_hook(event, req, &r, deadline).await?
        }
        Mode::Local(url) => {
            let store = open_store(&url, false).await?;
            let resp = hooks::handle_request(&store, event, req, &model::Identity::root()).await?;
            // Local mode has no server to prune on a schedule: do it at session end, at most daily.
            if let hooks::HookEvent::SessionEnd = event
                && let Err(e) = store.maybe_prune(&retention_from_env(), 86_400_000).await
            {
                eprintln!("memdusa: pruning failed: {e}");
            }
            resp
        }
    };
    Ok(harness.render(event, resp.output))
}

/// Remote hook with an offline fallback: replay any backlog first, then send
/// this event. If the server is unreachable, queue write-only events locally
/// and serve SessionStart from the cached brief.
async fn remote_hook(
    event: hooks::HookEvent,
    req: hooks::HookRequest,
    r: &config::Remote,
    deadline: Duration,
) -> anyhow::Result<hooks::HookResponse> {
    let spool = spool::Spool::open()?;
    let mut unreachable = None;
    if !spool.is_empty() {
        // a short budget, so a hanging server can't eat this hook's whole deadline
        let flush_client = remote::Client::from_config((deadline / 3).min(Duration::from_secs(2)))?;
        match spool.flush(&flush_client, &r.url).await {
            Ok(n) if n > 0 => eprintln!("memdusa: synced {n} queued event(s)"),
            Ok(_) => {}
            Err(e) if e.retryable => unreachable = Some(e),
            Err(e) => eprintln!("memdusa: could not sync the offline queue: {e}"),
        }
    }
    let result = match unreachable {
        Some(e) => Err(e), // just failed to connect: don't wait for another timeout
        None => remote::Client::from_config(deadline)?.hook(event, &req).await,
    };
    match result {
        Ok(resp) => {
            if let (hooks::HookEvent::SessionStart, Some(out)) = (event, &resp.output)
                && let Err(e) = spool::cache_brief(&req.project, out)
            {
                eprintln!("memdusa: could not cache the brief: {e:#}");
            }
            Ok(resp)
        }
        Err(e) if e.retryable => {
            let project = req.project.clone();
            if event.queueable() {
                spool.push(&r.url, hooks::HookBatchItem { event: event.arg().into(), request: req })?;
                let why =
                    if e.needs_login { "sign-in expired; run `memdusa login` again" } else { "server unreachable" };
                eprintln!("memdusa: {why}, queued {} for later ({e})", event.arg());
            }
            if let hooks::HookEvent::SessionStart = event {
                return Ok(spool::cached_brief(&project, &e.message).unwrap_or_default());
            }
            Ok(hooks::HookResponse::default())
        }
        Err(e) => Err(e.into()),
    }
}

/// Redaction settings: `MEMDUSA_REDACT=off` disables it; extra regexes come
/// from `MEMDUSA_REDACT_PATTERNS` (one per line) and `<MEMDUSA_HOME>/redact-patterns.txt`.
fn redactor_from_env() -> anyhow::Result<Redactor> {
    if let Ok(v) = std::env::var("MEMDUSA_REDACT")
        && matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no")
    {
        return Ok(Redactor::disabled());
    }
    let mut patterns: Vec<String> =
        std::env::var("MEMDUSA_REDACT_PATTERNS").map(|v| v.lines().map(String::from).collect()).unwrap_or_default();
    let file = config::dir()?.join("redact-patterns.txt");
    if let Ok(text) = std::fs::read_to_string(&file) {
        patterns.extend(text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from));
    }
    Redactor::new(&patterns).with_context(|| format!("loading redaction patterns (env or {})", file.display()))
}

async fn open_store(url: &str, allow_download: bool) -> anyhow::Result<Store> {
    let mut store = Store::connect(url).await?.with_redactor(redactor_from_env()?);
    // Semantic search is opt-in (MEMDUSA_EMBED_URL / MEMDUSA_EMBED_PROVIDER); a broken setup must
    // not stop memdusa from working. Only `embed --download-model` may fetch model files.
    match embedder::from_env(allow_download).await {
        Ok(Some(e)) => store = store.with_embedder(e),
        Ok(None) => {}
        Err(e) => eprintln!("memdusa: semantic search disabled, bad embedding config: {e:#}"),
    }
    Ok(store)
}

/// `memdusa embed` / `POST /v1/embeddings/backfill` result as a line.
fn print_backfill(r: &memdusa_core::embed::BackfillReport) {
    println!("model {}: embedded {}, already up to date {}, failed {}", r.model, r.embedded, r.up_to_date, r.failed);
    if r.failed > 0 {
        println!("some memories could not be embedded (see the warning above); run `memdusa embed` again later");
    }
}

/// The status line about semantic search.
fn semantic_line(model: Option<&str>, embedded: i64, total: i64) -> String {
    match model {
        Some(m) => format!("semantic search: on (model {m}, {embedded}/{total} memories embedded)"),
        None => "semantic search: off (set MEMDUSA_EMBED_URL to enable)".into(),
    }
}

fn default_db_url() -> anyhow::Result<String> {
    let dir = config::dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(memdusa_core::store::sqlite_url(&dir.join("memdusa.db")))
}

fn describe_identity(whoami: &serde_json::Value) -> String {
    let admin = if whoami["admin"].as_bool().unwrap_or(false) { " (admin)" } else { "" };
    match whoami["user"].as_str() {
        Some(u) => format!("{u}{admin}"),
        None => format!("shared token / open server{admin}"),
    }
}

/// Lines printed by `memdusa status` in remote mode. Kept separate from I/O so
/// tests can assert on the "log in again" hint without capturing stdout.
async fn status_lines(r: &config::Remote, project: Option<&str>) -> anyhow::Result<Vec<String>> {
    let mut lines = vec![];
    if let Some(oauth) = &r.oauth {
        match cli_auth::access_token(&r.url, false).await {
            Ok(_) => {
                let fresh = config::remote().ok().flatten().and_then(|r| r.oauth).unwrap_or_else(|| oauth.clone());
                lines.push(format!(
                    "sign-in: OAuth ({}), user {}, expires {}, scopes {}",
                    fresh.method,
                    fresh.user.as_deref().unwrap_or("<unknown>"),
                    expires_in(fresh.expires_at),
                    fresh.scope
                ));
            }
            Err(e) => lines.push(format!("sign-in: {e} — run `memdusa login` again")),
        }
    }
    // Still useful when the server is down: that's when the queue matters.
    match remote::Client::from_config(Duration::from_secs(15))?.whoami().await {
        Ok(info) => {
            lines.push(format!(
                "mode: remote {} (server v{}, {}, reachable)",
                r.url,
                info["version"].as_str().unwrap_or("?"),
                info["backend"].as_str().unwrap_or("?")
            ));
            lines.push(format!("identity: {}", describe_identity(&info)));
            let sem = &info["semantic"];
            lines.push(format!(
                "{} (server side)",
                semantic_line(
                    sem["model"].as_str(),
                    sem["embedded"].as_i64().unwrap_or(0),
                    sem["total"].as_i64().unwrap_or(0)
                )
            ));
        }
        Err(e) => lines.push(format!("mode: remote {} (NOT reachable: {e})", r.url)),
    }
    lines.push(format!("project: {}", project.unwrap_or("<none>")));
    lines.push(format!("offline queue: {} event(s)", spool::Spool::open()?.len()));
    Ok(lines)
}

/// What OAuth login prints: identity and lifetimes, never the secrets.
fn print_login(url: &str, tokens: &config::OAuthTokens) {
    println!("logged in to {url} with OAuth ({})", tokens.method);
    if let Some(user) = &tokens.user {
        println!("identity: {user}");
    }
    println!("scopes: {}", tokens.scope);
    println!("access token expires {}", expires_in(tokens.expires_at));
    println!(
        "saved to {}",
        config::dir()
            .map(|d| d.join("config.json").display().to_string())
            .unwrap_or_else(|_| "~/.memdusa/config.json".into())
    );
}

fn expires_in(ms: i64) -> String {
    let secs = (ms - model::now_ms()).max(0) / 1000;
    match secs {
        0 => "already (will refresh on first use)".into(),
        1..60 => format!("in {secs}s"),
        60..3600 => format!("in {}m", secs / 60),
        _ => format!("in {}h", secs / 3600),
    }
}

fn ts(ms: i64) -> String {
    let secs = (model::now_ms() - ms).max(0) / 1000;
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

fn print_user(u: &User) {
    let mut flags = vec![];
    if u.admin {
        flags.push("admin".to_string());
    }
    if let Some(d) = u.disabled_at {
        flags.push(format!("DISABLED {}", ts(d)));
    }
    println!("{:<20} {}", u.name, flags.join(", "));
}

fn print_key(k: &ApiKey) {
    let state = match (k.revoked_at, k.last_used_at) {
        (Some(r), _) => format!("REVOKED {}", ts(r)),
        (None, Some(u)) => format!("last used {}", ts(u)),
        (None, None) => "never used".into(),
    };
    println!("{:<14} {:<12} {:<14} {state}   id {}", k.prefix, k.user, k.name, k.id);
}

fn print_created(k: &CreatedKey) {
    println!("\nAPI key for {} ({}). Shown once, store it now:\n\n  {}\n", k.key.user, k.key.name, k.secret);
    println!("Use it on a machine with:  memdusa login <server-url> --token {}", k.secret);
}

fn print_grant(g: &ProjectGrant) {
    println!("{:<36} {:<20} {:<6} created by {}  id {}", g.pattern, g.user, g.role.as_str(), g.created_by, g.id);
}

fn print_identity(i: &memdusa_core::oauth::UserIdentity) {
    let state = if i.pending() { "pending" } else { "linked" };
    let handle = i.login.as_deref().or(i.email.as_deref()).unwrap_or_else(|| i.handle());
    println!("{:<8} {:<28} {:<40} {:<8} user {}   id {}", i.provider, handle, i.subject, state, i.user_name, i.id);
}

fn print_session(s: &memdusa_core::oauth::OAuthSession) {
    let state = s.last_used_at.map_or_else(|| "never used".into(), |u| format!("last used {}", ts(u)));
    println!(
        "{:<14} {:<14} {:<20} {:<16} {:<14} {}   id {}",
        s.prefix, s.kind, s.user, s.client_name, s.scope, state, s.id
    );
}

fn print_audit(e: &AuditEvent) {
    let scope = e.project.as_deref().or(e.target.as_deref()).unwrap_or("-");
    println!(
        "{:<12} {:<16} {:<20} {:<7} {:<28} {}",
        ts(e.at),
        e.actor,
        e.action,
        e.outcome.as_str(),
        scope,
        e.detail.as_ref().map(|d| d.to_string()).unwrap_or_default()
    );
}

/// `--since` accepts unix millis or a relative `7d` / `24h` / `2w`.
fn parse_since(s: &str) -> anyhow::Result<i64> {
    let s = s.trim();
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(ms);
    }
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let mult = match unit {
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 604_800_000,
        _ => anyhow::bail!("invalid --since '{s}': use unix millis or a value like 24h / 7d / 2w"),
    };
    let n: i64 = n.parse().context("invalid --since number")?;
    Ok(model::now_ms() - n * mult)
}

fn audit_query(
    since: Option<String>,
    actor: Option<String>,
    project: Option<String>,
    limit: i64,
) -> anyhow::Result<AuditQuery> {
    Ok(AuditQuery { since: since.as_deref().map(parse_since).transpose()?, actor, project, limit: Some(limit) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real repository, so the collector is tested against real git output.
    #[test]
    fn git_hook_collects_commit_facts_from_a_real_repo() {
        let dir = std::env::temp_dir().join(format!("memdusa-git-hook-{}", model::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]).unwrap();
        git(&dir, &["symbolic-ref", "HEAD", "refs/heads/main"]).unwrap();
        git(&dir, &["config", "user.email", "test@example.com"]).unwrap();
        git(&dir, &["config", "user.name", "Test User"]).unwrap();
        git(&dir, &["config", "commit.gpgsign", "false"]).unwrap();
        std::fs::create_dir_all(dir.join(".git/empty-hooks")).unwrap();
        git(&dir, &["config", "core.hooksPath", ".git/empty-hooks"]).unwrap();

        std::fs::write(dir.join("README.md"), "hi\n").unwrap();
        git(&dir, &["add", "README.md"]).unwrap();
        git(&dir, &["commit", "-q", "-m", "Initial commit"]).unwrap();

        // The root commit has no parent: --root must still count its files.
        let root = git_hook_input(install::GitHookKind::Commit, &[], &dir).unwrap().expect("root commit event");
        assert_eq!(root["data"]["subject"], "Initial commit");
        assert_eq!(root["data"]["files"], 1);
        assert_eq!(root["data"]["sha"], git(&dir, &["rev-parse", "HEAD"]).unwrap());

        std::fs::write(dir.join("refunds.rs"), "fn refunds() {}\n").unwrap();
        std::fs::write(dir.join("refunds_test.rs"), "#[test] fn t() {}\n").unwrap();
        git(&dir, &["add", "."]).unwrap();
        git(&dir, &["commit", "-q", "-m", "Add refunds"]).unwrap();

        let input = git_hook_input(install::GitHookKind::Commit, &[], &dir).unwrap().expect("commit event");
        let data = &input["data"];
        assert_eq!(input["git_event"], "commit");
        assert_eq!(data["subject"], "Add refunds");
        assert_eq!(data["branch"], "main");
        assert_eq!(data["files"], 2);
        assert_eq!(data["sha"], git(&dir, &["rev-parse", "HEAD"]).unwrap());
        let cwd = input["cwd"].as_str().expect("cwd is recorded");
        assert_eq!(cwd, git(&dir, &["rev-parse", "--show-toplevel"]).unwrap());
        assert!(Path::new(cwd).is_dir(), "cwd is the repo root");
        assert!(input["content"].as_str().unwrap().contains("Add refunds"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
