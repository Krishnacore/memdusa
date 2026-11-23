//! MCP server: the universal interface. Any MCP-capable harness (Claude Code,
//! Cursor, Codex, Gemini CLI, Windsurf, Zed, ...) connects over stdio or
//! streamable HTTP and gets the same memory, checkpoints and durable runs.

use std::sync::Arc;

use memdusa_core::model::*;
use memdusa_core::{Store, brief};

use crate::http::AccessCache;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{
    GetPromptRequestParams, GetPromptResponse, GetPromptResult, Implementation, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams, Prompt, PromptArgument, PromptMessage,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, ResourceTemplate,
    Role, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};
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
Finish with `run_finish`. A run whose owner disappeared shows as STALE in `resume`; take it over with `run_claim`.
5. To split work between agents, call `delegate` with the task names and launch each worker with the env line it \
returns. A worker finishes with `task_complete` (or `task_fail`); `resume` shows every task's status.";

#[derive(Clone)]
pub struct Defaults {
    /// Project used when a tool call doesn't name one (detected from cwd for stdio).
    pub project: Option<String>,
    /// Identifies this agent/harness as the owner of leases and author of records.
    pub agent: String,
    /// Repo directory whose git state is recorded (stdio only; remote callers send it).
    pub cwd: Option<std::path::PathBuf>,
    /// `MEMDUSA_TASK` (stdio): this agent is a worker on that delegated task. Over HTTP the
    /// `memdusa mcp` proxy sends it as the `task` argument instead.
    pub task: Option<String>,
    /// `MEMDUSA_RUN` (stdio): the delegated run `task` is a step of.
    pub run: Option<String>,
}

#[derive(Clone)]
pub struct MemdusaMcp {
    store: Store,
    defaults: Defaults,
    /// Sync mode: memory tools use `store`; runs/steps and delegated tasks go
    /// to the server through this client.
    remote: Option<Arc<crate::remote::Client>>,
    tool_router: ToolRouter<Self>,
}

type ToolResult = Result<String, String>;

fn json<T: serde::Serialize>(v: T) -> ToolResult {
    serde_json::to_string_pretty(&v).map_err(|e| e.to_string())
}

fn err(e: memdusa_core::Error) -> String {
    e.to_string()
}

// ---------------------------------------------------------------- lenient list params

/// Deserialize a `Vec<String>` from either a JSON array of strings or a single string. A string is
/// split on newlines, trimmed and has empty items dropped, so a model that sends multiline plain
/// text instead of an array still works. Schemars reads the Rust type, so the advertised JSON schema
/// stays an array.
fn string_list<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Vec<String>, D::Error> {
    use serde::de::{self, Visitor};
    use std::fmt;

    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a list of strings or a single string")
        }
        fn visit_str<E: de::Error>(self, s: &str) -> std::result::Result<Self::Value, E> {
            Ok(split_lines(s))
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(item) = seq.next_element::<String>()? {
                out.push(item);
            }
            Ok(out)
        }
    }
    d.deserialize_any(V)
}

/// [`string_list`] for `Option<Vec<String>>` fields (null stays `None`).
fn opt_string_list<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Vec<String>>, D::Error> {
    use serde::de::{self, Visitor};
    use std::fmt;

    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<Vec<String>>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a list of strings, a single string, or null")
        }
        fn visit_none<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> std::result::Result<Self::Value, D::Error> {
            string_list(d).map(Some)
        }
    }
    d.deserialize_option(V)
}

