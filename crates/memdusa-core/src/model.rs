//! Domain types shared by the store, the HTTP API and the MCP server.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Unix epoch milliseconds.
pub type Millis = i64;

pub fn now_ms() -> Millis {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or_default()
}

pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

// ---------------------------------------------------------------- memory

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// Something true about the project or world ("API uses JWT auth").
    #[default]
    Fact,
    /// How the user likes things done ("prefers small PRs").
    Preference,
    /// A choice that was made and why ("chose SQLite for local mode because ...").
    Decision,
    /// Something to come back to later.
    Todo,
    /// Anything else.
    Note,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Preference => "preference",
            Self::Decision => "decision",
            Self::Todo => "todo",
            Self::Note => "note",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "preference" => Self::Preference,
            "decision" => Self::Decision,
            "todo" => Self::Todo,
            "note" => Self::Note,
            _ => Self::Fact,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Memory {
    pub id: String,
    /// `None` = global memory, visible from every project (to its owner).
    pub project: Option<String>,
    pub kind: MemoryKind,
    pub content: String,
    pub tags: Vec<String>,
    /// Which agent/harness wrote it (`user/agent` on a multi-user server).
    pub source: Option<String>,
    /// Authenticated user who wrote it. Global memories are private to them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub created_at: Millis,
    pub updated_at: Millis,
    /// Only set on the result of a save: it was folded into an existing
    /// near-duplicate memory instead of creating a new one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub merged: bool,
}

/// A partial update of a memory: only the `Some` fields change.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct MemoryPatch {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub kind: Option<MemoryKind>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Set by the server from the authenticated caller; never from the request.
    #[serde(skip)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NewMemory {
    /// Project key. Omit (or set `global: true`) for a memory shared across all projects.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub kind: MemoryKind,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub source: Option<String>,
    /// Set by the server from the authenticated caller; never from the request.
    #[serde(skip)]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct MemoryQuery {
    /// Case-insensitive text match against content and tags. Empty = list recent.
    #[serde(default)]
    pub query: Option<String>,
    /// Restrict to this project (global memories are always included).
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub kind: Option<MemoryKind>,
    #[serde(default)]
    pub limit: Option<i64>,
    /// Caller identity, set by the server: whose global memories are visible.
    #[serde(skip)]
    pub user: Option<String>,
}

// ---------------------------------------------------------------- users & API keys

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct User {
    pub id: String,
    pub name: String,
    pub admin: bool,
    pub created_at: Millis,
    pub disabled_at: Option<Millis>,
}

/// Metadata of an API key. The secret itself is only returned once, at creation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApiKey {
    pub id: String,
    pub user: String,
    /// Label such as "laptop" or "ci".
    pub name: String,
    /// First characters of the key, to recognize it in listings.
    pub prefix: String,
    pub created_at: Millis,
    pub last_used_at: Option<Millis>,
    pub revoked_at: Option<Millis>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CreatedKey {
    #[serde(flatten)]
    pub key: ApiKey,
    /// The full key. Shown once; only its hash is stored.
    pub secret: String,
}

/// Who is making a request, as resolved from its credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// `None` for the shared server token or an open (no-auth) server.
    pub user: Option<String>,
    pub admin: bool,
}

impl Identity {
    /// Full-access identity used by an open server, the shared token and
    /// local (direct database) access.
    pub fn root() -> Self {
        Self { user: None, admin: true }
    }

    /// Qualify an agent name with the user: `claude-code` -> `alice/claude-code`.
    /// This is what records and leases are attributed to, so two users'
    /// agents with the same name never share a lease.
    pub fn qualify(&self, agent: &str) -> String {
        match &self.user {
            Some(u) => format!("{u}/{agent}"),
            None => agent.to_owned(),
        }
    }
}

