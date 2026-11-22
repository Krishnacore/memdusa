//! Claude Code hook handler: `memdusa hook <event>` reads the hook JSON on
//! stdin and makes the session durable without the agent having to remember:
//!
//! * `SessionStart`: inject the resume brief (also after /clear and compaction).
//! * `UserPromptSubmit`: journal what the user asked for.
//! * `PostToolUse`: journal file edits and shell commands.
//! * `Stop`: nudge the agent to checkpoint when it changed files and hasn't
//!   checkpointed for a while.
//! * `PreCompact` / `SessionEnd`: write an automatic fallback checkpoint if the
//!   agent didn't save one, so nothing is lost.
//! * `Git`: journal commits, branch switches and merges collected by
//!   `memdusa git-hook` (whoever made them).
//!
//! A hook must never break the session: every failure is logged to stderr and
//! the process exits 0 with no output.

use std::path::Path;

use memdusa_core::brief::{self, AGENT_MESSAGE, COMMAND, COMPACT_SUMMARY, FILE_CHANGE, MEMORY_RECALL, STOP_GUARD};
use memdusa_core::model::*;
use memdusa_core::store::{significant_words, words};
use memdusa_core::{Result, Store};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PostToolUse,
    Stop,
    PreCompact,
    /// Claude Code: after compaction, with the summary the harness generated.
    PostCompact,
    SessionEnd,
    /// Commits, branch switches and merges, collected by `memdusa git-hook`.
    Git,
}

impl HookEvent {
    /// CLI argument (`memdusa hook <arg>`).
    pub fn arg(self) -> &'static str {
        match self {
            Self::SessionStart => "session-start",
            Self::UserPromptSubmit => "user-prompt-submit",
            Self::PostToolUse => "post-tool-use",
            Self::Stop => "stop",
            Self::PreCompact => "pre-compact",
            Self::PostCompact => "post-compact",
            Self::SessionEnd => "session-end",
            Self::Git => "git",
        }
    }

    /// Hook timeout in seconds. SessionEnd only gets 1.5s by default; an
    /// explicit timeout raises that budget.
    pub fn timeout_secs(self) -> u64 {
        match self {
            Self::SessionStart | Self::SessionEnd | Self::PreCompact | Self::PostCompact => 10,
            Self::Git => 3,
            _ => 5,
        }
    }

    /// Events that save or compare code state (so the client reads git only then).
    pub fn needs_git(self) -> bool {
        matches!(self, Self::SessionStart | Self::PreCompact | Self::SessionEnd)
    }
}

pub const JOURNALED_TOOLS: &str = "Edit|MultiEdit|Write|NotebookEdit|Bash";

/// The subset of the Claude Code hook payload we use. Unknown fields are ignored.
#[derive(Debug, Default, Deserialize)]
pub struct HookInput {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// SessionStart: startup | resume | clear | compact
    #[serde(default)]
    pub source: Option<String>,
    /// UserPromptSubmit
    #[serde(default)]
    pub prompt: Option<String>,
    /// PostToolUse
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<Value>,
    /// Stop: true when Claude is already continuing because of a Stop hook.
    #[serde(default)]
    pub stop_hook_active: Option<bool>,
    /// Stop: the agent's final message of the turn (Claude Code, Codex; adapters normalize others).
    #[serde(default)]
    pub last_assistant_message: Option<String>,
    /// PostCompact: the summary the harness generated.
    #[serde(default)]
    pub compact_summary: Option<String>,
    /// PreCompact / PostCompact: manual | auto
    #[serde(default)]
    pub trigger: Option<String>,
    /// SessionEnd
    #[serde(default)]
    pub reason: Option<String>,
    /// Git: `commit`, `checkout` or `merge`.
    #[serde(default)]
    pub git_event: Option<String>,
    /// Git: one-line summary for the journal.
    #[serde(default)]
    pub content: Option<String>,
    /// Git: structured facts (sha, branch, files, ...).
    #[serde(default)]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    pub agent: String,
    /// Journal user prompts (UserPromptSubmit).
    pub journal_prompts: bool,
    /// Stop guard: ask for a checkpoint when files changed and the last
    /// checkpoint is older than this many minutes. 0 disables the guard.
    pub stop_guard_mins: i64,
    /// Inject the memories relevant to each prompt (UserPromptSubmit). `MEMDUSA_RECALL=off` disables it.
    #[serde(default = "default_recall")]
    pub recall: bool,
}

fn default_recall() -> bool {
    true
}

/// One hook invocation, as sent to `POST /v1/hooks/{event}` in remote mode.
/// The project is detected on the client (the server can't see its git repo);
/// `input` is the harness's raw hook payload, forwarded untouched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookRequest {
    pub project: String,
    #[serde(flatten)]
    pub config: HookConfig,
    pub input: Value,
    /// Code state of the client's repo (for events that save or compare it).
    #[serde(default)]
    pub git: Option<GitState>,
    /// When the hook fired on the client. Queued hooks replayed later keep
    /// their original time.
    #[serde(default)]
    pub at: Option<Millis>,
    /// `MEMDUSA_TASK` of the client's environment: this session is a worker delegated that task.
    #[serde(default)]
    pub task: Option<String>,
    /// `MEMDUSA_PARENT`: free text naming the coordinator that delegated it.
    #[serde(default)]
    pub parent: Option<String>,
    /// `MEMDUSA_RUN`: the delegated run `task` is a step of; the worker claims that step and keeps its lease alive.
    #[serde(default)]
    pub run: Option<String>,
}

/// One queued hook, replayed through `POST /v1/hooks/batch`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookBatchItem {
    /// Event name as used on the command line (`post-tool-use`).
    pub event: String,
    pub request: HookRequest,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookBatchResponse {
    pub processed: usize,
    /// Items the server rejected as invalid (they are not retried).
    #[serde(default)]
    pub rejected: Vec<String>,
}