fn split_lines(s: &str) -> Vec<String> {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

// ---------------------------------------------------------------- params

#[derive(Deserialize, JsonSchema)]
pub struct ProjectArg {
    /// Project key (e.g. `github.com/org/repo`). Defaults to the current project.
    #[serde(default)]
    pub project: Option<String>,
    /// Delegated task name: brief that task only (checkpoints and last words of its workers). Defaults to
    /// the `MEMDUSA_TASK` this agent was launched with; omit it as a coordinator to see all delegated work.
    #[serde(default)]
    pub task: Option<String>,
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
    #[serde(default, deserialize_with = "string_list")]
    pub tags: Vec<String>,
    /// Make this memory visible from every project (e.g. personal preferences).
    #[serde(default)]
    pub global: bool,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MemoryUpdateArgs {
    /// Id of the memory to change.
    pub id: String,
    /// The corrected knowledge, replacing the old content.
    #[serde(default)]
    pub content: Option<String>,
    /// fact | preference | decision | todo | note
    #[serde(default)]
    pub kind: Option<MemoryKind>,
    /// Replaces the tags.
    #[serde(default, deserialize_with = "opt_string_list")]
    pub tags: Option<Vec<String>>,
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
pub struct MemoryReviewArgs {
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
    #[serde(default, deserialize_with = "string_list")]
    pub done: Vec<String>,
    /// Concrete next actions, in order.
    #[serde(default, deserialize_with = "string_list")]
    pub next: Vec<String>,
    #[serde(default, deserialize_with = "string_list")]
    pub blockers: Vec<String>,
    /// Files a successor should look at first.
    #[serde(default, deserialize_with = "string_list")]
    pub files: Vec<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Optional workstream name when a project has several in flight. Defaults to the delegated task
    /// (`MEMDUSA_TASK`) this agent was launched with.
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
pub struct DelegateArgs {
    /// Name of the piece of work being split up, e.g. "windows release".
    pub name: String,
    /// One short, stable name per task (e.g. "install-scripts"): it becomes the worker's `MEMDUSA_TASK`.
    #[serde(deserialize_with = "string_list")]
    pub tasks: Vec<String>,
    /// Calling again with the same key (or, without one, the same name) reuses the run and adds missing tasks.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaskCompleteArgs {
    /// What was done and where to find it (short): shown to the coordinator and to anyone who starts this task again.
    #[serde(default)]
    pub summary: Option<String>,
    /// Defaults to the `MEMDUSA_RUN` this worker was launched with.
    #[serde(default)]
    pub run: Option<String>,
    /// Defaults to the `MEMDUSA_TASK` this worker was launched with.
    #[serde(default)]
    pub task: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaskFailArgs {
    /// Why the task can't be finished.
    pub reason: String,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
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
        Self { store, defaults, remote: None, tool_router: Self::tool_router() }
    }

    /// Route delegated work to a server (sync mode).
    pub fn with_remote(mut self, client: crate::remote::Client) -> Self {
        self.remote = Some(Arc::new(client));
        self
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

    /// The explicit task, else the one this worker was launched with. Blank counts as none.
    fn task(&self, explicit: Option<String>) -> Option<String> {
        explicit.or_else(|| self.defaults.task.clone()).map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
    }

    /// The (run, task) a worker's `task_complete` / `task_fail` is about: explicit, else the launch environment.
    fn run_task(&self, run: Option<String>, task: Option<String>) -> Result<(String, String), String> {
        let run = run.or_else(|| self.defaults.run.clone()).map(|r| r.trim().to_owned()).filter(|r| !r.is_empty());
        match (run, self.task(task)) {
            (Some(r), Some(t)) => Ok((r, t)),
            _ => Err("no delegated task: this agent wasn't launched with MEMDUSA_RUN and MEMDUSA_TASK; pass `run` and `task`"
                .into()),
        }
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

fn cache(ctx: &RequestContext<RoleServer>) -> Option<&AccessCache> {
    ctx.extensions.get::<http::request::Parts>()?.extensions.get::<AccessCache>()
}

/// Remote MCP calls carry the same per-request access snapshot as REST. Over
/// stdio there is no HTTP request: the local user owns the database (root).
async fn authorize(
    ctx: &RequestContext<RoleServer>,
    project: &str,
    needed: Access,
    action: &str,
) -> Result<(), String> {
    match cache(ctx) {
        Some(c) => c.authorize(&identity(ctx), project, needed, action).await.map_err(err),
        None => Ok(()),
    }
}

async fn readable_projects(store: &Store, ctx: &RequestContext<RoleServer>) -> Result<Vec<String>, String> {
    match cache(ctx) {
        Some(c) => c.readable_projects(store, &identity(ctx)).await.map_err(err),
        None => store.readable_projects(&identity(ctx), DefaultPolicy::Write).await.map_err(err),
    }
}

async fn authorize_run(
    store: &Store,
    ctx: &RequestContext<RoleServer>,
    run: &str,
    needed: Access,
    action: &str,
) -> Result<String, String> {
    let project = store.run_project(run).await.map_err(err)?;
    authorize(ctx, &project, needed, action).await?;
    Ok(project)
}

async fn authorize_memory(
    store: &Store,
    ctx: &RequestContext<RoleServer>,
    id: &str,
    action: &str,
) -> Result<(), String> {
    if let Some(project) = store.memory_project(id).await.map_err(err)? {
        authorize(ctx, &project, Access::Write, action).await?;
    }
    Ok(())
}

#[tool_router]
impl MemdusaMcp {
    #[tool(
        description = "Get the hand-off brief for a project: latest checkpoints, unfinished durable runs, memories and recent activity. Call this first in every session."
    )]
    async fn resume(&self, Parameters(a): Parameters<ProjectArg>, ctx: RequestContext<RoleServer>) -> ToolResult {
        let project = self.project(&ctx, a.project)?;
        authorize(&ctx, &project, Access::Read, "resume").await?;
        let git = a.git.or_else(|| self.local_git(&project));
        let task = self.task(a.task);
        Ok(brief::build(&self.store, &project, identity(&ctx).user.as_deref(), git.as_ref(), task.as_deref())
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
        if let Some(project) = &project {
            authorize(&ctx, project, Access::Write, "memory.save").await?;
        }
        let m = NewMemory {
            project,
            kind: a.kind,
            content: a.content,
            tags: a.tags,
            source: self.agent(&ctx),
            user: identity(&ctx).user,
        };
        let saved = self.store.memory_save(m).await.map_err(err)?;
        if saved.merged {
            let note = format!("updated existing memory {} (near-duplicate)\n", saved.id);
            return Ok(note + &json(saved)?);
        }
        json(saved)
    }

    #[tool(
        description = "Correct or refine an existing memory in place (by id) instead of saving a near-copy. Only the fields you pass change."
    )]
    async fn memory_update(
        &self,
        Parameters(a): Parameters<MemoryUpdateArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        authorize_memory(&self.store, &ctx, &a.id, "memory.update").await?;
        let patch = MemoryPatch { content: a.content, kind: a.kind, tags: a.tags, source: self.agent(&ctx) };
        json(self.store.memory_update_as(&a.id, patch, &identity(&ctx)).await.map_err(err)?)
    }

    #[tool(description = "Search memories (project + global) by words, optionally filtered by kind.")]
    async fn memory_search(
        &self,
        Parameters(a): Parameters<MemorySearchArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let project = self.project(&ctx, a.project)?;
        authorize(&ctx, &project, Access::Read, "memory.search").await?;
        let q = MemoryQuery {
            query: a.query,
            project: Some(project),
            kind: a.kind,
            limit: a.limit,
            user: identity(&ctx).user,
            allowed_projects: None,
        };
        json(self.store.memory_search(q).await.map_err(err)?)
    }

    #[tool(
        description = "Find memories that need consolidating. Use periodically, or when the brief says memories need review. Lists related memories to merge, memories that mention files changed since they were written, and old todos, each with ids. Then: merge each related group with memory_update on one memory and memory_forget on the rest; verify stale ones against the code (memory_update or memory_forget); close old todos."
    )]
    async fn memory_review(
        &self,
        Parameters(a): Parameters<MemoryReviewArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let project = self.project(&ctx, a.project)?;
        authorize(&ctx, &project, Access::Read, "memory.review").await?;
        let review = self.store.memory_review(&project, identity(&ctx).user.as_deref()).await.map_err(err)?;
        Ok(review.markdown(&project, memdusa_core::model::now_ms()))
    }

    #[tool(description = "Delete a memory that is wrong or obsolete.")]
    async fn memory_forget(&self, Parameters(a): Parameters<IdArg>, ctx: RequestContext<RoleServer>) -> ToolResult {
        authorize_memory(&self.store, &ctx, &a.id, "memory.forget").await?;
        let project = self.store.memory_project(&a.id).await.map_err(err)?.unwrap_or_default();
        self.store
            .audit(NewAuditEvent::ok(identity(&ctx).actor(), "memory.forget").project(&project).target(&a.id))
            .await
            .map_err(err)?;
        self.store.memory_forget_as(&a.id, &identity(&ctx)).await.map_err(err)?;
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
        authorize(&ctx, &project, Access::Write, "checkpoint").await?;
        let git = a.git.or_else(|| self.local_git(&project));
        let c = NewCheckpoint {
            project,
            task: self.task(a.task),
            goal: a.goal,
            done: a.done,
            next: a.next,
            blockers: a.blockers,
            files: a.files,
            notes: a.notes,
            source: self.agent(&ctx),
            git,
            created_at: None,
        };
        let cp = self.store.checkpoint_save(c).await.map_err(err)?;
        Ok(format!("checkpoint {} saved", cp.id))
    }

    #[tool(
        description = "Append an entry to the project's activity journal (notable actions, commits, test results, discoveries)."
    )]
    async fn log_event(&self, Parameters(a): Parameters<LogArgs>, ctx: RequestContext<RoleServer>) -> ToolResult {
        let project = self.project(&ctx, a.project)?;
        authorize(&ctx, &project, Access::Write, "log_event").await?;
        let e = NewEvent {
            project,
            session: a.session,
            source: self.agent(&ctx),
            kind: a.kind,
            content: a.content,
            data: a.data,
            created_at: None,
        };
        let ev = self.store.event_append(e).await.map_err(err)?;
        Ok(format!("logged {}", ev.id))
    }

    #[tool(
        description = "Start a durable multi-step run (or re-enter an existing one via idempotency_key). Returns the run with its journaled steps."
    )]
    async fn run_start(&self, Parameters(a): Parameters<RunStartArgs>, ctx: RequestContext<RoleServer>) -> ToolResult {
        let project = self.project(&ctx, a.project)?;
        if let Some(remote) = &self.remote {
            let r = NewRun {
                project,
                name: a.name,
                idempotency_key: a.idempotency_key,
                input: a.input,
                owner: self.agent(&ctx),
                lease_secs: a.lease_secs,
            };
            return json(remote.run_start(&r).await.map_err(|e| e.to_string())?);
        }
        authorize(&ctx, &project, Access::Write, "run.start").await?;
        let r = NewRun {
            project,
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
        if let Some(remote) = &self.remote {
            let d = remote
                .step_begin(&a.run_id, &a.step_key, a.input, &self.agent_name(&ctx))
                .await
                .map_err(|e| e.to_string())?;
            return json(d);
        }
        authorize_run(&self.store, &ctx, &a.run_id, Access::Write, "run.step").await?;
        json(self.store.step_begin(&a.run_id, &a.step_key, a.input, &self.agent_name(&ctx)).await.map_err(err)?)
    }

    #[tool(description = "Record the result of a step you executed. The output is journaled and replayed on resume.")]
    async fn run_step_complete(
        &self,
        Parameters(a): Parameters<StepCompleteArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        if let Some(remote) = &self.remote {
            let step = remote.step_complete(&a.run_id, &a.step_key, a.output).await.map_err(|e| e.to_string())?;
            return json(step);
        }
        authorize_run(&self.store, &ctx, &a.run_id, Access::Write, "run.step_complete").await?;
        json(self.store.step_complete(&a.run_id, &a.step_key, a.output).await.map_err(err)?)
    }

    #[tool(description = "Record that a step failed. Calling run_step again for it retries it.")]
    async fn run_step_fail(
        &self,
        Parameters(a): Parameters<StepFailArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        if let Some(remote) = &self.remote {
            let step = remote.step_fail(&a.run_id, &a.step_key, &a.error).await.map_err(|e| e.to_string())?;
            return json(step);
        }
        authorize_run(&self.store, &ctx, &a.run_id, Access::Write, "run.step_fail").await?;
        json(self.store.step_fail(&a.run_id, &a.step_key, &a.error).await.map_err(err)?)
    }

    #[tool(description = "Finish a durable run as completed, failed or cancelled, releasing its lease.")]
    async fn run_finish(
        &self,
        Parameters(a): Parameters<RunFinishArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        if let Some(remote) = &self.remote {
            let r = remote.run_finish(&a.run_id, a.outcome, a.output, a.error).await.map_err(|e| e.to_string())?;
            return json(r);
        }
        authorize_run(&self.store, &ctx, &a.run_id, Access::Write, "run.finish").await?;
        json(self.store.run_finish(&a.run_id, a.outcome, a.output, a.error).await.map_err(err)?)
    }

    #[tool(
        description = "Take over a durable run (e.g. a STALE one left by a crashed agent or another tool). Returns the run and its step journal."
    )]
    async fn run_claim(&self, Parameters(a): Parameters<RunClaimArgs>, ctx: RequestContext<RoleServer>) -> ToolResult {
        if let Some(remote) = &self.remote {
            let r =
                remote.run_claim(&a.run_id, &self.agent_name(&ctx), None, a.force).await.map_err(|e| e.to_string())?;
            return json(r);
        }
        authorize_run(&self.store, &ctx, &a.run_id, Access::Write, "run.claim").await?;
        json(self.store.run_claim(&a.run_id, &self.agent_name(&ctx), None, a.force).await.map_err(err)?)
    }

    #[tool(
        description = "Coordinator: split work into tasks for other agents (any harness). Creates one durable run with a pending step per task and returns, per task, the env line (MEMDUSA_RUN=... MEMDUSA_TASK=...) to launch its worker with. Workers claim their task with a lease; a crashed worker's task goes stale and a replacement takes it over; finished tasks are never redone. `resume` shows each task's status."
    )]
    async fn delegate(&self, Parameters(a): Parameters<DelegateArgs>, ctx: RequestContext<RoleServer>) -> ToolResult {
        let project = self.project(&ctx, a.project)?;
        if let Some(remote) = &self.remote {
            let run = remote
                .delegate(&project, &a.name, &a.tasks, a.idempotency_key.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            return Ok(brief::delegate_report(&run));
        }
        authorize(&ctx, &project, Access::Write, "delegate").await?;
        let run = self.store.run_delegate(&project, &a.name, a.tasks, a.idempotency_key).await.map_err(err)?;
        Ok(brief::delegate_report(&run))
    }

    #[tool(
        description = "Worker: mark your delegated task (MEMDUSA_RUN / MEMDUSA_TASK) as done, with a short summary of what was done and where. Do this once, when the task is finished."
    )]
    async fn task_complete(
        &self,
        Parameters(a): Parameters<TaskCompleteArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> ToolResult {
        let (run, task) = self.run_task(a.run, a.task)?;
        let output = a.summary.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).map(Value::String);
        if let Some(remote) = &self.remote {
            remote.step_complete(&run, &task, output).await.map_err(|e| e.to_string())?;
            return Ok(format!("task {task} marked done"));
        }
        authorize_run(&self.store, &ctx, &run, Access::Write, "task.complete").await?;
        self.store.step_complete(&run, &task, output).await.map_err(err)?;
        Ok(format!("task {task} marked done"))
    }

    #[tool(
        description = "Worker: give up on your delegated task (MEMDUSA_RUN / MEMDUSA_TASK) with the reason. A replacement worker can claim it again."
    )]
    async fn task_fail(&self, Parameters(a): Parameters<TaskFailArgs>, ctx: RequestContext<RoleServer>) -> ToolResult {
        let (run, task) = self.run_task(a.run, a.task)?;
        if let Some(remote) = &self.remote {
            remote.step_fail(&run, &task, &a.reason).await.map_err(|e| e.to_string())?;
            return Ok(format!("task {task} marked failed"));
        }
        authorize_run(&self.store, &ctx, &run, Access::Write, "task.fail").await?;
        self.store.step_fail(&run, &task, &a.reason).await.map_err(err)?;
        Ok(format!("task {task} marked failed"))
    }

    #[tool(description = "Get a durable run with its full step journal.")]
    async fn run_get(&self, Parameters(a): Parameters<IdArg>, ctx: RequestContext<RoleServer>) -> ToolResult {
        if let Some(remote) = &self.remote {
            return json(remote.run_get(&a.id).await.map_err(|e| e.to_string())?);
        }
        authorize_run(&self.store, &ctx, &a.id, Access::Read, "run.get").await?;
        json(self.store.run_get(&a.id).await.map_err(err)?)
    }

    #[tool(description = "List durable runs for the project (or all projects), newest first.")]
    async fn run_list(&self, Parameters(a): Parameters<RunListArgs>, ctx: RequestContext<RoleServer>) -> ToolResult {
        if let Some(remote) = &self.remote {
            let project = if a.all_projects { None } else { Some(self.project(&ctx, a.project)?) };
            return json(remote.run_list(project.as_deref(), a.status).await.map_err(|e| e.to_string())?);
        }
        let (project, allowed) = if a.all_projects {
            (None, Some(readable_projects(&self.store, &ctx).await?))
        } else {
            let project = self.project(&ctx, a.project)?;
            authorize(&ctx, &project, Access::Read, "run.list").await?;
            (Some(project), None)
        };
        json(self.store.run_list_visible(project.as_deref(), allowed.as_deref(), a.status, 50).await.map_err(err)?)
    }
}

