//! MCP server: the universal interface. Any MCP-capable harness (Claude Code,
//! Cursor, Codex, Gemini CLI, Windsurf, Zed, ...) connects over stdio or
//! streamable HTTP and gets the same memory, checkpoints and durable runs.

use memdusa_core::model::*;
use memdusa_core::{Store, brief};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const INSTRUCTIONS: &str = "\
memdusa keeps your memory and progress outside this session, so work survives crashes, \
context resets and switching to a different agent or tool.

1. At the START of a session call `resume` and continue from the brief it returns.
2. Call `checkpoint` after meaningful progress and ALWAYS before you stop: goal, done, next, blockers, key files.
3. Call `memory_save` for durable knowledge: user preferences, decisions (with the why), project facts. \
Use `global: true` for things that apply to every project.
4. For multi-step work with side effects (migrations, deploys, batch edits) use a durable run: \
`run_start`, then for each step call `run_step`. If it says `replay`, the step already happened: use the recorded \
output and do NOT redo it. If it says `execute`, do the work, then call `run_step_complete` (or `run_step_fail`). \
Finish with `run_finish`. A run whose owner disappeared shows as STALE in `resume`; take it over with `run_claim`.";

#[derive(Clone)]
pub struct Defaults {
    /// Project used when a tool call doesn't name one (detected from cwd for stdio).
    pub project: Option<String>,
    /// Identifies this agent/harness as the owner of leases and author of records.
    pub agent: String,
    /// Repo directory whose git state is recorded (stdio only; remote callers send it).
    pub cwd: Option<std::path::PathBuf>,
}

#[derive(Clone)]
pub struct MemdusaMcp {
    store: Store,
    defaults: Defaults,
    tool_router: ToolRouter<Self>,
}

type ToolResult = Result<String, String>;

fn json<T: serde::Serialize>(v: T) -> ToolResult {
    serde_json::to_string_pretty(&v).map_err(|e| e.to_string())
}

fn err(e: memdusa_core::Error) -> String {
    e.to_string()
}

// ---------------------------------------------------------------- params

