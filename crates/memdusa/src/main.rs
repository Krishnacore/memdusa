mod adapters;
mod config;
mod hooks;
mod http;
mod install;
mod mcp;
mod ops;
mod project;
mod proxy;
mod remote;
mod spool;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use memdusa_core::model::{self, ApiKey, CreatedKey, MemoryKind, MemoryQuery, NewMemory, User};
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
    /// Print the detected project key.
    Project,
    /// Use a remote memdusa server for hooks, MCP and the CLI on this machine.
    Login {
        /// Server URL, e.g. https://memdusa.example.com
        url: String,
        /// Bearer token (as set with `memdusa serve --token`).
        #[arg(long, env = "MEMDUSA_TOKEN", hide_env_values = true)]
        token: Option<String>,
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

fn print_import(r: &transfer::ImportReport) {
    println!(
        "imported {} memories, {} events, {} checkpoints, {} runs ({} steps); skipped {} existing",
        r.memories, r.events, r.checkpoints, r.runs, r.steps, r.skipped
    );
}

fn print_prune(r: &memdusa_core::retention::PruneReport) {
    let verb = if r.dry_run { "would" } else { "did" };
    println!(
        "{verb} summarize {} session(s) ({} activity events), delete {} checkpoint(s), {} run(s) ({} steps)",
        r.sessions_summarized, r.events_deleted, r.checkpoints_deleted, r.runs_deleted, r.steps_deleted
    );
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
                mcp: !no_mcp,
                dry_run: false,
                uninstall: true,
            });
        }
        Cmd::Hook { event, harness, agent, no_prompts, stop_guard_mins } => {
            // A hook must never break the user's session: log and exit 0.
            let agent = agent.unwrap_or_else(|| harness.agent().into());
            let cfg = hooks::HookConfig { agent, journal_prompts: !no_prompts, stop_guard_mins };
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
        Cmd::Login { url, token } => {
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
    let store = match &mode {
        Mode::Remote(r) => return run_remote(cli.cmd, r, project).await,
        Mode::Local(url) => open_store(url).await.with_context(|| format!("connecting to {url}"))?,
    };

    match cli.cmd {
        Cmd::Status => {
            let Mode::Local(url) = &mode else { unreachable!() };
            println!("mode: local database {url} (schema ok, {} users)", store.user_count().await?);
            println!("project: {}", project.as_deref().unwrap_or("<none>"));
        }
        Cmd::Sync => println!("local mode: nothing to sync (the queue is only used with a remote server)"),
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
        },
        Cmd::Key { cmd } => match cmd {
            KeyCmd::Create { user, name } => {
                let user = user.context("local mode: pass --user <name>")?;
                print_created(&store.key_create(&user, &name).await?);
            }
            KeyCmd::List { user } => store.key_list(user.as_deref()).await?.iter().for_each(print_key),
            KeyCmd::Revoke { key } => print_key(&store.key_revoke(&key, None).await?),
        },
        Cmd::Mcp { agent } => {
            tracing::info!(?project, agent, "memdusa MCP server on stdio");
            let server =
                mcp::MemdusaMcp::new(store, mcp::Defaults { project, agent, cwd: std::env::current_dir().ok() });
            let running = server.serve(rmcp::transport::stdio()).await?;
            running.waiting().await?;
        }
        Cmd::Serve { addr, token, allowed_hosts, no_auto_prune, rate_limit, no_metrics } => {
            let auto_prune = (!no_auto_prune).then(retention_from_env);
            let opts = http::HttpOptions { addr, token, allowed_hosts, auto_prune, rate_limit, metrics: !no_metrics };
            http::serve(store, opts).await?;
        }
        Cmd::Prune { dry_run, only_project, policy } => {
            print_prune(&store.prune(&policy.into(), only_project.as_deref(), dry_run).await?);
        }
        Cmd::Resume { json } => {
            let project = project.context("could not detect project; pass --project")?;
            let b = brief::build(&store, &project, None, project::git_state_for(&project).as_ref()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&b)?);
            } else {
                print!("{}", b.markdown);
            }
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
            println!("saved {}", m.id);
        }
        Cmd::Recall { query } => {
            for m in store.memory_search(MemoryQuery { query, project, ..Default::default() }).await? {
                let scope = m.project.as_deref().unwrap_or("global");
                println!("[{}] ({}) {}  — {}", m.kind.as_str(), scope, m.content, m.id);
            }
        }
        Cmd::Project
        | Cmd::Hook { .. }
        | Cmd::Install { .. }
        | Cmd::Uninstall { .. }
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