const BRIEF_URI: &str = "memdusa://brief";
const BRIEF_TEMPLATE: &str = "memdusa://brief/{project}";
const BRIEF_PREFIX: &str = "memdusa://brief/";
const MEMORIES_URI: &str = "memdusa://memories";
const MARKDOWN: &str = "text/markdown";

const RESUME_INSTRUCTION: &str =
    "Continue from this brief: verify the code state first, then carry on with the next steps.";
const CHECKPOINT_PROMPT: &str = "\
Save a memdusa checkpoint now with the `checkpoint` tool: the overall goal, what is done, the concrete next steps, \
any blockers, and the key files a successor should look at first. \
Then call `memory_save` for every decision worth keeping (with the why), and for durable user preferences or project facts.";

/// Decode `%XX` escapes in a URI path segment.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

impl MemdusaMcp {
    /// The same brief the `resume` tool returns.
    async fn brief_markdown(
        &self,
        ctx: &RequestContext<RoleServer>,
        project: Option<String>,
    ) -> Result<String, McpError> {
        let project = self.project(ctx, project).map_err(|e| McpError::invalid_params(e, None))?;
        authorize(ctx, &project, Access::Read, "brief").await.map_err(|e| McpError::internal_error(e, None))?;
        let git = self.local_git(&project);
        let task = self.task(None);
        Ok(brief::build(&self.store, &project, identity(ctx).user.as_deref(), git.as_ref(), task.as_deref())
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .markdown)
    }

