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
//!
//! A hook must never break the session: every failure is logged to stderr and
//! the process exits 0 with no output.

use std::path::Path;

use memdusa_core::brief::{self, COMMAND, FILE_CHANGE};
use memdusa_core::model::*;
use memdusa_core::{Result, Store};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PostToolUse,
    Stop,
    PreCompact,
    SessionEnd,
}

impl HookEvent {
    pub const ALL: [HookEvent; 6] =
        [Self::SessionStart, Self::UserPromptSubmit, Self::PostToolUse, Self::Stop, Self::PreCompact, Self::SessionEnd];

    /// Name of the event in Claude Code settings.
    pub fn claude_name(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PostToolUse => "PostToolUse",
            Self::Stop => "Stop",
            Self::PreCompact => "PreCompact",
            Self::SessionEnd => "SessionEnd",
        }
    }

    /// CLI argument (`memdusa hook <arg>`).
    pub fn arg(self) -> &'static str {
        match self {
            Self::SessionStart => "session-start",
            Self::UserPromptSubmit => "user-prompt-submit",
            Self::PostToolUse => "post-tool-use",
            Self::Stop => "stop",
            Self::PreCompact => "pre-compact",
            Self::SessionEnd => "session-end",
        }
    }

    /// Hook timeout in seconds. SessionEnd only gets 1.5s by default; an
    /// explicit timeout raises that budget.
    pub fn timeout_secs(self) -> u64 {
        match self {
            Self::SessionStart | Self::SessionEnd | Self::PreCompact => 10,
            _ => 5,
        }
    }

    /// Events that save or compare code state (so the client reads git only then).
    pub fn needs_git(self) -> bool {
        matches!(self, Self::SessionStart | Self::PreCompact | Self::SessionEnd)
    }

    /// Tool matcher for events that support one.
    pub fn matcher(self) -> Option<&'static str> {
        match self {
            Self::PostToolUse => Some(JOURNALED_TOOLS),
            _ => None,
        }
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
    /// PreCompact: manual | auto
    #[serde(default)]
    pub trigger: Option<String>,
    /// SessionEnd
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    pub agent: String,
    /// Journal user prompts (UserPromptSubmit).
    pub journal_prompts: bool,
    /// Stop guard: ask for a checkpoint when files changed and the last
    /// checkpoint is older than this many minutes. 0 disables the guard.
    pub stop_guard_mins: i64,
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
        cfg: &req.config,
    };
    Ok(HookResponse { output: handle(event, input, ctx).await? })
}

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
    pub cfg: &'a HookConfig,
}

/// Handle one hook event. Returns what to print on stdout (if anything).
pub async fn handle(event: HookEvent, input: HookInput, ctx: Ctx<'_>) -> Result<Option<String>> {
    match event {
        HookEvent::SessionStart => session_start(&ctx, input.source.as_deref().unwrap_or("startup")).await,
        HookEvent::UserPromptSubmit => {
            if ctx.cfg.journal_prompts
                && let Some(p) = input.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty())
            {
                log(&ctx, "prompt", truncate(p, 400), None).await?;
            }
            Ok(None)
        }
        HookEvent::PostToolUse => {
            post_tool_use(&ctx, input.tool_name.as_deref().unwrap_or_default(), input.tool_input.as_ref()).await?;
            Ok(None)
        }
        HookEvent::Stop => stop(&ctx, input.stop_hook_active.unwrap_or(false)).await,
        HookEvent::PreCompact => {
            let trigger = input.trigger.as_deref().unwrap_or("auto");
            auto_checkpoint(&ctx, &format!("context compaction ({trigger})")).await?;
            log(&ctx, "compaction", format!("context compacted ({trigger})"), None).await?;
            Ok(None)
        }
        HookEvent::SessionEnd => {
            let reason = input.reason.as_deref().unwrap_or("other");
            auto_checkpoint(&ctx, &format!("session end ({reason})")).await?;
            log(&ctx, "session_end", format!("session ended ({reason})"), None).await?;
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
        })
        .await
        .map(drop)
}

