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

use memdusa_core::brief::{self, AGENT_MESSAGE, COMMAND, COMPACT_SUMMARY, FILE_CHANGE, STOP_GUARD};
use memdusa_core::model::*;
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
                // Short line for the timeline; the full request is kept for checkpoints and the brief.
                // (The handoff eval caught this: a 400-char cut dropped half of a task's requirements.)
                log(&ctx, "prompt", truncate(p, 200), Some(json!({ "text": truncate(p, MAX_PROMPT_CHARS) }))).await?;
            }
            Ok(None)
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
            // one path, or several (a Codex patch touching many files)
            let many = input.and_then(|i| i.get("file_paths")).and_then(Value::as_array);
            let paths: Vec<&str> = match many {
                Some(list) => list.iter().filter_map(Value::as_str).collect(),
                None => field("file_path").or_else(|| field("notebook_path")).into_iter().collect(),
            };
            for path in paths {
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
    // The session's requests, oldest first, in full: the first is usually the task itself.
    let prompts: Vec<String> = events
        .iter()
        .rev()
        .filter(|e| e.kind == "prompt")
        .map(|e| e.data.as_ref().and_then(|d| d["text"].as_str()).unwrap_or(&e.content).to_owned())
        .collect();
    let task_line = prompts.first().map(|p| truncate(p.lines().next().unwrap_or(p).trim(), 140));

    // Carry the previous checkpoint forward so its plan isn't lost.
    let prev = ctx.store.checkpoints_latest(&ctx.project, 1).await?.into_iter().next();
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
            task: prev.as_ref().and_then(|p| p.task.clone()),
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
            at: now_ms(),
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

        let md = brief::build(&store, "p", None, None).await.unwrap().markdown;
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
        let md = brief::build(&store, "p", None, None).await.unwrap().markdown;
        assert!(md.contains("Where things stand") && !md.contains("Last words"), "{md}");
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
}
