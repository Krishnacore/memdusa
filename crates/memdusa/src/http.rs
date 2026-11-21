//! HTTP server: REST API for custom agents/hooks plus MCP over streamable
//! HTTP (`/mcp`) for remote harnesses and cloud agents.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::{Extension, Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use memdusa_core::model::*;
use memdusa_core::{Error, Store, brief};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::hooks::{self, HookEvent, HookRequest, HookResponse};
use crate::mcp::{Defaults, MemdusaMcp};

#[derive(Clone)]
struct AppState {
    store: Store,
    /// Shared break-glass token (`--token`): full admin access.
    token: Option<Arc<str>>,
    /// Latched to true once any user exists (or a token is set). Users can be
    /// created by the CLI while the server runs, so an open server re-checks
    /// on every request until it latches.
    auth_required: Arc<AtomicBool>,
}

impl AppState {
    async fn requires_auth(&self) -> Result<bool, Error> {
        if self.auth_required.load(Ordering::Relaxed) {
            return Ok(true);
        }
        let required = self.store.user_count().await? > 0;
        if required {
            self.auth_required.store(true, Ordering::Relaxed);
        }
        Ok(required)
    }
}

pub struct HttpOptions {
    pub addr: String,
    pub token: Option<String>,
    pub allowed_hosts: Vec<String>,
}

struct ApiError(Error);

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::Invalid(_) => StatusCode::BAD_REQUEST,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::Db(e) => {
                tracing::error!("database error: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, Json(json!({ "error": self.0.to_string() }))).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

pub async fn serve(store: Store, opts: HttpOptions) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&opts.addr).await?;
    tracing::info!("memdusa listening on http://{}  (REST /v1, MCP /mcp)", listener.local_addr()?);
    let users = store.user_count().await?;
    match (users, &opts.token) {
        (0, None) => {
            tracing::warn!("auth: OPEN (no users, no --token). Create a user with `memdusa user add <name> --admin`")
        }
        (0, Some(_)) => tracing::info!("auth: shared token only"),
        (n, t) => tracing::info!("auth: API keys ({n} users){}", if t.is_some() { " + shared token" } else { "" }),
    }
    axum::serve(listener, app(store, opts)).with_graceful_shutdown(shutdown()).await?;
    Ok(())
}