    async fn memories_markdown(&self, ctx: &RequestContext<RoleServer>) -> Result<String, McpError> {
        let project = self.project(ctx, None).map_err(|e| McpError::invalid_params(e, None))?;
        authorize(ctx, &project, Access::Read, "memories").await.map_err(|e| McpError::internal_error(e, None))?;
        let memories = self
            .store
            .memory_search(MemoryQuery {
                project: Some(project.clone()),
                limit: Some(200),
                user: identity(ctx).user,
                ..Default::default()
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(brief::memories_markdown(&project, &memories))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MemdusaMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_resources().enable_prompts().build())
            .with_server_info(Implementation::new("memdusa", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new(BRIEF_URI, "brief")
                .with_title("Resume brief")
                .with_description(
                    "Hand-off brief for the current project: checkpoints, runs, memories, recent activity",
                )
                .with_mime_type(MARKDOWN),
            Resource::new(MEMORIES_URI, "memories")
                .with_title("Project memories")
                .with_description("Saved memories for the current project plus global ones, grouped by kind")
                .with_mime_type(MARKDOWN),
        ]))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new(BRIEF_TEMPLATE, "project-brief")
                .with_title("Resume brief for a project")
                .with_description(
                    "Hand-off brief for an explicit project key (URL-encoded, e.g. github.com%2Facme%2Fshop)",
                )
                .with_mime_type(MARKDOWN),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let uri = request.uri;
        let text = if uri == BRIEF_URI {
            self.brief_markdown(&ctx, None).await?
        } else if uri == MEMORIES_URI {
            self.memories_markdown(&ctx).await?
        } else if let Some(project) = uri.strip_prefix(BRIEF_PREFIX).and_then(percent_decode).filter(|p| !p.is_empty())
        {
            self.brief_markdown(&ctx, Some(project)).await?
        } else {
            return Err(McpError::resource_not_found(
                format!("unknown resource: {uri}"),
                Some(serde_json::json!({ "uri": uri })),
            ));
        };
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, uri).with_mime_type(MARKDOWN)]).into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        Ok(ListPromptsResult::with_all_items(vec![
            Prompt::new(
                "resume",
                Some("Load the memdusa hand-off brief and continue the work from it"),
                Some(vec![
                    PromptArgument::new("project")
                        .with_description("Project key (defaults to the current project)")
                        .with_required(false),
                ]),
            ),
            Prompt::new::<_, &str>(
                "checkpoint",
                Some("Save a memdusa checkpoint (goal, done, next, blockers, key files) and durable memories"),
                None,
            ),
        ]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
        let text = match request.name.as_str() {
            "resume" => {
                let project = request
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("project"))
                    .and_then(|p| p.as_str())
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(String::from);
                let brief = self.brief_markdown(&ctx, project).await?;
                format!("{brief}\n\n{RESUME_INSTRUCTION}")
            }
            "checkpoint" => CHECKPOINT_PROMPT.to_string(),
            other => return Err(McpError::invalid_params(format!("unknown prompt: {other}"), None)),
        };
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)]).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::ServiceExt;
    use rmcp::model::{CallToolRequestParams, ContentBlock, ErrorCode};
    use rmcp::service::ServiceError;

    async fn client() -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        client_for(None).await
    }

    /// A client whose server was launched as a worker on `task` (`MEMDUSA_TASK`).
    async fn client_for(task: Option<&str>) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        let dir = std::env::temp_dir().join(format!("memdusa-mcp-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::connect(&memdusa_core::store::sqlite_url(&dir.join("t.db"))).await.unwrap();
        let server = MemdusaMcp::new(
            store,
            Defaults {
                project: Some("github.com/acme/app".into()),
                agent: "test".into(),
                cwd: None,
                task: task.map(String::from),
                run: None,
            },
        );
        let (a, b) = tokio::io::duplex(65536);
        tokio::spawn(async move {
            let s = server.serve(a).await.unwrap();
            let _ = s.waiting().await;
        });
        ().serve(b).await.unwrap()
    }

    async fn save_checkpoint(
        c: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
        project: Option<&str>,
        goal: &str,
    ) {
        let mut args = serde_json::json!({ "goal": goal });
        if let Some(p) = project {
            args["project"] = p.into();
        }
        let r = c
            .call_tool(CallToolRequestParams::new("checkpoint").with_arguments(args.as_object().unwrap().clone()))
            .await
            .unwrap();
        assert_ne!(r.is_error, Some(true));
    }

    async fn read_text(c: &rmcp::service::RunningService<rmcp::RoleClient, ()>, uri: &str) -> String {
        let r = c.read_resource(ReadResourceRequestParams::new(uri)).await.unwrap();
        match &r.contents[0] {
            ResourceContents::TextResourceContents { text, mime_type, .. } => {
                assert_eq!(mime_type.as_deref(), Some("text/markdown"));
                text.clone()
            }
            _ => panic!("expected text contents"),
        }
    }

    #[tokio::test]
    async fn worker_checkpoints_default_to_the_env_task_and_resume_accepts_a_task() {
        let c = client_for(Some("install-scripts")).await;
        call(&c, "checkpoint", serde_json::json!({ "goal": "windows installer" })).await;
        call(&c, "checkpoint", serde_json::json!({ "goal": "own pick", "task": "docs" })).await;

        // the worker's brief is its task's; an explicit `task` argument overrides the env
        let (_, own) = call(&c, "resume", serde_json::json!({})).await;
        assert!(own.contains("Goal [install-scripts]: windows installer") && !own.contains("own pick"), "{own}");
        let (_, docs) = call(&c, "resume", serde_json::json!({ "task": "docs" })).await;
        assert!(docs.contains("Goal [docs]: own pick") && !docs.contains("windows installer"), "{docs}");

        // without MEMDUSA_TASK a checkpoint has no task
        let plain = client().await;
        call(&plain, "checkpoint", serde_json::json!({ "goal": "coordinate" })).await;
        let (_, md) = call(&plain, "resume", serde_json::json!({})).await;
        assert!(md.contains("Goal: coordinate") && !md.contains("delegated task"), "{md}");
    }

    #[tokio::test]
    async fn lists_resources_and_templates() {
        let c = client().await;
        let uris: Vec<_> = c.list_all_resources().await.unwrap().into_iter().map(|r| r.uri).collect();
        assert!(uris.contains(&"memdusa://brief".to_string()));
        assert!(uris.contains(&"memdusa://memories".to_string()));
        let templates: Vec<_> =
            c.list_all_resource_templates().await.unwrap().into_iter().map(|t| t.uri_template).collect();
        assert!(templates.contains(&"memdusa://brief/{project}".to_string()));
    }

    #[tokio::test]
    async fn reads_brief_and_project_template() {
        let c = client().await;
        save_checkpoint(&c, None, "ship the resources feature").await;
        save_checkpoint(&c, Some("github.com/acme/shop"), "rebuild the checkout").await;

        let brief = read_text(&c, "memdusa://brief").await;
        assert!(brief.contains("ship the resources feature"), "{brief}");
        assert!(!brief.contains("rebuild the checkout"));

        let other = read_text(&c, "memdusa://brief/github.com%2Facme%2Fshop").await;
        assert!(other.contains("rebuild the checkout"), "{other}");
        assert!(!other.contains("ship the resources feature"));
    }

    #[tokio::test]
    async fn reads_memories() {
        let c = client().await;
        let args = serde_json::json!({ "content": "prefer tabs over spaces", "kind": "preference" });
        c.call_tool(CallToolRequestParams::new("memory_save").with_arguments(args.as_object().unwrap().clone()))
            .await
            .unwrap();
        let text = read_text(&c, "memdusa://memories").await;
        assert!(text.contains("prefer tabs over spaces"), "{text}");
    }

    async fn call(
        c: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
        tool: &'static str,
        args: serde_json::Value,
    ) -> (bool, String) {
        let r = c.call_tool(CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone())).await;
        let r = r.unwrap();
        let text = serde_json::to_value(&r.content[0]).unwrap()["text"].as_str().unwrap().to_owned();
        (r.is_error != Some(true), text)
    }

    #[tokio::test]
    async fn updates_memories_and_reports_near_duplicate_merges() {
        let c = client().await;
        let (ok, saved) =
            call(&c, "memory_save", serde_json::json!({ "content": "The API uses JWT tokens for auth" })).await;
        assert!(ok);
        let id = serde_json::from_str::<Value>(&saved).unwrap()["id"].as_str().unwrap().to_owned();

        let (ok, text) =
            call(&c, "memory_save", serde_json::json!({ "content": "API uses JWT tokens for authentication" })).await;
        assert!(ok);
        assert!(text.contains(&format!("updated existing memory {id} (near-duplicate)")), "{text}");

        let (ok, text) = call(
            &c,
            "memory_update",
            serde_json::json!({ "id": id, "content": "Auth moved to opaque session cookies", "kind": "decision", "tags": ["auth"] }),
        )
        .await;
        assert!(ok, "{text}");
        let m: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(m["content"], "Auth moved to opaque session cookies");
        assert_eq!(m["kind"], "decision");
        assert_eq!(m["tags"], serde_json::json!(["auth"]));

        let (ok, _) = call(&c, "memory_update", serde_json::json!({ "id": "nope", "content": "x" })).await;
        assert!(!ok);
        let (_, found) = call(&c, "memory_search", serde_json::json!({ "query": "cookies" })).await;
        assert!(found.contains(&id), "{found}");
    }

    #[tokio::test]
    async fn memory_review_lists_related_memories_with_ids() {
        let c = client().await;
        let mut ids = vec![];
        for content in
            ["Payments webhook retries use exponential backoff", "Payments webhook retries use capped backoff delays"]
        {
            let (ok, saved) = call(&c, "memory_save", serde_json::json!({ "content": content, "kind": "fact" })).await;
            assert!(ok, "{saved}");
            ids.push(serde_json::from_str::<Value>(&saved).unwrap()["id"].as_str().unwrap().to_owned());
        }
        let (ok, text) = call(&c, "memory_review", serde_json::json!({})).await;
        assert!(ok, "{text}");
        assert!(text.contains("Related memories") && ids.iter().all(|id| text.contains(id)), "{text}");
    }

    #[tokio::test]
    async fn list_args_accept_a_string_or_an_array() {
        let c = client().await;
        let (ok, r) = call(
            &c,
            "checkpoint",
            serde_json::json!({
                "goal": "lenient lists",
                "done": "a\nb",
                "next": ["x", "y"],
                "blockers": "blocked",
                "files": "\n src/a.rs \n\n src/b.rs\n"
            }),
        )
        .await;
        assert!(ok, "{r}");
        let (_, md) = call(&c, "resume", serde_json::json!({})).await;
        assert!(md.contains("**Done:**\n- a\n- b\n"), "{md}");
        assert!(md.contains("**Next:**\n- x\n- y\n"), "{md}");
        assert!(md.contains("**Blockers:**\n- blocked\n"), "{md}");
        assert!(md.contains("**Key files:**\n- src/a.rs\n- src/b.rs\n"), "{md}");

        // tags on memory_save use the same lenient deserializer
        let (ok, saved) =
            call(&c, "memory_save", serde_json::json!({ "content": "lenient tags", "tags": "alpha\nbeta" })).await;
        assert!(ok, "{saved}");
        assert_eq!(serde_json::from_str::<Value>(&saved).unwrap()["tags"], serde_json::json!(["alpha", "beta"]));
    }

    #[tokio::test]
    async fn unknown_resource_is_not_found() {
        let c = client().await;
        for uri in ["memdusa://nope", "memdusa://brief/", "memdusa://brief/%zz"] {
            match c.read_resource(ReadResourceRequestParams::new(uri)).await {
                Err(ServiceError::McpError(e)) => assert_eq!(e.code, ErrorCode::RESOURCE_NOT_FOUND, "{uri}"),
                other => panic!("expected resource-not-found for {uri}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn prompts_resume_and_checkpoint() {
        let c = client().await;
        let names: Vec<_> = c.list_all_prompts().await.unwrap().into_iter().map(|p| p.name).collect();
        assert!(names.contains(&"resume".to_string()) && names.contains(&"checkpoint".to_string()));

        save_checkpoint(&c, None, "finish the prompt tests").await;
        let r = c.get_prompt(GetPromptRequestParams::new("resume")).await.unwrap();
        assert_eq!(r.messages.len(), 1);
        assert_eq!(r.messages[0].role, Role::User);
        let ContentBlock::Text(t) = &r.messages[0].content else { panic!("expected text") };
        assert!(t.text.contains("finish the prompt tests"));
        assert!(t.text.contains("Continue from this brief"));

        let args = serde_json::json!({ "project": "github.com/acme/other" });
        let r = c
            .get_prompt(GetPromptRequestParams::new("resume").with_arguments(args.as_object().unwrap().clone()))
            .await
            .unwrap();
        let ContentBlock::Text(t) = &r.messages[0].content else { panic!("expected text") };
        assert!(t.text.contains("github.com/acme/other") && !t.text.contains("finish the prompt tests"));

        let r = c.get_prompt(GetPromptRequestParams::new("checkpoint")).await.unwrap();
        let ContentBlock::Text(t) = &r.messages[0].content else { panic!("expected text") };
        assert!(t.text.contains("memory_save"));

        assert!(c.get_prompt(GetPromptRequestParams::new("nope")).await.is_err());
    }
}