impl HookEvent {
    /// Write-only events whose effect can be applied later if the server is
    /// unreachable now. SessionStart and Stop produce output that is only
    /// useful in the moment, so they aren't queued.
    pub fn queueable(self) -> bool {
        matches!(
            self,
            Self::UserPromptSubmit
                | Self::PostToolUse
                | Self::PreCompact
                | Self::PostCompact
                | Self::SessionEnd
                | Self::Git
        )
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookResponse {
    /// What the hook should print on stdout, if anything.
    pub output: Option<String>,
}

impl std::str::FromStr for HookEvent {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        <Self as clap::ValueEnum>::from_str(s, true)
    }
}

/// Entry point shared by local hooks and the HTTP endpoint, so both modes
/// run exactly the same logic.
pub async fn handle_request(
    store: &Store,
    event: HookEvent,
    mut req: HookRequest,
    identity: &Identity,
) -> Result<HookResponse> {
    // On a multi-user server records are attributed to `user/agent`.
    req.config.agent = identity.qualify(&req.config.agent);
    let input: HookInput =
        serde_json::from_value(req.input).map_err(|e| memdusa_core::Error::Invalid(format!("hook input: {e}")))?;
    let ctx = Ctx {
        store,
        project: req.project,
        session: input.session_id.clone(),
        cwd: input.cwd.clone(),
        user: identity.user.clone(),
        git: req.git.clone(),
        at: req.at.map_or_else(now_ms, |t| t.min(now_ms())),
        task: req.task.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()),
        parent: req.parent.map(|p| p.trim().to_owned()).filter(|p| !p.is_empty()),
        run: req.run.map(|r| r.trim().to_owned()).filter(|r| !r.is_empty()),
        cfg: &req.config,
    };
    Ok(HookResponse { output: handle(event, input, ctx).await? })
}

/// How much of a user's request is kept verbatim.
const MAX_PROMPT_CHARS: usize = 4_000;
/// The agent's last message and a compaction summary, as stored.
const MAX_MESSAGE_CHARS: usize = 1_500;
const MAX_SUMMARY_CHARS: usize = 4_000;
/// How much of the last message goes into a fallback checkpoint's notes.
const MAX_NOTE_MESSAGE_CHARS: usize = 800;

/// Claude Code caps `additionalContext` at 10,000 chars (beyond that it only
/// shows a preview); keep the brief plus wrapper safely under it.
const MAX_CONTEXT_CHARS: usize = 8_500;

pub struct Ctx<'a> {
    pub store: &'a Store,
    pub project: String,
    pub session: Option<String>,
    pub cwd: Option<String>,
    /// Authenticated user, if any (decides which global memories are visible).
    pub user: Option<String>,
    /// The client's current code state, when the event needs it.
    pub git: Option<GitState>,
    /// When the hook fired (what records are timestamped with).
    pub at: Millis,
    /// The delegated task this session works on (`MEMDUSA_TASK`), if it is a worker.
    pub task: Option<String>,
    pub parent: Option<String>,
    /// The delegated run the task belongs to (`MEMDUSA_RUN`); with `task`, the worker holds that step's lease.
    pub run: Option<String>,
    pub cfg: &'a HookConfig,
}

/// Who holds a worker's step lease: the agent and its session, so a replacement session (or harness) is told apart.
fn step_owner(ctx: &Ctx<'_>) -> String {
    match &ctx.session {
        Some(s) => format!("{}:{s}", ctx.cfg.agent),
        None => ctx.cfg.agent.clone(),
    }
}

/// Keep the worker's step lease alive. Cheap, and a failure never matters to the session.
async fn heartbeat(ctx: &Ctx<'_>) {
    let (Some(run), Some(task)) = (&ctx.run, &ctx.task) else { return };
    let lease = memdusa_core::store::step_lease_secs();
    if let Err(e) = ctx.store.step_heartbeat(run, task, &step_owner(ctx), lease).await {
        eprintln!("memdusa: could not renew the lease on task {task}: {e}");
    }
}

/// Claim the worker's step at session start; the notice for the top of its brief says how that went.
async fn claim_step(ctx: &Ctx<'_>, source: &str) -> Option<String> {
    let (run, task) = (ctx.run.as_deref()?, ctx.task.as_deref()?);
    // /clear and compaction restart the same worker in the same terminal, not a rival: its own lease is not a conflict
    let force = matches!(source, "clear" | "compact");
    let lease = memdusa_core::store::step_lease_secs();
    Some(match ctx.store.step_claim(run, task, &step_owner(ctx), lease, force).await {
        Ok(claim) => brief::claim_notice(task, &claim),
        Err(e) => {
            eprintln!("memdusa: could not claim task {task} of run {run}: {e}");
            format!(
                "**memdusa could not claim the delegated task `{task}` of run `{run}`: {e}.** Check MEMDUSA_RUN and \
                 MEMDUSA_TASK; until then nothing stops another worker from taking the same task."
            )
        }
    })
}