// ---------------------------------------------------------------- journal

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Event {
    pub id: String,
    pub project: String,
    pub session: Option<String>,
    pub source: Option<String>,
    /// Free-form type, e.g. `message`, `tool_call`, `file_change`, `commit`, `note`.
    pub kind: String,
    pub content: String,
    pub data: Option<Value>,
    pub created_at: Millis,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NewEvent {
    pub project: String,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    pub kind: String,
    pub content: String,
    #[serde(default)]
    pub data: Option<Value>,
    /// When it happened (defaults to now; future times are clamped to now).
    /// Lets queued events keep their original time when replayed.
    #[serde(default)]
    pub created_at: Option<Millis>,
}

// ---------------------------------------------------------------- checkpoints

/// A snapshot of where a piece of work stands. The latest checkpoint per
/// project is the heart of the hand-off brief.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Checkpoint {
    pub id: String,
    pub project: String,
    pub task: Option<String>,
    pub goal: String,
    pub done: Vec<String>,
    pub next: Vec<String>,
    pub blockers: Vec<String>,
    pub files: Vec<String>,
    pub notes: Option<String>,
    pub source: Option<String>,
    /// Code state when the checkpoint was saved (captured on the client).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitState>,
    pub created_at: Millis,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NewCheckpoint {
    pub project: String,
    /// Optional task name, to track several workstreams in one project.
    #[serde(default)]
    pub task: Option<String>,
    pub goal: String,
    #[serde(default)]
    pub done: Vec<String>,
    #[serde(default)]
    pub next: Vec<String>,
    #[serde(default)]
    pub blockers: Vec<String>,
    /// Files that matter for picking the work back up.
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    /// Code state; filled in automatically by hooks, the CLI and the MCP proxy.
    #[serde(default)]
    pub git: Option<GitState>,
    /// Defaults to now; future times are clamped to now.
    #[serde(default)]
    pub created_at: Option<Millis>,
}

/// Where the code was when something was recorded. Lets a resuming agent
/// notice it's looking at a different branch, commit or machine than the one
/// the notes describe.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitState {
    /// `None` when detached.
    #[serde(default)]
    pub branch: Option<String>,
    /// Full commit SHA; `None` in a repo without commits.
    #[serde(default)]
    pub head: Option<String>,
    /// Number of modified/untracked files.
    #[serde(default)]
    pub dirty_count: i64,
    /// Up to 20 of them.
    #[serde(default)]
    pub dirty_files: Vec<String>,
    /// Machine name: uncommitted work only exists there.
    #[serde(default)]
    pub host: Option<String>,
    /// Absolute path of the worktree root (`git rev-parse --show-toplevel`).
    /// Two worktrees of one repo share a project key but not their code state.
    #[serde(default)]
    pub worktree: Option<String>,
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

fn file_list(files: &[String], total: i64) -> String {
    let shown: Vec<&str> = files.iter().take(5).map(String::as_str).collect();
    let more = total - shown.len() as i64;
    if more > 0 { format!("{}, +{more} more", shown.join(", ")) } else { shown.join(", ") }
}

impl GitState {
    /// One line for the brief: `` `feat/x` @ `a1b2c3d` · 3 uncommitted (a.rs, b.rs, c.rs) · on `mbp` ``
    pub fn summary(&self) -> String {
        let mut parts = vec![];
        match (&self.branch, &self.head) {
            (Some(b), Some(h)) => parts.push(format!("`{b}` @ `{}`", short(h))),
            (None, Some(h)) => parts.push(format!("detached @ `{}`", short(h))),
            (Some(b), None) => parts.push(format!("`{b}` (no commits)")),
            (None, None) => {}
        }
        if self.dirty_count > 0 {
            parts.push(format!(
                "{} uncommitted ({})",
                self.dirty_count,
                file_list(&self.dirty_files, self.dirty_count)
            ));
        } else {
            parts.push("clean".into());
        }
        if let Some(h) = &self.host {
            parts.push(format!("on `{h}`"));
        }
        parts.join(" · ")
    }

    /// Differences between the code state a checkpoint was saved in (`self`)
    /// and the resuming agent's (`current`) that could make the notes wrong.
    pub fn drift(&self, current: &GitState) -> Vec<String> {
        let mut w = vec![];
        let other_host = matches!((&self.host, &current.host), (Some(a), Some(b)) if a != b);
        if other_host && self.dirty_count > 0 {
            w.push(format!(
                "The checkpoint was saved on `{}` with {} uncommitted file(s) ({}). Those changes may exist only on that \
                 machine: check before redoing or overwriting that work.",
                self.host.as_deref().unwrap_or("?"),
                self.dirty_count,
                file_list(&self.dirty_files, self.dirty_count)
            ));
        }
        match ((&self.branch, &self.head), (&current.branch, &current.head)) {
            ((Some(a), _), (Some(b), _)) if a != b => {
                w.push(format!("The checkpoint was on branch `{a}`, but you're on `{b}`."))
            }
            ((_, Some(a)), (_, Some(b))) if a != b => w.push(format!(
                "HEAD moved since the checkpoint: `{}` → `{}` (new commits, a rebase, or a different checkout).",
                short(a),
                short(b)
            )),
            _ => {}
        }
        if !other_host && self.head == current.head && self.dirty_count > 0 && current.dirty_count == 0 {
            w.push(format!(
                "{} file(s) uncommitted at checkpoint time are no longer modified: they may have been committed \
                 elsewhere, stashed or discarded.",
                self.dirty_count
            ));
        }
        w
    }
}

// ---------------------------------------------------------------- durable execution

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Running,
        }
    }
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Created by a coordinator (`run_delegate`), not started by anyone yet.
    Pending,
    Running,
    Completed,
    Failed,
}

impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => Self::Running,
        }
    }
}

/// Marker in a run's `input` that tells delegated runs (steps are tasks with their own leases) from
/// single-agent durable runs (one lease on the whole run).
pub const DELEGATED_MARKER: &str = "memdusa_delegated";

/// Whether a run's stored `input` marks it as created by `run_delegate`.
pub fn is_delegated_input(input: Option<&Value>) -> bool {
    input.and_then(|i| i.get(DELEGATED_MARKER)).and_then(Value::as_bool).unwrap_or(false)
}

/// `MEMDUSA_RUN=<run> MEMDUSA_TASK=<task>`: what launches a worker on one task of a delegated run.
pub fn worker_env_line(run_id: &str, task: &str) -> String {
    let quote = |s: &str| {
        if s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@')) {
            s.to_owned()
        } else {
            format!("'{}'", s.replace('\'', r"'\''"))
        }
    };
    format!("MEMDUSA_RUN={} MEMDUSA_TASK={}", quote(run_id), quote(task))
}

/// A durable, resumable unit of multi-step work. Steps are journaled so that
/// after a crash (or a switch to another harness) completed steps are replayed
/// from the journal instead of being executed again.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Run {
    pub id: String,
    pub project: String,
    pub name: String,
    pub idempotency_key: Option<String>,
    pub status: RunStatus,
    pub input: Option<Value>,
    pub output: Option<Value>,
    pub error: Option<String>,
    /// Agent currently holding the lease.
    pub owner: Option<String>,
    pub lease_until: Option<Millis>,
    pub created_at: Millis,
    pub updated_at: Millis,
    /// True when the run is `running` but its lease expired: the owner most
    /// likely crashed or the session ended, and anyone may claim it.
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Step {
    pub run_id: String,
    pub key: String,
    pub seq: i64,
    pub status: StepStatus,
    pub input: Option<Value>,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub attempts: i64,
    pub started_at: Millis,
    pub completed_at: Option<Millis>,
    /// Worker holding the step's lease (delegated runs). Kept after completion as "done by".
    #[serde(default)]
    pub owner: Option<String>,
    /// When the worker's lease expires; cleared when the step completes or fails.
    #[serde(default)]
    pub lease_until: Option<Millis>,
    /// True when the step is `running` under a lease that expired: its worker is gone and anyone may claim it.
    #[serde(default)]
    pub stale: bool,
}