async fn run_remote(cmd: Cmd, r: &config::Remote, project: Option<String>) -> anyhow::Result<()> {
    let client = || remote::Client::new(r.clone(), Duration::from_secs(15));
    match cmd {
        Cmd::Status => {
            // Still useful when the server is down: that's when the queue matters.
            match client()?.whoami().await {
                Ok(info) => {
                    println!(
                        "mode: remote {} (server v{}, {}, reachable)",
                        r.url,
                        info["version"].as_str().unwrap_or("?"),
                        info["backend"].as_str().unwrap_or("?")
                    );
                    println!("identity: {}", describe_identity(&info));
                }
                Err(e) => println!("mode: remote {} (NOT reachable: {e})", r.url),
            }
            println!("project: {}", project.as_deref().unwrap_or("<none>"));
            println!("offline queue: {} event(s)", spool::Spool::open()?.len());
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
        Cmd::Mcp { agent } => proxy::run(r, project, &agent).await?,
        Cmd::Resume { json } => {
            let project = project.context("could not detect project; pass --project")?;
            let b = client()?.resume(&project, project::git_state_for(&project).as_ref()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&b)?);
            } else {
                print!("{}", b["markdown"].as_str().unwrap_or_default());
            }
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
            println!("saved {}", client()?.memory_save(&m).await?.id);
        }
        Cmd::Recall { query } => {
            for m in client()?.memory_search(&MemoryQuery { query, project, ..Default::default() }).await? {
                let scope = m.project.as_deref().unwrap_or("global");
                println!("[{}] ({}) {}  — {}", m.kind.as_str(), scope, m.content, m.id);
            }
        }
        _ => unreachable!("handled before mode selection"),
    }
    Ok(())
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
    // Translate the harness's payload into memdusa's canonical one (or ignore this event).
    let Some(mut input) = harness.normalize(event, native) else { return Ok(None) };

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
    let mut req = hooks::HookRequest { project, config, input, git, at: Some(model::now_ms()) };

    let resp = match Mode::detect(database_url)? {
        Mode::Remote(r) => {
            // Redact on this machine too, so secrets never leave it (the server redacts again).
            req.input = redactor_from_env()?.json(req.input);
            remote_hook(event, req, &r, deadline).await?
        }
        Mode::Local(url) => {
            let store = open_store(&url).await?;
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
        let flush_client = remote::Client::new(r.clone(), (deadline / 3).min(Duration::from_secs(2)))?;
        match spool.flush(&flush_client, &r.url).await {
            Ok(n) if n > 0 => eprintln!("memdusa: synced {n} queued event(s)"),
            Ok(_) => {}
            Err(e) if e.retryable => unreachable = Some(e),
            Err(e) => eprintln!("memdusa: could not sync the offline queue: {e}"),
        }
    }
    let result = match unreachable {
        Some(e) => Err(e), // just failed to connect: don't wait for another timeout
        None => remote::Client::new(r.clone(), deadline)?.hook(event, &req).await,
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
                eprintln!("memdusa: server unreachable, queued {} for later ({e})", event.arg());
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

async fn open_store(url: &str) -> anyhow::Result<Store> {
    Ok(Store::connect(url).await?.with_redactor(redactor_from_env()?))
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