/// Handle one hook event. Returns what to print on stdout (if anything).
pub async fn handle(event: HookEvent, input: HookInput, ctx: Ctx<'_>) -> Result<Option<String>> {
    // Every event of a worker is a sign of life. SessionEnd is not: the lease is left to expire, so a worker
    // that stopped mid-task shows as stale (completion is explicit: `task_complete`).
    if !matches!(event, HookEvent::SessionStart | HookEvent::SessionEnd | HookEvent::Git) {
        heartbeat(&ctx).await;
    }
    match event {
        HookEvent::SessionStart => session_start(&ctx, input.source.as_deref().unwrap_or("startup")).await,
        HookEvent::UserPromptSubmit => {
            if ctx.cfg.journal_prompts
                && let Some(p) = input.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty())
            {
                // Short line for the timeline; the full request is kept for checkpoints and the brief.
                // (The handoff eval caught this: a 400-char cut dropped half of a task's requirements.)
                log(&ctx, "prompt", truncate(p, 200), Some(json!({ "text": truncate(p, MAX_PROMPT_CHARS) }))).await?;
            }
            // Best effort: a failed recall must never fail the hook.
            let prompt = input.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty());
            match prompt {
                Some(p) if ctx.cfg.recall => recall(&ctx, p).await.or_else(|e| {
                    eprintln!("memdusa: recall failed: {e}");
                    Ok(None)
                }),
                _ => Ok(None),
            }
        }
        HookEvent::PostToolUse => {
            post_tool_use(&ctx, input.tool_name.as_deref().unwrap_or_default(), input.tool_input.as_ref()).await?;
            Ok(None)
        }
        HookEvent::Stop => {
            if let Some(m) = input.last_assistant_message.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
                record_agent_message(&ctx, m).await?;
            }
            stop(&ctx, input.stop_hook_active.unwrap_or(false)).await
        }
        HookEvent::PostCompact => {
            if let Some(s) = input.compact_summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let trigger = input.trigger.as_deref().unwrap_or("auto");
                log(&ctx, COMPACT_SUMMARY, truncate(s, MAX_SUMMARY_CHARS), Some(json!({ "trigger": trigger }))).await?;
            }
            Ok(None)
        }
        HookEvent::PreCompact => {
            let trigger = input.trigger.as_deref().unwrap_or("auto");
            auto_checkpoint(&ctx, &format!("context compaction ({trigger})")).await?;
            log(&ctx, "compaction", format!("context compacted ({trigger})"), None).await?;
            Ok(None)
        }
        HookEvent::SessionEnd => {
            let reason = input.reason.as_deref().unwrap_or("other");
            // Harnesses whose prompt/stop hooks don't fire (`cursor-agent -p`) hand over the request and the
            // agent's last words here. A session that already journaled them (stop, prompt hooks) isn't repeated.
            if ctx.cfg.journal_prompts
                && let Some(p) = input.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty())
                && !has_event(&ctx, "prompt").await?
            {
                log(&ctx, "prompt", truncate(p, 200), Some(json!({ "text": truncate(p, MAX_PROMPT_CHARS) }))).await?;
            }
            if let Some(m) = input.last_assistant_message.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
                record_agent_message(&ctx, m).await?;
            }
            auto_checkpoint(&ctx, &format!("session end ({reason})")).await?;
            log(&ctx, "session_end", format!("session ended ({reason})"), None).await?;
            Ok(None)
        }
        HookEvent::Git => {
            let git_event = input.git_event.as_deref().unwrap_or_default();
            if !matches!(git_event, "commit" | "checkout" | "merge") {
                return Err(memdusa_core::Error::Invalid(format!(
                    "git_event must be commit, checkout or merge (got {git_event:?})"
                )));
            }
            log(&ctx, git_event, input.content.unwrap_or_default(), input.data).await?;
            Ok(None)
        }
    }
}

async fn log(ctx: &Ctx<'_>, kind: &str, content: String, data: Option<Value>) -> Result<()> {
    ctx.store
        .event_append(NewEvent {
            project: ctx.project.clone(),
            session: ctx.session.clone(),
            source: Some(ctx.cfg.agent.clone()),
            kind: kind.into(),
            content,
            data,
            created_at: Some(ctx.at),
        })
        .await
        .map(drop)
}

async fn session_start(ctx: &Ctx<'_>, source: &str) -> Result<Option<String>> {
    let notice = claim_step(ctx, source).await;
    // Build the brief before journaling, so it doesn't report its own start.
    let b = brief::build(ctx.store, &ctx.project, ctx.user.as_deref(), ctx.git.as_ref(), ctx.task.as_deref()).await?;
    log(ctx, "session_start", format!("session started ({source})"), None).await?;
    if let Some(task) = &ctx.task {
        let data = json!({ "task": task, "parent": ctx.parent, "harness": ctx.cfg.agent, "source": source });
        log(ctx, brief::DELEGATED_SESSION, task.clone(), Some(data)).await?;
    }

    let empty = b.checkpoints.is_empty() && b.active_runs.is_empty() && b.memories.is_empty() && b.delegated.is_empty();
    if empty && b.recent_events.is_empty() && notice.is_none() {
        return Ok(None);
    }
    let mut md = b.markdown;
    if md.len() > MAX_CONTEXT_CHARS {
        let cut = md.floor_char_boundary(MAX_CONTEXT_CHARS);
        md.truncate(cut);
        md.push_str("\n\n…(truncated; call the memdusa `resume` tool for the full brief)");
    }
    // The memories in the brief count as already shown: don't recall them again this session.
    let shown: Vec<&str> = b.memories.iter().filter(|m| md.contains(&m.content)).map(|m| m.id.as_str()).collect();
    if let Some(n) = &notice {
        md = format!("## Your delegated task\n{n}\n\n{md}");
    }
    if ctx.session.is_some() && !shown.is_empty() {
        let n = shown.len();
        if let Err(e) =
            log(ctx, MEMORY_RECALL, format!("{n} memories in the session brief"), Some(json!({ "ids": shown }))).await
        {
            eprintln!("memdusa: could not record the brief's memories: {e}");
        }
    }
    let context = format!(
        "<memdusa-resume project=\"{}\">\n{md}\n</memdusa-resume>\n\
         The brief above was loaded from memdusa, the shared memory used by every agent and tool on this project \
         (possibly written by a different harness). Continue from it rather than starting over. Save progress with \
         the memdusa `checkpoint` tool before you finish.",
        ctx.project
    );
    Ok(Some(
        json!({ "hookSpecificOutput": { "hookEventName": "SessionStart", "additionalContext": context } }).to_string(),
    ))
}

/// Memories injected per prompt, at most.
const RECALL_MAX: usize = 5;
/// Ranked candidates fetched (some are dropped as weak matches or already shown).
const RECALL_CANDIDATES: i64 = 25;
/// Longest the prompt waits for the query embedding (semantic search), then recall is lexical.
const RECALL_EMBED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
/// Default cosine similarity from which a memory is recalled without sharing words with the prompt
/// (stricter than the search floor: recall runs on every prompt). Similarities differ between models,
/// so `MEMDUSA_EMBED_RECALL_MIN` overrides it (read where the hook runs: the server in remote mode).
pub(crate) const RECALL_COSINE: f32 = 0.5;

