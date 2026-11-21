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
    Running,
    Completed,
    Failed,
}

impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => Self::Running,
        }
    }
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