async fn session_start(ctx: &Ctx<'_>, source: &str) -> Result<Option<String>> {
    // Build the brief before journaling, so it doesn't report its own start.
    let b = brief::build(ctx.store, &ctx.project, ctx.user.as_deref(), ctx.git.as_ref()).await?;
    log(ctx, "session_start", format!("session started ({source})"), None).await?;

    let empty = b.checkpoints.is_empty() && b.active_runs.is_empty() && b.memories.is_empty();
    if empty && b.recent_events.is_empty() {
        return Ok(None);
    }
    let mut md = b.markdown;
    if md.len() > MAX_CONTEXT_CHARS {
        let cut = md.floor_char_boundary(MAX_CONTEXT_CHARS);
        md.truncate(cut);
        md.push_str("\n\n…(truncated; call the memdusa `resume` tool for the full brief)");
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

async fn post_tool_use(ctx: &Ctx<'_>, tool: &str, input: Option<&Value>) -> Result<()> {
    let field = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str);
    match tool {
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
            if let Some(path) = field("file_path").or_else(|| field("notebook_path")) {
                let rel = relative(path, ctx.cwd.as_deref());
                log(ctx, FILE_CHANGE, rel, Some(json!({ "tool": tool, "path": path }))).await?;
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

async fn stop(ctx: &Ctx<'_>, already_continuing: bool) -> Result<Option<String>> {
    let (Some(session), mins) = (&ctx.session, ctx.cfg.stop_guard_mins) else { return Ok(None) };
    if already_continuing || mins <= 0 {
        return Ok(None);
    }
    let last = ctx.store.last_checkpoint_at(&ctx.project).await?;
    if last.is_some_and(|t| now_ms() - t < mins * 60_000) {
        return Ok(None);
    }
    let changed = ctx.store.count_events_since(&ctx.project, session, FILE_CHANGE, last.unwrap_or(0)).await?;
    if changed == 0 {
        return Ok(None);
    }
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
    let last = ctx.store.last_checkpoint_at(&ctx.project).await?.unwrap_or(0);
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
    let last_prompt = events.iter().find(|e| e.kind == "prompt").map(|e| e.content.clone());

    // Carry the previous checkpoint forward so its plan isn't lost.
    let prev = ctx.store.checkpoints_latest(&ctx.project, 1).await?.into_iter().next();
    let goal = prev
        .as_ref()
        .map(|p| p.goal.clone())
        .or_else(|| last_prompt.clone())
        .unwrap_or_else(|| "(unknown: no checkpoint or prompt recorded)".into());
    let mut done = prev.as_ref().map(|p| p.done.clone()).unwrap_or_default();
    done.push(format!("(auto) edited {} file(s) in session {}", files.len(), truncate(session, 8)));
    let mut notes = format!(
        "Automatic checkpoint written by memdusa at {why}: the agent changed files without saving a checkpoint. \
         Verify the state of the listed files before continuing."
    );
    if let Some(p) = &last_prompt {
        notes.push_str(&format!(" Last user request: \"{p}\""));
    }
    ctx.store
        .checkpoint_save(NewCheckpoint {
            project: ctx.project.clone(),
            task: prev.as_ref().and_then(|p| p.task.clone()),
            goal,
            done,
            next: prev.as_ref().map(|p| p.next.clone()).unwrap_or_default(),
            blockers: prev.as_ref().map(|p| p.blockers.clone()).unwrap_or_default(),
            files,
            notes: Some(notes),
            source: Some(format!("{} (auto)", ctx.cfg.agent)),
            git: ctx.git.clone(),
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
        Store::connect(&format!("sqlite://{}", dir.join("t.db").display())).await.unwrap()
    }

    fn cfg() -> HookConfig {
        HookConfig { agent: "claude-code".into(), journal_prompts: true, stop_guard_mins: 15 }
    }

    fn ctx<'a>(store: &'a Store, cfg: &'a HookConfig) -> Ctx<'a> {
        Ctx {
            store,
            project: "p".into(),
            session: Some("s1".into()),
            cwd: Some("/repo".into()),
            user: None,
            git: None,
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
}
