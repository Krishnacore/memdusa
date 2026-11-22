//! REST client for a remote `memdusa serve`, used by hooks and the CLI in
//! remote mode.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use memdusa_core::model::{
    ApiKey, AuditEvent, AuditQuery, CreatedKey, Event, GitState, Memory, MemoryQuery, NewMemory, ProjectGrant, Role,
    RunWithSteps, Step, User,
};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::config::{self, OAuthTokens, Remote};
use crate::hooks::{HookBatchItem, HookBatchResponse, HookEvent, HookRequest, HookResponse};

/// A failed call to the server. `retryable` means the server couldn't be
/// reached or had a transient problem, as opposed to rejecting the request.
/// `needs_login` means the OAuth grant is gone: hooks must spool the event and
/// exit successfully instead of failing the harness.
#[derive(Debug)]
pub struct RemoteError {
    pub retryable: bool,
    pub needs_login: bool,
    pub message: String,
}

impl RemoteError {
    pub fn unreachable(message: impl Into<String>) -> Self {
        Self { retryable: true, needs_login: false, message: message.into() }
    }

    fn auth(message: impl Into<String>) -> Self {
        Self {
            retryable: true, needs_login: true, message: format!("{} — run `memdusa login` again", message.into())
        }
    }
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RemoteError {}

enum Auth {
    ApiKey(Option<String>),
    OAuth(OAuthTokens),
}

pub struct Client {
    http: reqwest::Client,
    url: String,
    /// A rotated token is written back here so later calls in this process use it.
    auth: Mutex<Auth>,
}

impl Client {
    pub fn new(remote: Remote, timeout: Duration) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(3)))
            .user_agent(concat!("memdusa/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let url = remote.url.clone();
        let auth = match remote.oauth {
            Some(tokens) => Auth::OAuth(tokens),
            None => Auth::ApiKey(remote.token),
        };
        Ok(Self { http, url, auth: Mutex::new(auth) })
    }

    /// Build from the current config file: another hook may have rotated the
    /// refresh token since this process started.
    pub fn from_config(timeout: Duration) -> anyhow::Result<Self> {
        let remote = config::remote()?.context("not logged in to a remote server; run `memdusa login`")?;
        Self::new(remote, timeout)
    }

    fn bearer(&self) -> Option<String> {
        match &*self.auth.lock().expect("auth lock poisoned") {
            Auth::ApiKey(token) => token.clone(),
            Auth::OAuth(tokens) => Some(tokens.access_token.clone()),
        }
    }

    fn is_oauth(&self) -> bool {
        matches!(&*self.auth.lock().expect("auth lock poisoned"), Auth::OAuth(_))
    }

    fn near_expiry(&self) -> bool {
        match &*self.auth.lock().expect("auth lock poisoned") {
            Auth::OAuth(tokens) => {
                tokens.expires_at <= memdusa_core::model::now_ms() + crate::cli_auth::REFRESH_SKEW_MS
            }
            Auth::ApiKey(_) => false,
        }
    }

    /// Refresh once under a cross-process lock and update our in-memory token.
    async fn refresh(&self) -> Result<String, RemoteError> {
        let token =
            crate::cli_auth::access_token(&self.url, true).await.map_err(|e| RemoteError::auth(e.to_string()))?;
        if let Ok(Some(remote)) = config::remote()
            && let Some(tokens) = remote.oauth
        {
            *self.auth.lock().expect("auth lock poisoned") = Auth::OAuth(tokens);
        }
        Ok(token)
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let mut req = self.http.request(method, format!("{}{path}", self.url));
        if let Some(token) = self.bearer() {
            req = req.bearer_auth(token);
        }
        req
    }

    async fn ensure_success(&self, resp: Response) -> Result<Response, RemoteError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let url = &self.url;
        let body: Value = resp.json().await.unwrap_or_default();
        let msg = body.get("error").and_then(Value::as_str).unwrap_or("no details");
        Err(RemoteError {
            // the server is struggling (or rate limiting): try again later
            retryable: status.is_server_error() || status.as_u16() == 429,
            needs_login: false,
            message: format!("{url} returned {status}: {msg}"),
        })
    }

    /// Replace (not append to) the old bearer header. `bearer_auth` appends,
    /// which would leave the stale token in the retried request too.
    fn with_bearer(&self, req: RequestBuilder, token: &str) -> Result<reqwest::Request, RemoteError> {
        let mut request = req.build().map_err(|e| RemoteError::unreachable(e.to_string()))?;
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| RemoteError::unreachable(format!("bad access token header: {e}")))?;
        request.headers_mut().insert(AUTHORIZATION, value);
        Ok(request)
    }

    /// Refresh before an expired token is even used, and retry once on 401.
    async fn send_raw(&self, req: RequestBuilder) -> Result<Response, RemoteError> {
        let retry = req.try_clone(); // None only for streaming bodies (we never use those)
        if self.near_expiry() {
            let token = self.refresh().await?;
            let request = self.with_bearer(retry.unwrap_or(req), &token)?;
            let resp = self
                .http
                .execute(request)
                .await
                .map_err(|e| RemoteError::unreachable(format!("contacting {}: {e}", self.url)))?;
            return self.ensure_success(resp).await;
        }
        let resp = req.send().await.map_err(|e| RemoteError::unreachable(format!("contacting {}: {e}", self.url)))?;
        if resp.status() == StatusCode::UNAUTHORIZED
            && self.is_oauth()
            && let Some(retry_req) = retry
        {
            let token = self.refresh().await?;
            let request = self.with_bearer(retry_req, &token)?;
            let resp = self
                .http
                .execute(request)
                .await
                .map_err(|e| RemoteError::unreachable(format!("contacting {}: {e}", self.url)))?;
            return self.ensure_success(resp).await;
        }
        self.ensure_success(resp).await
    }

    async fn send<T: DeserializeOwned>(&self, req: RequestBuilder) -> Result<T, RemoteError> {
        let url = self.url.clone();
        let resp = self.send_raw(req).await?;
        resp.json().await.map_err(|e| RemoteError {
            retryable: false,
            needs_login: false,
            message: format!("bad response from {url}: {e}"),
        })
    }

    pub async fn whoami(&self) -> anyhow::Result<Value> {
        Ok(self.send(self.request(Method::GET, "/v1/whoami")).await?)
    }

    pub async fn hook(&self, event: HookEvent, req: &HookRequest) -> Result<HookResponse, RemoteError> {
        self.send(self.request(Method::POST, &format!("/v1/hooks/{}", event.arg())).json(req)).await
    }

    /// Replay queued hooks in one request, in order.
    pub async fn hook_batch(&self, items: &[HookBatchItem]) -> Result<HookBatchResponse, RemoteError> {
        self.send(self.request(Method::POST, "/v1/hooks/batch").json(&json!({ "items": items }))).await
    }

    /// Recent events of a project, newest first, optionally of one session.
    pub async fn events(&self, project: &str, session: Option<&str>, limit: i64) -> anyhow::Result<Vec<Event>> {
        let mut q = vec![("project", project.to_owned()), ("limit", limit.to_string())];
        if let Some(s) = session {
            q.push(("session", s.to_owned()));
        }
        Ok(self.send(self.request(Method::GET, "/v1/events").query(&q)).await?)
    }

    /// The brief as JSON (its `markdown` field holds the rendered text).
    pub async fn resume(&self, project: &str, git: Option<&GitState>, task: Option<&str>) -> anyhow::Result<Value> {
        let mut q = vec![("project", project.to_owned())];
        if let Some(g) = git {
            q.push(("git", serde_json::to_string(g)?));
        }
        if let Some(t) = task {
            q.push(("task", t.to_owned()));
        }
        Ok(self.send(self.request(Method::GET, "/v1/resume").query(&q)).await?)
    }

    /// Delegate `tasks` as one durable run (a pending step per task).
    pub async fn delegate(
        &self,
        project: &str,
        name: &str,
        tasks: &[String],
        idempotency_key: Option<&str>,
    ) -> anyhow::Result<RunWithSteps> {
        let body = json!({ "project": project, "name": name, "tasks": tasks, "idempotency_key": idempotency_key });
        Ok(self.send(self.request(Method::POST, "/v1/delegate").json(&body)).await?)
    }

    /// A worker marks its task done (`error: None`) or failed.
    pub async fn task_finish(
        &self,
        run: &str,
        task: &str,
        outcome: Result<Option<Value>, String>,
    ) -> anyhow::Result<Step> {
        let base = format!("/v1/runs/{}/steps/{}", path_segment(run), path_segment(task));
        Ok(match outcome {
            Ok(output) => {
                let body = json!({ "output": output });
                self.send(self.request(Method::POST, &format!("{base}/complete")).json(&body)).await?
            }
            Err(error) => {
                let body = json!({ "error": error });
                self.send(self.request(Method::POST, &format!("{base}/fail")).json(&body)).await?
            }
        })
    }

    pub async fn memory_save(&self, m: &NewMemory) -> anyhow::Result<Memory> {
        Ok(self.send(self.request(Method::POST, "/v1/memories").json(m)).await?)
    }

    pub async fn memory_search(&self, q: &MemoryQuery) -> anyhow::Result<Vec<Memory>> {
        Ok(self.send(self.request(Method::GET, "/v1/memories").query(&Query(q))).await?)
    }

    pub async fn memory_review(&self, project: &str) -> anyhow::Result<memdusa_core::review::MemoryReview> {
        Ok(self.send(self.request(Method::GET, "/v1/memories/review").query(&[("project", project)])).await?)
    }

    /// Embed memories missing a vector on the server (admin); the server's embedding config decides.
    /// With `project`, only that project's memories are embedded.
    pub async fn embeddings_backfill(
        &self,
        all: bool,
        project: Option<&str>,
    ) -> anyhow::Result<memdusa_core::embed::BackfillReport> {
        let body = json!({ "all": all, "project": project });
        // embedding a big backlog takes a while
        let req = self.request(Method::POST, "/v1/embeddings/backfill").json(&body).timeout(Duration::from_secs(600));
        Ok(self.send(req).await?)
    }

    pub async fn prune(
        &self,
        policy: &memdusa_core::retention::RetentionPolicy,
        project: Option<&str>,
        dry_run: bool,
    ) -> anyhow::Result<memdusa_core::retention::PruneReport> {
        let body = json!({ "policy": policy, "project": project, "dry_run": dry_run });
        Ok(self.send(self.request(Method::POST, "/v1/admin/prune").json(&body)).await?)
    }

    /// JSON Lines export (admin).
    pub async fn export(&self, project: Option<&str>) -> anyhow::Result<String> {
        let mut req = self.request(Method::GET, "/v1/admin/export");
        if let Some(p) = project {
            req = req.query(&[("project", p)]);
        }
        let resp = self.send_raw(req).await?;
        Ok(resp.text().await?)
    }

    pub async fn import(&self, jsonl: &str) -> anyhow::Result<memdusa_core::transfer::ImportReport> {
        let req = self.request(Method::POST, "/v1/admin/import").header("content-type", "application/x-ndjson");
        Ok(self.send(req.body(jsonl.to_owned())).await?)
    }

    // ------------------------------------------------------------ access control

    pub async fn grant_create(&self, user: &str, pattern: &str, role: Role) -> anyhow::Result<ProjectGrant> {
        let body = json!({ "user": user, "pattern": pattern, "role": role.as_str() });
        Ok(self.send(self.request(Method::POST, "/v1/admin/grants").json(&body)).await?)
    }

    pub async fn grants(&self, user: Option<&str>) -> anyhow::Result<Vec<ProjectGrant>> {
        let mut req = self.request(Method::GET, "/v1/admin/grants");
        if let Some(u) = user {
            req = req.query(&[("user", u)]);
        }
        Ok(self.send(req).await?)
    }

    pub async fn grant_revoke(&self, id: &str) -> anyhow::Result<ProjectGrant> {
        let path = format!("/v1/admin/grants/{}", path_segment(id));
        Ok(self.send(self.request(Method::DELETE, &path)).await?)
    }

    pub async fn audit(&self, q: &AuditQuery) -> anyhow::Result<Vec<AuditEvent>> {
        Ok(self.send(self.request(Method::GET, "/v1/admin/audit").query(q)).await?)
    }

    // ------------------------------------------------------------ users & keys

    pub async fn user_list(&self) -> anyhow::Result<Vec<User>> {
        Ok(self.send(self.request(Method::GET, "/v1/admin/users")).await?)
    }

    pub async fn user_add(
        &self,
        name: &str,
        admin: bool,
        key_name: &str,
    ) -> anyhow::Result<crate::http::NewUserResponse> {
        let body = json!({ "name": name, "admin": admin, "key_name": key_name });
        Ok(self.send(self.request(Method::POST, "/v1/admin/users").json(&body)).await?)
    }

    pub async fn user_set_disabled(&self, name: &str, disabled: bool) -> anyhow::Result<User> {
        let action = if disabled { "disable" } else { "enable" };
        Ok(self.send(self.request(Method::POST, &format!("/v1/admin/users/{name}/{action}"))).await?)
    }

    pub async fn identity_list(&self, user: &str) -> anyhow::Result<Vec<memdusa_core::oauth::UserIdentity>> {
        Ok(self.send(self.request(Method::GET, &format!("/v1/admin/users/{}/identities", path_segment(user)))).await?)
    }

    pub async fn identity_link(
        &self,
        user: &str,
        provider: &str,
        identity: &str,
    ) -> anyhow::Result<memdusa_core::oauth::UserIdentity> {
        let body = json!({ "provider": provider, "identity": identity });
        Ok(self
            .send(self.request(Method::POST, &format!("/v1/admin/users/{}/identities", path_segment(user))).json(&body))
            .await?)
    }

    pub async fn identity_unlink(
        &self,
        user: &str,
        provider: &str,
        identity: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let path = format!(
            "/v1/admin/users/{}/identities/{}/{}",
            path_segment(user),
            path_segment(provider),
            path_segment(identity)
        );
        Ok(self.send(self.request(Method::DELETE, &path)).await?)
    }

    /// `user: None` = the caller's own keys.
    pub async fn key_list(&self, user: Option<&str>) -> anyhow::Result<Vec<ApiKey>> {
        match user {
            None => Ok(self.send(self.request(Method::GET, "/v1/me/keys")).await?),
            Some(u) => Ok(self.send(self.request(Method::GET, "/v1/admin/keys").query(&[("user", u)])).await?),
        }
    }

    pub async fn key_create(&self, user: Option<&str>, name: &str) -> anyhow::Result<CreatedKey> {
        let path = match user {
            None => "/v1/me/keys".to_string(),
            Some(u) => format!("/v1/admin/users/{u}/keys"),
        };
        Ok(self.send(self.request(Method::POST, &path).json(&json!({ "name": name }))).await?)
    }

    /// Admins can revoke anyone's key; others only their own.
    pub async fn key_revoke(&self, key: &str) -> anyhow::Result<ApiKey> {
        let admin = self.whoami().await?["admin"].as_bool().unwrap_or(false);
        let path = if admin { format!("/v1/admin/keys/{key}") } else { format!("/v1/me/keys/{key}") };
        Ok(self.send(self.request(Method::DELETE, &path)).await?)
    }

    // -------------------------------------------------------------- sessions

    /// `user: None` = the caller's own OAuth sessions.
    pub async fn sessions(&self, user: Option<&str>) -> anyhow::Result<Vec<memdusa_core::oauth::OAuthSession>> {
        let path = match user {
            None => "/v1/me/sessions".to_string(),
            Some(u) => format!("/v1/admin/sessions?user={}", path_segment(u)),
        };
        Ok(self.send(self.request(Method::GET, &path)).await?)
    }

    pub async fn session_revoke(&self, id: &str) -> anyhow::Result<Value> {
        let admin = self.whoami().await?["admin"].as_bool().unwrap_or(false);
        let path = if admin {
            format!("/v1/admin/sessions/{}", path_segment(id))
        } else {
            format!("/v1/me/sessions/{}", path_segment(id))
        };
        Ok(self.send(self.request(Method::DELETE, &path)).await?)
    }
}

/// A value as one URL path segment (everything but unreserved characters percent-encoded).
fn path_segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `MemoryQuery` as URL query parameters (enums as their snake_case names).
struct Query<'a>(&'a MemoryQuery);

impl Serialize for Query<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let q = self.0;
        let mut m = s.serialize_map(None)?;
        if let Some(v) = &q.query {
            m.serialize_entry("query", v)?;
        }
        if let Some(v) = &q.project {
            m.serialize_entry("project", v)?;
        }
        if let Some(v) = q.kind {
            m.serialize_entry("kind", v.as_str())?;
        }
        if let Some(v) = q.limit {
            m.serialize_entry("limit", &v)?;
        }
        m.end()
    }
}