pub fn app(store: Store, opts: HttpOptions) -> Router {
    let mcp_store = store.clone();
    let mut mcp_config = StreamableHttpServerConfig::default();
    if !opts.allowed_hosts.is_empty() {
        mcp_config = mcp_config.with_allowed_hosts(opts.allowed_hosts.clone());
    }
    let mcp = StreamableHttpService::new(
        move || {
            Ok(MemdusaMcp::new(
                mcp_store.clone(),
                // Remote clients have no cwd: they must name the project.
                Defaults { project: None, agent: "mcp-http".into(), cwd: None },
            ))
        },
        Arc::new(LocalSessionManager::default()),
        mcp_config,
    );

    let state = AppState {
        store,
        auth_required: Arc::new(AtomicBool::new(opts.token.is_some())),
        token: opts.token.map(Into::into),
    };

    let api = Router::new()
        .route("/v1/whoami", get(whoami))
        .route("/v1/me/keys", get(my_keys).post(my_key_create))
        .route("/v1/me/keys/{id}", delete(my_key_revoke))
        .route("/v1/admin/users", get(admin_users).post(admin_user_create))
        .route("/v1/admin/users/{name}/disable", post(admin_user_disable))
        .route("/v1/admin/users/{name}/enable", post(admin_user_enable))
        .route("/v1/admin/users/{name}/keys", post(admin_key_create))
        .route("/v1/admin/keys", get(admin_keys))
        .route("/v1/admin/keys/{id}", delete(admin_key_revoke))
        .route("/v1/hooks/{event}", post(hook))
        .route("/v1/resume", get(resume))
        .route("/v1/memories", post(memory_save).get(memory_search))
        .route("/v1/memories/{id}", delete(memory_forget))
        .route("/v1/events", post(event_append).get(events_recent))
        .route("/v1/checkpoints", post(checkpoint_save).get(checkpoints_latest))
        .route("/v1/runs", post(run_start).get(run_list))
        .route("/v1/runs/{id}", get(run_get))
        .route("/v1/runs/{id}/claim", post(run_claim))
        .route("/v1/runs/{id}/finish", post(run_finish))
        .route("/v1/runs/{id}/steps/{key}/begin", post(step_begin))
        .route("/v1/runs/{id}/steps/{key}/complete", post(step_complete))
        .route("/v1/runs/{id}/steps/{key}/fail", post(step_fail))
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state);

    Router::new().route("/health", get(|| async { "ok" })).merge(api)
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Resolve the caller and attach an [`Identity`] to the request (REST
/// handlers and MCP tools both read it).
async fn auth(State(s): State<AppState>, mut req: Request, next: Next) -> Response {
    let unauthorized = |msg: &str| (StatusCode::UNAUTHORIZED, Json(json!({ "error": msg }))).into_response();
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .map(String::from);

    let identity = match bearer {
        Some(t) if s.token.as_ref().is_some_and(|tok| constant_time_eq(t.as_bytes(), tok.as_bytes())) => {
            Identity::root()
        }
        Some(t) => match s.store.authenticate(&t).await {
            Ok(Some(id)) => id,
            Ok(None) => return unauthorized("invalid, revoked or disabled API key"),
            Err(e) => return ApiError(e).into_response(),
        },
        None => match s.requires_auth().await {
            Ok(true) => return unauthorized("missing bearer token (an API key from `memdusa key create`)"),
            Ok(false) => Identity::root(), // open server: local dev only
            Err(e) => return ApiError(e).into_response(),
        },
    };
    req.extensions_mut().insert(identity);
    next.run(req).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------- handlers

async fn whoami(State(s): State<AppState>, Extension(id): Extension<Identity>) -> Json<Value> {
    let backend = match s.store.backend() {
        memdusa_core::Backend::Sqlite => "sqlite",
        memdusa_core::Backend::Postgres => "postgres",
    };
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "backend": backend,
        "user": id.user,
        "admin": id.admin,
    }))
}

async fn hook(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Path(event): Path<String>,
    Json(req): Json<HookRequest>,
) -> ApiResult<HookResponse> {
    let event: HookEvent = event.parse().map_err(|e: String| Error::Invalid(format!("unknown hook event: {e}")))?;
    Ok(Json(hooks::handle_request(&s.store, event, req, &id).await?))
}

// ---------------------------------------------------------------- keys & users

fn me(id: &Identity) -> Result<&str, Error> {
    id.user.as_deref().ok_or_else(|| {
        Error::Invalid("this credential isn't tied to a user (shared token / open server); use /v1/admin".into())
    })
}

fn admin(id: &Identity) -> Result<(), Error> {
    if id.admin { Ok(()) } else { Err(Error::Forbidden("admin only".into())) }
}

#[derive(Deserialize, Default)]
struct KeyBody {
    #[serde(default)]
    name: String,
}

async fn my_keys(State(s): State<AppState>, Extension(id): Extension<Identity>) -> ApiResult<Vec<ApiKey>> {
    Ok(Json(s.store.key_list(Some(me(&id)?)).await?))
}

async fn my_key_create(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Json(b): Json<KeyBody>,
) -> ApiResult<CreatedKey> {
    Ok(Json(s.store.key_create(me(&id)?, &b.name).await?))
}

async fn my_key_revoke(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Path(key): Path<String>,
) -> ApiResult<ApiKey> {
    Ok(Json(s.store.key_revoke(&key, Some(me(&id)?)).await?))
}

async fn admin_users(State(s): State<AppState>, Extension(id): Extension<Identity>) -> ApiResult<Vec<User>> {
    admin(&id)?;
    Ok(Json(s.store.user_list().await?))
}