/// Outcome of a worker claiming its step of a delegated run.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ClaimResult {
    /// The step is now running under the caller's lease.
    Claimed {
        attempt: i64,
        /// The previous holder, when the caller took the step over from it.
        previous_owner: Option<String>,
        /// The previous holder's lease had expired (it crashed or was rate limited), as opposed to a forced takeover.
        stale: bool,
        /// Why the previous attempt failed, if it did.
        previous_error: Option<String>,
    },
    /// The step already completed: do NOT redo it.
    AlreadyDone { output: Option<Value> },
    /// Another worker holds a live lease on the step.
    HeldBy { owner: String, lease_until: Millis },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RunWithSteps {
    #[serde(flatten)]
    pub run: Run,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NewRun {
    pub project: String,
    pub name: String,
    /// Starting a run twice with the same key returns the existing run.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub input: Option<Value>,
    #[serde(default)]
    pub owner: Option<String>,
    /// Lease length in seconds (default 600). Renewed by every step call.
    #[serde(default)]
    pub lease_secs: Option<i64>,
}

/// What the caller should do for a step.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum StepDecision {
    /// Step already completed earlier: use this recorded output, do NOT redo it.
    Replay { output: Option<Value> },
    /// Step must be executed now; report back with step_complete / step_fail.
    Execute {
        attempt: i64,
        /// Error from the previous attempt, if this is a retry.
        previous_error: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Completed,
    Failed,
    Cancelled,
}

impl From<RunOutcome> for RunStatus {
    fn from(o: RunOutcome) -> Self {
        match o {
            RunOutcome::Completed => Self::Completed,
            RunOutcome::Failed => Self::Failed,
            RunOutcome::Cancelled => Self::Cancelled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::GitState;

    fn g(branch: &str, head: &str, dirty: &[&str], host: &str) -> GitState {
        GitState {
            branch: Some(branch.into()),
            head: Some(head.into()),
            dirty_count: dirty.len() as i64,
            dirty_files: dirty.iter().map(|s| s.to_string()).collect(),
            host: Some(host.into()),
            worktree: None,
        }
    }

    #[test]
    fn drift_detection() {
        let saved = g("feat/refunds", "a1b2c3d4e5", &["src/refund.ts"], "laptop");
        assert!(saved.drift(&saved).is_empty(), "identical state: no warnings");

        let w = saved.drift(&g("feat/refunds", "a1b2c3d4e5", &[], "desktop"));
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("saved on `laptop`") && w[0].contains("src/refund.ts"), "{w:?}");

        let w = saved.drift(&g("main", "ffffff0000", &["src/refund.ts"], "laptop"));
        assert_eq!(w, vec!["The checkpoint was on branch `feat/refunds`, but you're on `main`."]);

        let w = saved.drift(&g("feat/refunds", "9f8e7d6c5b", &["src/refund.ts"], "laptop"));
        assert!(w[0].contains("`a1b2c3d` → `9f8e7d6`"), "{w:?}");

        let w = saved.drift(&g("feat/refunds", "a1b2c3d4e5", &[], "laptop"));
        assert!(w[0].contains("no longer modified"), "{w:?}");

        let clean = g("main", "a1b2c3d4e5", &[], "laptop");
        assert!(clean.drift(&g("main", "a1b2c3d4e5", &[], "desktop")).is_empty(), "clean tree: machine doesn't matter");
    }

    #[test]
    fn worktree_defaults_and_round_trips() {
        // Old clients/data have no `worktree` field at all.
        let old: GitState = serde_json::from_str(r#"{"branch":"main","host":"mbp"}"#).unwrap();
        assert_eq!(old.worktree, None);
        let g = GitState { worktree: Some("/work/wt-a".into()), ..g("main", "a1b2c3d4e5", &[], "mbp") };
        let json = serde_json::to_string(&g).unwrap();
        assert!(json.contains("/work/wt-a"), "{json}");
        assert_eq!(serde_json::from_str::<GitState>(&json).unwrap().worktree, g.worktree);
        // Worktree alone doesn't create drift when everything else matches.
        assert!(g.drift(&g).is_empty());
    }

    #[test]
    fn summary_line() {
        let many: Vec<String> = (1..=8).map(|i| format!("f{i}.rs")).collect();
        let s = GitState { dirty_count: 8, dirty_files: many, ..g("main", "a1b2c3d4e5", &[], "mbp") };
        assert_eq!(
            s.summary(),
            "`main` @ `a1b2c3d` · 8 uncommitted (f1.rs, f2.rs, f3.rs, f4.rs, f5.rs, +3 more) · on `mbp`"
        );
    }
}