fn recall_cosine() -> f32 {
    std::env::var("MEMDUSA_EMBED_RECALL_MIN")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(crate::embedder::default_recall_min)
}
/// Prompt terms searched for.
const RECALL_TERMS: usize = 16;
/// Longest memory shown, in characters.
const RECALL_MEMORY_CHARS: usize = 500;

/// Prompt jargon that is too generic to recall on, on top of the shared language filter in
/// [`significant_words`]. Applying the core filter first means every term counted toward `need`
/// is also a term the full-text query keeps — the two filters agree by construction.
const RECALL_STOPWORDS: &[&str] = &[
    "her", "one", "out", "him", "his", "may", "new", "now", "old", "see", "two", "way", "get", "got", "put", "say",
    "she", "too", "use", "been", "while", "these", "those", "such", "only", "like", "make", "dont", "thanks", "thank",
    "much", "each", "other", "over", "after", "before", "again", "still", "work", "code", "file", "files",
];

/// Memories relevant to the user's prompt, as `additionalContext`. Needs a
/// session (to remember what it already showed); returns `None` when nothing
/// matches well enough or everything relevant was shown before.
async fn recall(ctx: &Ctx<'_>, prompt: &str) -> Result<Option<String>> {
    let Some(session) = &ctx.session else { return Ok(None) };
    let terms: Vec<String> = significant_words(prompt)
        .into_iter()
        .filter(|w| !RECALL_STOPWORDS.contains(&w.as_str()))
        .take(RECALL_TERMS)
        .collect();
    if terms.is_empty() {
        return Ok(None);
    }
    // "Meaningful": at least two of the prompt's terms (one, if it only has one).
    let need = terms.len().min(2);

    // A slow embedding API must not stall the prompt: cap it, then fall back to lexical search.
    let candidates = ctx
        .store
        .memory_search_scored(
            MemoryQuery {
                query: Some(terms.join(" ")),
                project: Some(ctx.project.clone()),
                limit: Some(RECALL_CANDIDATES),
                user: ctx.user.clone(),
                ..Default::default()
            },
            Some(RECALL_EMBED_TIMEOUT),
        )
        .await?;
    if candidates.is_empty() {
        return Ok(None);
    }

    let mut shown = std::collections::HashSet::new();
    for e in ctx.store.events_recent_kind(&ctx.project, Some(session), MEMORY_RECALL, 200).await? {
        for id in e.data.as_ref().and_then(|d| d["ids"].as_array()).into_iter().flatten().filter_map(Value::as_str) {
            shown.insert(id.to_owned());
        }
    }

    let min_cosine = recall_cosine();
    let picked: Vec<&Memory> = candidates
        .iter()
        .filter(|s| !shown.contains(&s.memory.id))
        .filter(|s| {
            // a close embedding match is relevant even when the words differ
            if s.cosine.is_some_and(|c| c >= min_cosine) {
                return true;
            }
            let m = &s.memory;
            let have: std::collections::HashSet<String> =
                words(&format!("{} {}", m.content, m.tags.join(" "))).into_iter().collect();
            terms.iter().filter(|t| have.contains(*t)).count() >= need
        })
        .map(|s| &s.memory)
        .take(RECALL_MAX)
        .collect();
    if picked.is_empty() {
        return Ok(None);
    }

    let ids: Vec<&str> = picked.iter().map(|m| m.id.as_str()).collect();
    log(ctx, MEMORY_RECALL, format!("recalled {} memories for the prompt", ids.len()), Some(json!({ "ids": ids })))
        .await?;

    let mut text = String::from("<memdusa-recall>\nRelevant memories:\n");
    for m in picked {
        let body = truncate(m.content.trim(), RECALL_MEMORY_CHARS).replace('\n', "\n  ");
        text.push_str(&format!("- ({}) {body}\n", m.kind.as_str()));
    }
    text.push_str("</memdusa-recall>");
    Ok(Some(
        json!({ "hookSpecificOutput": { "hookEventName": "UserPromptSubmit", "additionalContext": text } }).to_string(),
    ))
}