#[derive(Deserialize, JsonSchema)]
pub struct ProjectArg {
    /// Project key (e.g. `github.com/org/repo`). Defaults to the current project.
    #[serde(default)]
    pub project: Option<String>,
    /// Code state, attached automatically by the client (hidden from the model).
    #[serde(default)]
    #[schemars(skip)]
    pub git: Option<GitState>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MemorySaveArgs {
    /// The knowledge to remember, as a self-contained sentence.
    pub content: String,
    /// fact | preference | decision | todo | note
    #[serde(default)]
    pub kind: MemoryKind,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Make this memory visible from every project (e.g. personal preferences).
    #[serde(default)]
    pub global: bool,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MemorySearchArgs {
    /// Words to match. Omit to list the most recent memories.
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub kind: Option<MemoryKind>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct IdArg {
    pub id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct CheckpointArgs {
    /// What we're trying to achieve overall.
    pub goal: String,
    #[serde(default)]
    pub done: Vec<String>,
    /// Concrete next actions, in order.
    #[serde(default)]
    pub next: Vec<String>,
    #[serde(default)]
    pub blockers: Vec<String>,
    /// Files a successor should look at first.
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Optional workstream name when a project has several in flight.
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    /// Code state, attached automatically by the client (hidden from the model).
    #[serde(default)]
    #[schemars(skip)]
    pub git: Option<GitState>,
}

#[derive(Deserialize, JsonSchema)]
pub struct LogArgs {
    /// e.g. message, tool_call, file_change, commit, test_run, note
    pub kind: String,
    pub content: String,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RunStartArgs {
    /// Human-readable name, e.g. "migrate users table".
    pub name: String,
    /// Reuse an existing run with the same key instead of starting a new one.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub input: Option<Value>,
    /// Lease length in seconds (default 600). Each step call renews it.
    #[serde(default)]
    pub lease_secs: Option<i64>,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct StepArgs {
    pub run_id: String,
    /// Stable, deterministic key for this step, e.g. "1-create-table".
    pub step_key: String,
    #[serde(default)]
    pub input: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
pub struct StepCompleteArgs {
    pub run_id: String,
    pub step_key: String,
    /// Result to journal; returned verbatim on replay.
    #[serde(default)]
    pub output: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
pub struct StepFailArgs {
    pub run_id: String,
    pub step_key: String,
    pub error: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct RunFinishArgs {
    pub run_id: String,
    /// completed | failed | cancelled
    pub outcome: RunOutcome,
    #[serde(default)]
    pub output: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RunClaimArgs {
    pub run_id: String,
    /// Take over even if another agent holds a live lease.
    #[serde(default)]
    pub force: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct RunListArgs {
    /// running | completed | failed | cancelled
    #[serde(default)]
    pub status: Option<RunStatus>,
    /// List runs from every project.
    #[serde(default)]
    pub all_projects: bool,
    #[serde(default)]
    pub project: Option<String>,
}

// ---------------------------------------------------------------- tools

impl MemdusaMcp {
    pub fn new(store: Store, defaults: Defaults) -> Self {
        Self { store, defaults, tool_router: Self::tool_router() }
    }

    fn project(&self, ctx: &RequestContext<RoleServer>, p: Option<String>) -> Result<String, String> {
        p.or_else(|| header(ctx, PROJECT_HEADER))
            .or_else(|| self.defaults.project.clone())
            .ok_or_else(|| "no project given and none could be detected; pass `project`".to_string())
    }

    /// Leases and records are attributed to `user/agent` on a multi-user server.
    fn agent_name(&self, ctx: &RequestContext<RoleServer>) -> String {
        identity(ctx).qualify(&header(ctx, AGENT_HEADER).unwrap_or_else(|| self.defaults.agent.clone()))
    }

    /// Only when the call is about the repo this server was started in.
    fn local_git(&self, project: &str) -> Option<GitState> {
        if self.defaults.project.as_deref() != Some(project) {
            return None;
        }
        self.defaults.cwd.as_deref().and_then(crate::project::git_state)
    }

    fn agent(&self, ctx: &RequestContext<RoleServer>) -> Option<String> {
        Some(self.agent_name(ctx))
    }
}

/// Over HTTP, clients (or the `memdusa mcp` proxy) identify themselves and
/// their project with these headers, so tool calls don't need to repeat them.
pub const PROJECT_HEADER: &str = "x-memdusa-project";
pub const AGENT_HEADER: &str = "x-memdusa-agent";

/// Caller identity set by the HTTP auth middleware. Over stdio there is no
/// HTTP request: the local user owns the database, so they get full access.
fn identity(ctx: &RequestContext<RoleServer>) -> Identity {
    ctx.extensions
        .get::<http::request::Parts>()
        .and_then(|p| p.extensions.get::<Identity>().cloned())
        .unwrap_or_else(Identity::root)
}

fn header(ctx: &RequestContext<RoleServer>, name: &str) -> Option<String> {
    let parts = ctx.extensions.get::<http::request::Parts>()?;
    parts.headers.get(name)?.to_str().ok().map(str::trim).filter(|v| !v.is_empty()).map(String::from)
}

#[tool_router]
impl MemdusaMcp {
    #[tool(
        description = "Get the hand-off brief for a project: latest checkpoints, unfinished durable runs, memories and recent activity. Call this first in every session."
    )]
    async fn resume(&self, Parameters(a): Parameters<ProjectArg>, ctx: RequestContext<RoleServer>) -> ToolResult {
        let project = self.project(&ctx, a.project)?;
        let git = a.git.or_else(|| self.local_git(&project));
        Ok(brief::build(&self.store, &project, identity(&ctx).user.as_deref(), git.as_ref())
            .await
            .map_err(err)?
            .markdown)
    }

    #[tool(
        description = "Remember a fact, preference, decision or todo across sessions and tools. Identical memories are deduplicated."
    )]
    async fn memory_save(
        &self,
        Parameters(a): Parameters<MemorySaveArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let project = if a.global { None } else { Some(self.project(&ctx, a.project)?) };
        let m = NewMemory {
            project,
            kind: a.kind,
            content: a.content,
            tags: a.tags,
            source: self.agent(&ctx),
            user: identity(&ctx).user,
        };
        json(self.store.memory_save(m).await.map_err(err)?)
    }

    #[tool(description = "Search memories (project + global) by words, optionally filtered by kind.")]
    async fn memory_search(
        &self,
        Parameters(a): Parameters<MemorySearchArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let q = MemoryQuery {
            query: a.query,
            project: Some(self.project(&ctx, a.project)?),
            kind: a.kind,
            limit: a.limit,
            user: identity(&ctx).user,
        };
        json(self.store.memory_search(q).await.map_err(err)?)
    }

    #[tool(description = "Delete a memory that is wrong or obsolete.")]
    async fn memory_forget(&self, Parameters(a): Parameters<IdArg>) -> ToolResult {
        self.store.memory_forget(&a.id).await.map_err(err)?;
        Ok(format!("forgot {}", a.id))
    }

    #[tool(
        description = "Save a progress checkpoint so any agent can resume this work later. Call after meaningful progress and before ending a session."
    )]
    async fn checkpoint(
        &self,
        Parameters(a): Parameters<CheckpointArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let project = self.project(&ctx, a.project)?;
        let git = a.git.or_else(|| self.local_git(&project));
        let c = NewCheckpoint {
            project,
            task: a.task,
            goal: a.goal,
            done: a.done,
            next: a.next,
            blockers: a.blockers,
            files: a.files,
            notes: a.notes,
            source: self.agent(&ctx),
            git,
        };
        let cp = self.store.checkpoint_save(c).await.map_err(err)?;
        Ok(format!("checkpoint {} saved", cp.id))
    }

    #[tool(
        description = "Append an entry to the project's activity journal (notable actions, commits, test results, discoveries)."
    )]
    async fn log_event(&self, Parameters(a): Parameters<LogArgs>, ctx: RequestContext<RoleServer>) -> ToolResult {
        let e = NewEvent {
            project: self.project(&ctx, a.project)?,
            session: a.session,
            source: self.agent(&ctx),
            kind: a.kind,
            content: a.content,
            data: a.data,
        };
        let ev = self.store.event_append(e).await.map_err(err)?;
        Ok(format!("logged {}", ev.id))
    }

    #[tool(
        description = "Start a durable multi-step run (or re-enter an existing one via idempotency_key). Returns the run with its journaled steps."
    )]
    async fn run_start(&self, Parameters(a): Parameters<RunStartArgs>, ctx: RequestContext<RoleServer>) -> ToolResult {
        let r = NewRun {
            project: self.project(&ctx, a.project)?,
            name: a.name,
            idempotency_key: a.idempotency_key,
            input: a.input,
            owner: self.agent(&ctx),
            lease_secs: a.lease_secs,
        };
        json(self.store.run_start(r).await.map_err(err)?)
    }

    #[tool(
        description = "Enter a step of a durable run. Returns {action: \"replay\", output} if the step already completed (do NOT redo it), or {action: \"execute\", attempt} if you must perform it now and then call run_step_complete / run_step_fail."
    )]
    async fn run_step(&self, Parameters(a): Parameters<StepArgs>, ctx: RequestContext<RoleServer>) -> ToolResult {
        json(self.store.step_begin(&a.run_id, &a.step_key, a.input, &self.agent_name(&ctx)).await.map_err(err)?)
    }

    #[tool(description = "Record the result of a step you executed. The output is journaled and replayed on resume.")]
    async fn run_step_complete(&self, Parameters(a): Parameters<StepCompleteArgs>) -> ToolResult {
        json(self.store.step_complete(&a.run_id, &a.step_key, a.output).await.map_err(err)?)
    }

    #[tool(description = "Record that a step failed. Calling run_step again for it retries it.")]
    async fn run_step_fail(&self, Parameters(a): Parameters<StepFailArgs>) -> ToolResult {
        json(self.store.step_fail(&a.run_id, &a.step_key, &a.error).await.map_err(err)?)
    }

    #[tool(description = "Finish a durable run as completed, failed or cancelled, releasing its lease.")]
    async fn run_finish(&self, Parameters(a): Parameters<RunFinishArgs>) -> ToolResult {
        json(self.store.run_finish(&a.run_id, a.outcome, a.output, a.error).await.map_err(err)?)
    }

    #[tool(
        description = "Take over a durable run (e.g. a STALE one left by a crashed agent or another tool). Returns the run and its step journal."
    )]
    async fn run_claim(&self, Parameters(a): Parameters<RunClaimArgs>, ctx: RequestContext<RoleServer>) -> ToolResult {
        json(self.store.run_claim(&a.run_id, &self.agent_name(&ctx), None, a.force).await.map_err(err)?)
    }

    #[tool(description = "Get a durable run with its full step journal.")]
    async fn run_get(&self, Parameters(a): Parameters<IdArg>) -> ToolResult {
        json(self.store.run_get(&a.id).await.map_err(err)?)
    }

    #[tool(description = "List durable runs for the project (or all projects), newest first.")]
    async fn run_list(&self, Parameters(a): Parameters<RunListArgs>, ctx: RequestContext<RoleServer>) -> ToolResult {
        let project = if a.all_projects { None } else { Some(self.project(&ctx, a.project)?) };
        json(self.store.run_list(project.as_deref(), a.status, 50).await.map_err(err)?)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MemdusaMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("memdusa", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