#[derive(Deserialize)]
struct NewUserBody {
    name: String,
    #[serde(default)]
    admin: bool,
    #[serde(default)]
    key_name: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct NewUserResponse {
    pub user: User,
    pub key: CreatedKey,
}

async fn admin_user_create(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Json(b): Json<NewUserBody>,
) -> ApiResult<NewUserResponse> {
    admin(&id)?;
    let user = s.store.user_create(&b.name, b.admin).await?;
    let key = s.store.key_create(&user.name, b.key_name.as_deref().unwrap_or("default")).await?;
    s.auth_required.store(true, Ordering::Relaxed);
    Ok(Json(NewUserResponse { user, key }))
}

async fn admin_user_disable(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Path(name): Path<String>,
) -> ApiResult<User> {
    admin(&id)?;
    Ok(Json(s.store.user_set_disabled(&name, true).await?))
}

async fn admin_user_enable(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Path(name): Path<String>,
) -> ApiResult<User> {
    admin(&id)?;
    Ok(Json(s.store.user_set_disabled(&name, false).await?))
}

async fn admin_key_create(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Path(name): Path<String>,
    Json(b): Json<KeyBody>,
) -> ApiResult<CreatedKey> {
    admin(&id)?;
    Ok(Json(s.store.key_create(&name, &b.name).await?))
}

#[derive(Deserialize)]
struct KeysQ {
    user: Option<String>,
}

async fn admin_keys(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Query(q): Query<KeysQ>,
) -> ApiResult<Vec<ApiKey>> {
    admin(&id)?;
    Ok(Json(s.store.key_list(q.user.as_deref()).await?))
}

async fn admin_key_revoke(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Path(key): Path<String>,
) -> ApiResult<ApiKey> {
    admin(&id)?;
    Ok(Json(s.store.key_revoke(&key, None).await?))
}

// ---------------------------------------------------------------- data

#[derive(Deserialize)]
struct ProjectQ {
    project: String,
    session: Option<String>,
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct ResumeQ {
    project: String,
    /// Caller's code state as JSON (`GitState`), to get drift warnings.
    git: Option<String>,
}

async fn resume(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Query(q): Query<ResumeQ>,
) -> ApiResult<memdusa_core::Brief> {
    let git: Option<GitState> =
        q.git.map(|g| serde_json::from_str(&g)).transpose().map_err(|e| Error::Invalid(format!("git: {e}")))?;
    Ok(Json(brief::build(&s.store, &q.project, id.user.as_deref(), git.as_ref()).await?))
}

/// Attribution always comes from the credential, never from the request body.
fn attribute(id: &Identity, source: Option<String>) -> Option<String> {
    source.map(|s| id.qualify(&s)).or_else(|| id.user.clone())
}

async fn memory_save(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Json(mut m): Json<NewMemory>,
) -> ApiResult<Memory> {
    m.source = attribute(&id, m.source);
    m.user = id.user;
    Ok(Json(s.store.memory_save(m).await?))
}

async fn memory_search(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Query(mut q): Query<MemoryQuery>,
) -> ApiResult<Vec<Memory>> {
    q.user = id.user;
    Ok(Json(s.store.memory_search(q).await?))
}

async fn memory_forget(State(s): State<AppState>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    s.store.memory_forget(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn event_append(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Json(mut e): Json<NewEvent>,
) -> ApiResult<Event> {
    e.source = attribute(&id, e.source);
    Ok(Json(s.store.event_append(e).await?))
}

async fn events_recent(State(s): State<AppState>, Query(q): Query<ProjectQ>) -> ApiResult<Vec<Event>> {
    Ok(Json(s.store.events_recent(&q.project, q.session.as_deref(), q.limit.unwrap_or(50)).await?))
}

async fn checkpoint_save(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Json(mut c): Json<NewCheckpoint>,
) -> ApiResult<Checkpoint> {
    c.source = attribute(&id, c.source);
    Ok(Json(s.store.checkpoint_save(c).await?))
}

async fn checkpoints_latest(State(s): State<AppState>, Query(q): Query<ProjectQ>) -> ApiResult<Vec<Checkpoint>> {
    Ok(Json(s.store.checkpoints_latest(&q.project, q.limit.unwrap_or(10)).await?))
}

async fn run_start(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Json(mut r): Json<NewRun>,
) -> ApiResult<RunWithSteps> {
    r.owner = Some(id.qualify(r.owner.as_deref().unwrap_or("anonymous")));
    Ok(Json(s.store.run_start(r).await?))
}

#[derive(Deserialize)]
struct RunListQ {
    project: Option<String>,
    status: Option<RunStatus>,
    limit: Option<i64>,
}

async fn run_list(State(s): State<AppState>, Query(q): Query<RunListQ>) -> ApiResult<Vec<Run>> {
    Ok(Json(s.store.run_list(q.project.as_deref(), q.status, q.limit.unwrap_or(50)).await?))
}

async fn run_get(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<RunWithSteps> {
    Ok(Json(s.store.run_get(&id).await?))
}

#[derive(Deserialize)]
struct ClaimBody {
    owner: String,
    lease_secs: Option<i64>,
    #[serde(default)]
    force: bool,
}

async fn run_claim(
    State(s): State<AppState>,
    Extension(who): Extension<Identity>,
    Path(id): Path<String>,
    Json(b): Json<ClaimBody>,
) -> ApiResult<RunWithSteps> {
    Ok(Json(s.store.run_claim(&id, &who.qualify(&b.owner), b.lease_secs, b.force).await?))
}

#[derive(Deserialize)]
struct FinishBody {
    outcome: RunOutcome,
    output: Option<Value>,
    error: Option<String>,
}

async fn run_finish(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<FinishBody>,
) -> ApiResult<RunWithSteps> {
    Ok(Json(s.store.run_finish(&id, b.outcome, b.output, b.error).await?))
}

#[derive(Deserialize)]
struct BeginBody {
    owner: String,
    input: Option<Value>,
}

async fn step_begin(
    State(s): State<AppState>,
    Extension(who): Extension<Identity>,
    Path((id, key)): Path<(String, String)>,
    Json(b): Json<BeginBody>,
) -> ApiResult<StepDecision> {
    Ok(Json(s.store.step_begin(&id, &key, b.input, &who.qualify(&b.owner)).await?))
}

#[derive(Deserialize)]
struct CompleteBody {
    output: Option<Value>,
}

async fn step_complete(
    State(s): State<AppState>,
    Path((id, key)): Path<(String, String)>,
    Json(b): Json<CompleteBody>,
) -> ApiResult<Step> {
    Ok(Json(s.store.step_complete(&id, &key, b.output).await?))
}

#[derive(Deserialize)]
struct FailBody {
    error: String,
}

async fn step_fail(
    State(s): State<AppState>,
    Path((id, key)): Path<(String, String)>,
    Json(b): Json<FailBody>,
) -> ApiResult<Step> {
    Ok(Json(s.store.step_fail(&id, &key, &b.error).await?))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::config::Remote;
    use crate::hooks::HookConfig;
    use crate::remote::Client;

    async fn spawn(token: Option<&str>) -> (String, Store) {
        let dir = std::env::temp_dir().join(format!("memdusa-http-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::connect(&format!("sqlite://{}", dir.join("t.db").display())).await.unwrap();
        let opts = HttpOptions { addr: String::new(), token: token.map(String::from), allowed_hosts: vec![] };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = app(store.clone(), opts);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, store)
    }

    fn client(url: &str, token: Option<&str>) -> Client {
        Client::new(Remote::new(url, token.map(String::from)).unwrap(), Duration::from_secs(5)).unwrap()
    }

    #[tokio::test]
    async fn remote_hooks_round_trip_with_auth() {
        let (url, store) = spawn(Some("s3cret")).await;
        assert!(client(&url, Some("wrong")).whoami().await.is_err(), "bad token rejected");
        let c = client(&url, Some("s3cret"));
        assert_eq!(c.whoami().await.unwrap()["ok"], true);

        let req = |input| HookRequest {
            project: "github.com/acme/shop".into(),
            config: HookConfig { agent: "claude-code".into(), journal_prompts: true, stop_guard_mins: 15 },
            input,
            git: None,
        };
        let edit =
            json!({"session_id": "A", "cwd": "/r", "tool_name": "Write", "tool_input": {"file_path": "/r/a.rs"}});
        assert!(c.hook(HookEvent::PostToolUse, &req(edit)).await.unwrap().output.is_none());
        let stop = c.hook(HookEvent::Stop, &req(json!({"session_id": "A"}))).await.unwrap();
        assert!(stop.output.unwrap().contains("\"block\""), "stop guard runs server-side");
        c.hook(HookEvent::SessionEnd, &req(json!({"session_id": "A", "reason": "logout"}))).await.unwrap();

        // state landed in the server's store
        let cps = store.checkpoints_latest("github.com/acme/shop", 1).await.unwrap();
        assert_eq!(cps[0].files, vec!["a.rs"]);
        let start = c.hook(HookEvent::SessionStart, &req(json!({"session_id": "B"}))).await.unwrap();
        assert!(start.output.unwrap().contains("a.rs"));

        // unknown events are a clean 400, not a crash
        let r = reqwest::Client::new()
            .post(format!("{url}/v1/hooks/bogus"))
            .bearer_auth("s3cret")
            .json(&req(json!({})))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
    }

    #[tokio::test]
    async fn remote_cli_memory_round_trip() {
        let (url, _) = spawn(None).await;
        let c = client(&url, None);
        let m = NewMemory {
            project: Some("p".into()),
            kind: MemoryKind::Decision,
            content: "use Stripe".into(),
            ..Default::default()
        };
        c.memory_save(&m).await.unwrap();
        let q = MemoryQuery {
            query: Some("stripe".into()),
            project: Some("p".into()),
            kind: Some(MemoryKind::Decision),
            limit: Some(5),
            user: None,
        };
        assert_eq!(c.memory_search(&q).await.unwrap().len(), 1);
        assert!(c.resume("p", None).await.unwrap()["markdown"].as_str().unwrap().contains("use Stripe"));
    }

    /// Raw request helper: (status, body).
    async fn call(method: reqwest::Method, url: &str, key: Option<&str>, body: Option<Value>) -> (u16, Value) {
        let mut r = reqwest::Client::new().request(method, url);
        if let Some(k) = key {
            r = r.bearer_auth(k);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or_default())
    }

    #[tokio::test]
    async fn per_user_keys_end_to_end() {
        use reqwest::Method as M;
        let (url, store) = spawn(Some("break-glass")).await;
        let u = |p: &str| format!("{url}{p}");

        // the shared token still works and is admin
        let (st, me) = call(M::GET, &u("/v1/whoami"), Some("break-glass"), None).await;
        assert_eq!((st, me["admin"].clone(), me["user"].clone()), (200, json!(true), Value::Null));

        // first admin created straight in the DB (as `memdusa user add` on the server would)
        store.user_create("alice", true).await.unwrap();
        let alice = store.key_create("alice", "laptop").await.unwrap().secret;
        assert_eq!(call(M::GET, &u("/v1/whoami"), None, None).await.0, 401, "no key, no access");
        assert_eq!(call(M::GET, &u("/v1/whoami"), Some("mdk_bogus"), None).await.0, 401);
        let (_, me) = call(M::GET, &u("/v1/whoami"), Some(&alice), None).await;
        assert_eq!((me["user"].as_str(), me["admin"].as_bool()), (Some("alice"), Some(true)));

        // alice creates bob (not admin) over the API
        let (st, created) =
            call(M::POST, &u("/v1/admin/users"), Some(&alice), Some(json!({"name": "bob", "key_name": "work"}))).await;
        assert_eq!(st, 200);
        let bob = created["key"]["secret"].as_str().unwrap().to_owned();
        assert_eq!(call(M::GET, &u("/v1/admin/users"), Some(&bob), None).await.0, 403, "bob isn't admin");

        // attribution comes from the key, global memories are private, project memories shared
        let mem =
            |content: &str, project: Option<&str>| json!({"content": content, "project": project, "source": "codex"});
        let (_, m) = call(M::POST, &u("/v1/memories"), Some(&bob), Some(mem("bob likes tabs", None))).await;
        assert_eq!((m["source"].as_str(), m["user"].as_str()), (Some("bob/codex"), Some("bob")));
        call(M::POST, &u("/v1/memories"), Some(&bob), Some(mem("API uses JWT", Some("shop")))).await;
        let (_, seen) = call(M::GET, &u("/v1/memories?project=shop&user=bob"), Some(&alice), None).await;
        let seen: Vec<_> = seen.as_array().unwrap().iter().map(|m| m["content"].as_str().unwrap()).collect();
        assert_eq!(
            seen,
            vec!["API uses JWT"],
            "alice sees the project memory, not bob's private global (even asking as bob)"
        );
        let (_, brief) = call(M::GET, &u("/v1/resume?project=shop"), Some(&bob), None).await;
        assert!(brief["markdown"].as_str().unwrap().contains("bob likes tabs"), "bob sees his own global");

        // same agent name, different users: separate lease owners
        let (_, run) = call(
            M::POST,
            &u("/v1/runs"),
            Some(&alice),
            Some(json!({"project": "shop", "name": "deploy", "owner": "claude-code"})),
        )
        .await;
        assert_eq!(run["owner"], "alice/claude-code");
        let run_id = run["id"].as_str().unwrap();
        let (st, _) = call(
            M::POST,
            &u(&format!("/v1/runs/{run_id}/steps/a/begin")),
            Some(&bob),
            Some(json!({"owner": "claude-code"})),
        )
        .await;
        assert_eq!(st, 409, "bob's claude-code can't step into alice's leased run");

        // self-service keys; revocation and disabling take effect immediately
        let (_, k2) = call(M::POST, &u("/v1/me/keys"), Some(&bob), Some(json!({"name": "ci"}))).await;
        let bob_ci = k2["secret"].as_str().unwrap().to_owned();
        let (_, mine) = call(M::GET, &u("/v1/me/keys"), Some(&bob), None).await;
        assert_eq!(mine.as_array().unwrap().len(), 2);
        assert!(
            mine.to_string().find("mdk_").is_some() && !mine.to_string().contains(&bob),
            "listings never contain secrets"
        );
        let prefix = k2["prefix"].as_str().unwrap();
        assert_eq!(call(M::DELETE, &u(&format!("/v1/me/keys/{prefix}")), Some(&bob), None).await.0, 200);
        assert_eq!(call(M::GET, &u("/v1/whoami"), Some(&bob_ci), None).await.0, 401, "revoked");
        assert_eq!(call(M::GET, &u("/v1/whoami"), Some(&bob), None).await.0, 200, "other key unaffected");
        let alice_keys = store.key_list(Some("alice")).await.unwrap();
        let (st, _) = call(M::DELETE, &u(&format!("/v1/me/keys/{}", alice_keys[0].id)), Some(&bob), None).await;
        assert_eq!(st, 404, "bob can't revoke alice's key");
        call(M::POST, &u("/v1/admin/users/bob/disable"), Some(&alice), None).await;
        assert_eq!(call(M::GET, &u("/v1/whoami"), Some(&bob), None).await.0, 401, "disabled user locked out");
        call(M::POST, &u("/v1/admin/users/bob/enable"), Some(&alice), None).await;
        assert_eq!(call(M::GET, &u("/v1/whoami"), Some(&bob), None).await.0, 200);
    }

    #[tokio::test]
    async fn open_server_locks_once_first_user_exists() {
        let (url, store) = spawn(None).await;
        assert_eq!(call(reqwest::Method::GET, &format!("{url}/v1/whoami"), None, None).await.0, 200, "open");
        store.user_create("carol", true).await.unwrap(); // e.g. CLI on the server, while it runs
        assert_eq!(call(reqwest::Method::GET, &format!("{url}/v1/whoami"), None, None).await.0, 401, "now locked");
    }
}