async fn post_tool_use(ctx: &Ctx<'_>, tool: &str, input: Option<&Value>) -> Result<()> {
    let field = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str);
    match tool {
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
            // one path, or several (a Codex patch touching many files)
            let many = input.and_then(|i| i.get("file_paths")).and_then(Value::as_array);
            let paths: Vec<&str> = match many {
                Some(list) => list.iter().filter_map(Value::as_str).collect(),
                None => field("file_path").or_else(|| field("notebook_path")).into_iter().collect(),
            };
            // `cwd` is the only worktree hint available here: PostToolUse must stay fast and
            // never shells out to git. Canonicalizing it (no process spawn) makes it comparable
            // with `git rev-parse --show-toplevel`, e.g. /var vs /private/var on macOS.
            let cwd = ctx.cwd.as_deref().map(|c| {
                std::fs::canonicalize(c).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| c.to_owned())
            });
            for path in paths {
                let rel = relative(path, ctx.cwd.as_deref());
                log(ctx, FILE_CHANGE, rel, Some(json!({ "tool": tool, "path": path, "cwd": cwd }))).await?;
            }
        }
        "Bash" => {
            if let Some(cmd) = field("command") {
                let first = cmd.lines().next().unwrap_or_default().trim();
                let more = if cmd.trim().lines().count() > 1 { " …" } else { "" };
                log(ctx, COMMAND, format!("{}{more}", truncate(first, 160)), None).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Whether this session already has an event of `kind`.
async fn has_event(ctx: &Ctx<'_>, kind: &str) -> Result<bool> {
    let Some(session) = &ctx.session else { return Ok(false) };
    Ok(ctx.store.events_recent(&ctx.project, Some(session), 300).await?.iter().any(|e| e.kind == kind))
}

/// Journal the agent's last words, unless they repeat the session's previous message.
async fn record_agent_message(ctx: &Ctx<'_>, message: &str) -> Result<()> {
    let content = truncate(message, MAX_MESSAGE_CHARS);
    if let Some(session) = &ctx.session {
        // Stored content is redacted (after truncation), so compare it in that form.
        let redacted = ctx.store.redactor().text(&content).into_owned();
        let recent = ctx.store.events_recent(&ctx.project, Some(session), 300).await?;
        if recent.iter().find(|e| e.kind == AGENT_MESSAGE).is_some_and(|e| e.content == redacted) {
            return Ok(());
        }
    }
    log(ctx, AGENT_MESSAGE, content, None).await
}

/// The newest checkpoint of this session's own scope: a worker's task, or (for everyone else)
/// the project's newest checkpoint that isn't a delegated worker's.
async fn scope_checkpoint(ctx: &Ctx<'_>) -> Result<Option<Checkpoint>> {
    if let Some(task) = &ctx.task {
        return Ok(ctx.store.checkpoints_for_task(&ctx.project, task, 1).await?.into_iter().next());
    }
    let delegated = brief::delegated_task_names(ctx.store, &ctx.project).await?;
    let latest = ctx.store.checkpoints_latest(&ctx.project, 10 + delegated.len() as i64).await?;
    Ok(latest.into_iter().find(|c| c.task.as_ref().is_none_or(|t| !delegated.contains(t))))
}

async fn stop(ctx: &Ctx<'_>, already_continuing: bool) -> Result<Option<String>> {
    let (Some(session), mins) = (&ctx.session, ctx.cfg.stop_guard_mins) else { return Ok(None) };
    if already_continuing || mins <= 0 {
        return Ok(None);
    }
    let last = scope_checkpoint(ctx).await?.map(|c| c.created_at);
    if last.is_some_and(|t| now_ms() - t < mins * 60_000) {
        return Ok(None);
    }
    let changed = ctx.store.count_events_since(&ctx.project, session, FILE_CHANGE, last.unwrap_or(0)).await?;
    if changed == 0 {
        return Ok(None);
    }
    // At most once per guard window per session, whatever the harness reports about
    // loops (some, like Antigravity, have no stop_hook_active): never trap an agent
    // that can't checkpoint.
    let window_start = now_ms() - mins * 60_000;
    if ctx.store.count_events_since(&ctx.project, session, STOP_GUARD, window_start).await? > 0 {
        return Ok(None);
    }
    log(ctx, STOP_GUARD, format!("asked to checkpoint ({changed} file change(s))"), None).await?;
    let reason = format!(
        "memdusa: {changed} file change(s) since the last checkpoint. Before finishing, call the memdusa `checkpoint` \
         tool with the goal, what's done, concrete next steps, blockers and key files, so any agent can pick this up. \
         Also `memory_save` any decisions or preferences worth keeping. If the memdusa tools aren't available, just stop."
    );
    Ok(Some(json!({ "decision": "block", "reason": reason }).to_string()))
}

/// Fallback when context is about to be lost (compaction, session end) and
/// the agent changed files without checkpointing: record what we know.
async fn auto_checkpoint(ctx: &Ctx<'_>, why: &str) -> Result<()> {
    let Some(session) = &ctx.session else { return Ok(()) };
    // Carry the previous checkpoint of this scope forward so its plan isn't lost.
    let prev = scope_checkpoint(ctx).await?;
    let last = prev.as_ref().map_or(0, |p| p.created_at);
    let events: Vec<_> = ctx
        .store
        .events_recent(&ctx.project, Some(session), 300)
        .await?
        .into_iter()
        .filter(|e| e.created_at > last)
        .collect();
    let mut files: Vec<String> = vec![];
    for e in events.iter().filter(|e| e.kind == FILE_CHANGE) {
        if !files.contains(&e.content) {
            files.push(e.content.clone());
        }
    }
    if files.is_empty() {
        return Ok(());
    }
    // The session's requests, oldest first, in full: the first is usually the task itself.
    let prompts: Vec<String> = events
        .iter()
        .rev()
        .filter(|e| e.kind == "prompt")
        .map(|e| e.data.as_ref().and_then(|d| d["text"].as_str()).unwrap_or(&e.content).to_owned())
        .collect();
    let task_line = prompts.first().map(|p| truncate(p.lines().next().unwrap_or(p).trim(), 140));

    let goal = prev
        .as_ref()
        .map(|p| p.goal.clone())
        .or(task_line)
        .unwrap_or_else(|| "(unknown: no checkpoint or prompt recorded)".into());
    let mut done = prev.as_ref().map(|p| p.done.clone()).unwrap_or_default();
    done.push(format!("(auto) edited {} file(s) in session {}", files.len(), truncate(session, 8)));
    let mut notes = format!(
        "Automatic checkpoint written by memdusa at {why}: the agent changed files without saving a checkpoint. \
         Verify the state of the listed files before continuing."
    );
    let quote = |p: &str| p.lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n");
    match prompts.as_slice() {
        [] => {}
        [only] => notes.push_str(&format!("\n\nThe user's request, verbatim:\n\n{}", quote(only))),
        [first, .., last] => notes.push_str(&format!(
            "\n\nThe user's first request, verbatim:\n\n{}\n\nTheir latest request:\n\n{}",
            quote(first),
            quote(last)
        )),
    }
    if let Some(m) = events.iter().find(|e| e.kind == AGENT_MESSAGE) {
        notes.push_str(&format!(
            "\n\nThe agent's last message:\n\n{}",
            quote(&truncate(&m.content, MAX_NOTE_MESSAGE_CHARS))
        ));
    }
    ctx.store
        .checkpoint_save(NewCheckpoint {
            project: ctx.project.clone(),
            task: ctx.task.clone().or_else(|| prev.as_ref().and_then(|p| p.task.clone())),
            goal,
            done,
            next: prev.as_ref().map(|p| p.next.clone()).unwrap_or_default(),
            blockers: prev.as_ref().map(|p| p.blockers.clone()).unwrap_or_default(),
            files,
            notes: Some(notes),
            source: Some(format!("{} (auto)", ctx.cfg.agent)),
            git: ctx.git.clone(),
            created_at: Some(ctx.at),
        })
        .await
        .map(drop)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

fn relative(path: &str, cwd: Option<&str>) -> String {
    cwd.and_then(|c| Path::new(path).strip_prefix(c).ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ctx_store() -> Store {
        let dir = std::env::temp_dir().join(format!("memdusa-hooks-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        Store::connect(&memdusa_core::store::sqlite_url(&dir.join("t.db"))).await.unwrap()
    }

    fn cfg() -> HookConfig {
        HookConfig { agent: "claude-code".into(), journal_prompts: true, stop_guard_mins: 15, recall: true }
    }

    fn ctx<'a>(store: &'a Store, cfg: &'a HookConfig) -> Ctx<'a> {
        Ctx {
            store,
            project: "p".into(),
            session: Some("s1".into()),
            cwd: Some("/repo".into()),
            user: None,
            git: None,
            at: now_ms(),
            task: None,
            parent: None,
            run: None,
            cfg,
        }
    }

    fn input(v: Value) -> HookInput {
        serde_json::from_value(v).unwrap()
    }

    #[tokio::test]
    async fn full_session_lifecycle() {
        let store = ctx_store().await;
        let cfg = cfg();

        // empty project: nothing injected
        let out =
            handle(HookEvent::SessionStart, input(json!({"source": "startup"})), ctx(&store, &cfg)).await.unwrap();
        assert!(out.is_none());

        handle(HookEvent::UserPromptSubmit, input(json!({"prompt": "add login page"})), ctx(&store, &cfg))
            .await
            .unwrap();
        let edit = json!({"tool_name": "Edit", "tool_input": {"file_path": "/repo/src/login.rs"}});
        handle(HookEvent::PostToolUse, input(edit), ctx(&store, &cfg)).await.unwrap();
        let bash = json!({"tool_name": "Bash", "tool_input": {"command": "cargo test\necho done"}});
        handle(HookEvent::PostToolUse, input(bash), ctx(&store, &cfg)).await.unwrap();

        // Stop: files changed, no checkpoint → block once
        let out = handle(HookEvent::Stop, input(json!({"stop_hook_active": false})), ctx(&store, &cfg)).await.unwrap();
        let v: Value = serde_json::from_str(&out.expect("stop guard should fire")).unwrap();
        assert_eq!(v["decision"], "block");
        // ...but never twice in a row
        let out = handle(HookEvent::Stop, input(json!({"stop_hook_active": true})), ctx(&store, &cfg)).await.unwrap();
        assert!(out.is_none());

        // session ends without a checkpoint → automatic one
        handle(HookEvent::SessionEnd, input(json!({"reason": "logout"})), ctx(&store, &cfg)).await.unwrap();
        let cps = store.checkpoints_latest("p", 5).await.unwrap();
        assert_eq!(cps.len(), 1);
        assert_eq!(cps[0].goal, "add login page");
        assert_eq!(cps[0].files, vec!["src/login.rs"]);
        assert_eq!(cps[0].source.as_deref(), Some("claude-code (auto)"));

        // checkpoint is fresh → guard stays quiet; nothing new → no second auto checkpoint
        let out = handle(HookEvent::Stop, input(json!({})), ctx(&store, &cfg)).await.unwrap();
        assert!(out.is_none());
        handle(HookEvent::PreCompact, input(json!({"trigger": "auto"})), ctx(&store, &cfg)).await.unwrap();
        assert_eq!(store.checkpoints_latest("p", 5).await.unwrap()[0].id, cps[0].id);

        // a new session (any harness) gets the brief injected
        let cfg2 = HookConfig { agent: "other".into(), ..cfg };
        let mut c = ctx(&store, &cfg2);
        c.session = Some("s2".into());
        let out = handle(HookEvent::SessionStart, input(json!({"source": "startup"})), c).await.unwrap().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let ctx_text = v["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
        for needle in ["add login page", "src/login.rs", "cargo test …", "<memdusa-resume"] {
            assert!(ctx_text.contains(needle), "missing {needle}:\n{ctx_text}");
        }
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
    }

    #[tokio::test]
    async fn stop_records_last_message_once_and_guard_still_fires() {
        let store = ctx_store().await;
        let cfg = cfg();
        let edit = json!({"tool_name": "Edit", "tool_input": {"file_path": "/repo/a.rs"}});
        handle(HookEvent::PostToolUse, input(edit), ctx(&store, &cfg)).await.unwrap();

        let stop = json!({"last_assistant_message": "  Done with the endpoint; next: partial refunds.  "});
        let out = handle(HookEvent::Stop, input(stop.clone()), ctx(&store, &cfg)).await.unwrap();
        assert!(out.is_some(), "guard fires regardless of the message");
        handle(HookEvent::Stop, input(stop), ctx(&store, &cfg)).await.unwrap();
        let msgs: Vec<_> =
            store.events_recent("p", None, 50).await.unwrap().into_iter().filter(|e| e.kind == AGENT_MESSAGE).collect();
        assert_eq!(msgs.len(), 1, "identical repeated stop doesn't duplicate");
        assert_eq!(msgs[0].content, "Done with the endpoint; next: partial refunds.");

        let next = json!({"last_assistant_message": "x".repeat(2000)});
        handle(HookEvent::Stop, input(next), ctx(&store, &cfg)).await.unwrap();
        let msgs: Vec<_> =
            store.events_recent("p", None, 50).await.unwrap().into_iter().filter(|e| e.kind == AGENT_MESSAGE).collect();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content.chars().count(), MAX_MESSAGE_CHARS + 1);
    }

    #[tokio::test]
    async fn session_end_journals_prompt_and_last_words_once() {
        let store = ctx_store().await;
        let cfg = cfg();
        let end = json!({"reason": "completed", "prompt": "say ok", "last_assistant_message": "ok"});
        handle(HookEvent::SessionEnd, input(end.clone()), ctx(&store, &cfg)).await.unwrap();
        handle(HookEvent::SessionEnd, input(end), ctx(&store, &cfg)).await.unwrap();
        let events = store.events_recent("p", None, 50).await.unwrap();
        let counts = ["prompt", AGENT_MESSAGE, "session_end"].map(|k| events.iter().filter(|e| e.kind == k).count());
        assert_eq!(counts, [1, 1, 2], "prompt and last words are not repeated; session_end is");

        // a session whose prompt hook already ran, and whose stop already recorded the message, adds nothing
        let mut c = ctx(&store, &cfg);
        c.session = Some("s9".into());
        handle(HookEvent::UserPromptSubmit, input(json!({"prompt": "first"})), c).await.unwrap();
        let mut c = ctx(&store, &cfg);
        c.session = Some("s9".into());
        handle(HookEvent::Stop, input(json!({"last_assistant_message": "done"})), c).await.unwrap();
        let mut c = ctx(&store, &cfg);
        c.session = Some("s9".into());
        let end = json!({"prompt": "first (again)", "last_assistant_message": "done"});
        handle(HookEvent::SessionEnd, input(end), c).await.unwrap();
        let mine: Vec<_> = store
            .events_recent("p", Some("s9"), 50)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "prompt" || e.kind == AGENT_MESSAGE)
            .collect();
        assert_eq!(mine.len(), 2);
    }

    #[tokio::test]
    async fn repeated_stop_with_a_secret_doesnt_duplicate() {
        let store = ctx_store().await;
        let cfg = cfg();
        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4).get(..36).unwrap());
        let stop = json!({"last_assistant_message": format!("Deployed using {token}, tests next.")});
        handle(HookEvent::Stop, input(stop.clone()), ctx(&store, &cfg)).await.unwrap();
        handle(HookEvent::Stop, input(stop), ctx(&store, &cfg)).await.unwrap();
        let msgs: Vec<_> =
            store.events_recent("p", None, 50).await.unwrap().into_iter().filter(|e| e.kind == AGENT_MESSAGE).collect();
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(!msgs[0].content.contains(&token), "secret was redacted: {}", msgs[0].content);
    }

    #[tokio::test]
    async fn post_compact_records_summary() {
        let store = ctx_store().await;
        let cfg = cfg();
        assert!(HookEvent::PostCompact.queueable());
        assert!(!HookEvent::PostCompact.needs_git());
        assert_eq!(HookEvent::PostCompact.timeout_secs(), 10);
        assert_eq!(HookEvent::PostCompact.arg(), "post-compact");
        let payload = json!({"trigger": "manual", "compact_summary": "We built refunds."});
        let out = handle(HookEvent::PostCompact, input(payload), ctx(&store, &cfg)).await.unwrap();
        assert!(out.is_none());
        let events = store.events_recent("p", None, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!((events[0].kind.as_str(), events[0].content.as_str()), (COMPACT_SUMMARY, "We built refunds."));
    }

    #[tokio::test]
    async fn brief_shows_last_words_and_hides_them_from_events() {
        let store = ctx_store().await;
        let cfg = cfg();
        let stop = json!({"last_assistant_message": "Endpoint done, tests next."});
        handle(HookEvent::Stop, input(stop), ctx(&store, &cfg)).await.unwrap();
        let payload = json!({"compact_summary": "S".repeat(2000)});
        handle(HookEvent::PostCompact, input(payload), ctx(&store, &cfg)).await.unwrap();

        let md = brief::build(&store, "p", None, None, None).await.unwrap().markdown;
        assert!(md.contains("## Last words from the previous session"), "{md}");
        assert!(md.contains("> Endpoint done, tests next."), "{md}");
        assert!(md.contains(&format!("{}…", "S".repeat(1500))) && !md.contains(&"S".repeat(1501)), "{md}");
        assert!(!md.contains("agent_message") && !md.contains("compact_summary"), "hidden from Events:\n{md}");

        // a checkpoint newer than the message makes it redundant
        let mut c = ctx(&store, &cfg);
        c.at = now_ms() + 10_000;
        store
            .checkpoint_save(NewCheckpoint {
                project: "p".into(),
                goal: "refunds".into(),
                created_at: Some(c.at),
                ..Default::default()
            })
            .await
            .unwrap();
        let md = brief::build(&store, "p", None, None, None).await.unwrap().markdown;
        assert!(md.contains("Where things stand") && !md.contains("Last words"), "{md}");
    }

    #[tokio::test]
    async fn auto_checkpoint_at_session_end_doesnt_hide_last_words() {
        let store = ctx_store().await;
        let cfg = cfg();
        handle(HookEvent::UserPromptSubmit, input(json!({"prompt": "add refunds"})), ctx(&store, &cfg)).await.unwrap();
        let edit = json!({"tool_name": "Edit", "tool_input": {"file_path": "/repo/a.rs"}});
        handle(HookEvent::PostToolUse, input(edit), ctx(&store, &cfg)).await.unwrap();
        let stop = json!({"last_assistant_message": "Endpoint done, tests next."});
        handle(HookEvent::Stop, input(stop), ctx(&store, &cfg)).await.unwrap();
        let compact = json!({"trigger": "auto", "compact_summary": "Refund endpoint built."});
        handle(HookEvent::PostCompact, input(compact), ctx(&store, &cfg)).await.unwrap();
        handle(HookEvent::SessionEnd, input(json!({"reason": "logout"})), ctx(&store, &cfg)).await.unwrap();
        let cps = store.checkpoints_latest("p", 5).await.unwrap();
        assert_eq!(cps[0].source.as_deref(), Some("claude-code (auto)"));

        let md = brief::build(&store, "p", None, None, None).await.unwrap().markdown;
        assert!(md.contains("## Last words from the previous session"), "{md}");
        assert!(md.contains("> Endpoint done, tests next."), "{md}");
        assert!(md.contains("> Refund endpoint built."), "{md}");
    }

    #[tokio::test]
    async fn fallback_checkpoint_includes_last_message() {
        let store = ctx_store().await;
        let cfg = cfg();
        let edit = json!({"tool_name": "Edit", "tool_input": {"file_path": "/repo/a.rs"}});
        handle(HookEvent::PostToolUse, input(edit), ctx(&store, &cfg)).await.unwrap();
        let stop = json!({"last_assistant_message": "Left off at the refund validation."});
        handle(HookEvent::Stop, input(stop), ctx(&store, &cfg)).await.unwrap();
        handle(HookEvent::SessionEnd, input(json!({})), ctx(&store, &cfg)).await.unwrap();
        let notes = store.checkpoints_latest("p", 1).await.unwrap()[0].notes.clone().unwrap();
        assert!(notes.contains("The agent's last message:\n\n> Left off at the refund validation."), "{notes}");
    }

    #[tokio::test]
    async fn git_events_are_logged_and_validated() {
        let store = ctx_store().await;
        let cfg = cfg();
        let content = "a1b2c3d on main: Add refunds (3 files)";

        assert!(HookEvent::Git.queueable());
        assert!(!HookEvent::Git.needs_git());
        assert_eq!(HookEvent::Git.timeout_secs(), 3);

        let out = handle(
            HookEvent::Git,
            input(json!({
                "git_event": "commit",
                "content": content,
                "data": {"sha": "a1b2c3d4e5f6", "branch": "main", "subject": "Add refunds", "files": 3},
            })),
            ctx(&store, &cfg),
        )
        .await
        .unwrap();
        assert!(out.is_none(), "git hooks produce no output");

        let events = store.events_recent("p", None, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "commit");
        assert_eq!(events[0].content, content);
        assert_eq!(events[0].data.as_ref().unwrap()["files"], 3);
        assert_eq!(events[0].source.as_deref(), Some("claude-code"));

        let err = handle(HookEvent::Git, input(json!({"git_event": "push", "content": "nope"})), ctx(&store, &cfg))
            .await
            .unwrap_err();
        assert!(matches!(err, memdusa_core::Error::Invalid(_)), "unknown git_event rejected: {err}");
        assert_eq!(store.events_recent("p", None, 10).await.unwrap().len(), 1, "nothing recorded");
    }

    fn recalled(out: Option<String>) -> Option<String> {
        let v: Value = serde_json::from_str(&out?).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        Some(v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().to_owned())
    }

    async fn prompt(store: &Store, cfg: &HookConfig, session: &str, text: &str) -> Option<String> {
        let mut c = ctx(store, cfg);
        c.session = Some(session.into());
        recalled(handle(HookEvent::UserPromptSubmit, input(json!({ "prompt": text })), c).await.unwrap())
    }

    async fn save(store: &Store, kind: MemoryKind, content: &str) -> Memory {
        store
            .memory_save(NewMemory { project: Some("p".into()), kind, content: content.into(), ..Default::default() })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn prompts_recall_relevant_memories_once_per_session() {
        let store = ctx_store().await;
        let cfg = cfg();
        let decision = save(&store, MemoryKind::Decision, "We chose Postgres over MySQL for the billing ledger").await;
        save(&store, MemoryKind::Note, "Lunch is at noon on fridays").await;
        for i in 0..6 {
            save(&store, MemoryKind::Fact, &format!("filler fact {i} about gardening")).await;
        }

        // unrelated prompts, and one with a single stray word, inject nothing
        assert!(prompt(&store, &cfg, "s1", "please rename the button").await.is_none());
        assert!(prompt(&store, &cfg, "s1", "the and for with").await.is_none(), "only stopwords");
        assert!(prompt(&store, &cfg, "s1", "which database should hold wombats").await.is_none());

        // a prompt about the saved decision gets it
        let text = prompt(&store, &cfg, "s1", "Why did we pick postgres for the billing service?").await.unwrap();
        assert!(text.starts_with("<memdusa-recall>\nRelevant memories:\n- (decision) We chose Postgres"), "{text}");
        assert!(text.ends_with("</memdusa-recall>"), "{text}");
        assert!(!text.contains("Lunch"), "{text}");

        // recorded, and not injected again in the same session...
        let events = store.events_recent_kind("p", Some("s1"), MEMORY_RECALL, 10).await.unwrap();
        assert_eq!(events[0].data.as_ref().unwrap()["ids"], json!([decision.id]));
        assert!(prompt(&store, &cfg, "s1", "remind me why postgres for the billing ledger").await.is_none());
        // ...but a different session still gets it
        assert!(prompt(&store, &cfg, "s2", "remind me why postgres for the billing ledger").await.is_some());

        // the brief doesn't show the bookkeeping
        let b = memdusa_core::brief::build(&store, "p", None, None, None).await.unwrap();
        assert!(!b.markdown.contains(MEMORY_RECALL), "{}", b.markdown);
    }

    #[tokio::test]
    async fn memories_in_the_session_brief_are_not_recalled() {
        let store = ctx_store().await;
        let cfg = cfg();
        save(&store, MemoryKind::Decision, "We chose Postgres over MySQL for the billing ledger").await;
        let mut c = ctx(&store, &cfg);
        c.session = Some("s1".into());
        let out = handle(HookEvent::SessionStart, input(json!({"source": "startup"})), c).await.unwrap();
        assert!(out.unwrap().contains("billing ledger"), "brief carries the memory");
        assert!(prompt(&store, &cfg, "s1", "why postgres for the billing ledger").await.is_none());
        assert!(prompt(&store, &cfg, "other", "why postgres for the billing ledger").await.is_some());
    }

    #[tokio::test]
    async fn recall_can_be_switched_off() {
        let store = ctx_store().await;
        let cfg = HookConfig { recall: false, ..cfg() };
        save(&store, MemoryKind::Decision, "We chose Postgres over MySQL for the billing ledger").await;
        assert!(prompt(&store, &cfg, "s1", "why postgres for the billing ledger").await.is_none());
        assert!(store.events_recent_kind("p", Some("s1"), MEMORY_RECALL, 10).await.unwrap().is_empty());
        // still journaled
        assert_eq!(store.events_recent("p", Some("s1"), 10).await.unwrap()[0].kind, "prompt");
    }
}
