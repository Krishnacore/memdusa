//! HTTP server: REST API for custom agents/hooks plus MCP over streamable
//! HTTP (`/mcp`) for remote harnesses and cloud agents.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::{DefaultBodyLimit, Extension, MatchedPath, Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use memdusa_core::access::AccessPolicy;
use memdusa_core::model::*;
use memdusa_core::retention::{PruneReport, RetentionPolicy};
use memdusa_core::transfer;
use memdusa_core::{Error, Store, brief};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::hooks::{self, HookBatchItem, HookBatchResponse, HookEvent, HookRequest, HookResponse};
use crate::mcp::{Defaults, MemdusaMcp};
use crate::oauth::{self, OAuthConfig, OAuthRuntime};
use crate::ops::{Gauges, Metrics, RateLimiter};

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) store: Store,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) limiter: Option<Arc<RateLimiter>>,
    /// Shared break-glass token (`--token`): full admin access.
    pub(crate) token: Option<Arc<str>>,
    /// Latched to true once any user exists (or a token is set). Users can be
    /// created by the CLI while the server runs, so an open server re-checks
    /// on every request until it latches.
    pub(crate) auth_required: Arc<AtomicBool>,
    /// Fallback for users without a matching grant (`MEMDUSA_DEFAULT_ACCESS`).
    pub(crate) default_access: DefaultPolicy,
    /// OAuth authorization server, enabled by `MEMDUSA_PUBLIC_URL`.
    pub(crate) oauth: Option<OAuthRuntime>,
}

/// Per-request snapshot of the caller's grants: at most one grants query per
/// request, however many endpoints/tools check access. Inserted by `auth`.
#[derive(Clone)]
pub(crate) struct AccessCache {
    policy: AccessPolicy,
    store: Store,
    /// OAuth tokens scoped to `memdusa:read` can never write, whatever the grants say.
    read_only: bool,
}

impl AccessCache {
    /// Filter to readable projects using this request's cached grants.
    pub(crate) async fn readable_projects(&self, store: &Store, identity: &Identity) -> Result<Vec<String>, Error> {
        Ok(self.policy.readable(identity, &store.distinct_projects().await?))
    }

    /// Check `needed` access on `project`; denials are returned as 403 and
    /// written to the audit log.
    pub(crate) async fn authorize(
        &self,
        identity: &Identity,
        project: &str,
        needed: Access,
        action: &str,
    ) -> Result<(), Error> {
        if self.read_only && needed == Access::Write {
            let detail = json!({ "needed": needed.as_str(), "action": action, "reason": "read_only_token" });
            if let Err(e) = self
                .store
                .audit(NewAuditEvent::denied(identity.actor(), "denied").project(project).detail(detail))
                .await
            {
                tracing::error!("could not audit denied request: {e}");
            }
            return Err(Error::Forbidden("read-only token cannot write".into()));
        }
        if self.policy.allows(identity, project, needed) {
            return Ok(());
        }
        let detail = json!({ "needed": needed.as_str(), "action": action });
        if let Err(e) =
            self.store.audit(NewAuditEvent::denied(identity.actor(), "denied").project(project).detail(detail)).await
        {
            tracing::error!("could not audit denied request: {e}");
        }
        Err(Error::Forbidden(format!("access denied to project '{project}' (needs {})", needed.as_str())))
    }
}

async fn check(
    access: &AccessCache,
    identity: &Identity,
    project: &str,
    needed: Access,
    action: &str,
) -> Result<(), ApiError> {
    Ok(access.authorize(identity, project, needed, action).await?)
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
    /// Prune old data once a day with this policy (`None` = never).
    pub auto_prune: Option<RetentionPolicy>,
    /// Sustained requests per second per caller (bursts up to 10x); 0 = off.
    pub rate_limit: f64,
    /// Serve Prometheus metrics at `/metrics`.
    pub metrics: bool,
    /// Fallback access for users with no grant (default `write`, via
    /// `MEMDUSA_DEFAULT_ACCESS`).
    pub default_access: DefaultPolicy,
    /// OAuth authorization server config (`None` disables OAuth endpoints).
    pub oauth: Option<OAuthConfig>,
}

pub(crate) struct ApiError(pub(crate) Error);

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
    tracing::info!("default project access: {} (MEMDUSA_DEFAULT_ACCESS)", opts.default_access.as_str());
    if let Some(policy) = opts.auto_prune {
        let store = store.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                tick.tick().await;
                match store.maybe_prune(&policy, 86_400_000).await {
                    Ok(Some(r)) if r.events_deleted + r.checkpoints_deleted + r.runs_deleted > 0 => {
                        tracing::info!(?r, "pruned old data")
                    }
                    Ok(Some(_)) => tracing::debug!("nothing to prune"),
                    Ok(None) => {}
                    Err(e) => tracing::error!("auto-prune failed: {e}"),
                }
            }
        });
    }
    let app = app(store, opts).into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, app).with_graceful_shutdown(shutdown()).await?;
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
                Defaults { project: None, agent: "mcp-http".into(), cwd: None, task: None, run: None },
            ))
        },
        Arc::new(LocalSessionManager::default()),
        mcp_config,
    );

    let serve_metrics = opts.metrics;
    let state = AppState {
        store,
        metrics: Arc::new(Metrics::default()),
        limiter: (opts.rate_limit > 0.0).then(|| Arc::new(RateLimiter::new(opts.rate_limit, opts.rate_limit * 10.0))),
        auth_required: Arc::new(AtomicBool::new(opts.token.is_some())),
        token: opts.token.map(Into::into),
        default_access: opts.default_access,
        oauth: opts.oauth.map(OAuthRuntime::new),
    };

    let api = Router::new()
        .route("/v1/whoami", get(whoami))
        .route("/v1/me/keys", get(my_keys).post(my_key_create))
        .route("/v1/me/keys/{id}", delete(my_key_revoke))
        .route("/v1/admin/grants", get(admin_grants).post(admin_grant_create))
        .route("/v1/admin/grants/{id}", delete(admin_grant_delete))
        .route("/v1/admin/audit", get(admin_audit))
        .route("/v1/admin/users", get(admin_users).post(admin_user_create))
        .route("/v1/admin/users/{name}/disable", post(admin_user_disable))
        .route("/v1/admin/users/{name}/enable", post(admin_user_enable))
        .route("/v1/admin/users/{name}/keys", post(admin_key_create))
        .route("/v1/admin/keys", get(admin_keys))
        .route("/v1/admin/prune", post(admin_prune))
        .route("/v1/embeddings/backfill", post(embeddings_backfill))
        .route("/v1/admin/export", get(admin_export))
        .route("/v1/admin/import", post(admin_import).layer(DefaultBodyLimit::max(64 * 1024 * 1024)))
        .route("/v1/admin/keys/{id}", delete(admin_key_revoke))
        .route("/v1/hooks/batch", post(hook_batch).layer(DefaultBodyLimit::max(16 * 1024 * 1024)))
        .route("/v1/hooks/{event}", post(hook))
        .route("/v1/resume", get(resume))
        .route("/v1/memories", post(memory_save).get(memory_search))
        .route("/v1/memories/review", get(memory_review))
        .route("/v1/memories/{id}", delete(memory_forget).patch(memory_update))
        .route("/v1/events", post(event_append).get(events_recent))
        .route("/v1/checkpoints", post(checkpoint_save).get(checkpoints_latest))
        .route("/v1/runs", post(run_start).get(run_list))
        .route("/v1/delegate", post(delegate))
        .route("/v1/runs/{id}", get(run_get))
        .route("/v1/runs/{id}/claim", post(run_claim))
        .route("/v1/runs/{id}/finish", post(run_finish))
        .route("/v1/runs/{id}/steps/{key}/begin", post(step_begin))
        .route("/v1/runs/{id}/steps/{key}/complete", post(step_complete))
        .route("/v1/runs/{id}/steps/{key}/fail", post(step_fail))
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn(mcp_session_close_status))
        .merge(oauth::identity_routes())
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), observe))
        .with_state(state.clone());

    let mut public = Router::new().route("/health", get(|| async { "ok" }));
    if serve_metrics {
        public = public.route("/metrics", get(metrics));
    }
    let mut all = public.with_state(state.clone()).merge(api);
    if state.oauth.is_some() {
        all = all
            .merge(oauth::public_routes().with_state(state.clone()))
            .merge(oauth::routes().with_state(state.clone()))
            .merge(
                oauth::session_routes().layer(middleware::from_fn_with_state(state.clone(), auth)).with_state(state),
            );
    }
    all
}

/// Who made the request, filled in by `auth` for the access log.
#[derive(Clone, Default)]
struct UserSlot(Arc<std::sync::Mutex<Option<String>>>);

/// Access log + metrics, around everything (including rejected requests).
/// Only route templates are recorded, never ids, query strings or bodies.
async fn observe(State(s): State<AppState>, mut req: Request, next: Next) -> Response {
    let start = std::time::Instant::now();
    let method = req.method().to_string();
    let route = req.extensions().get::<MatchedPath>().map_or("unmatched", |p| p.as_str()).to_owned();
    let slot = UserSlot::default();
    req.extensions_mut().insert(slot.clone());
    let resp = next.run(req).await;
    let (status, elapsed) = (resp.status().as_u16(), start.elapsed());
    s.metrics.observe(&method, &route, status, elapsed);
    let user = slot.0.lock().map(|u| u.clone()).unwrap_or_default();
    tracing::info!(
        target: "memdusa::access",
        method,
        route,
        status,
        ms = elapsed.as_millis() as u64,
        user = user.as_deref().unwrap_or("-")
    );
    resp
}

async fn metrics(State(s): State<AppState>) -> Result<Response, ApiError> {
    let (runs_running, runs_stale) = s.store.run_counts().await?;
    let gauges = Gauges { users: s.store.user_count().await?, runs_running, runs_stale };
    Ok(([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], s.metrics.render(&gauges)).into_response())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

/// rmcp answers a session close (`DELETE /mcp`) with 202 Accepted; MCP clients such as the official
/// Python SDK treat anything but 200/204 as a failed close ("Session termination failed: 202").
/// The session is gone once the handler returns, so report it as 204 No Content.
async fn mcp_session_close_status(req: Request, next: Next) -> Response {
    let is_close = req.method() == axum::http::Method::DELETE && req.uri().path().starts_with("/mcp");
    let mut res = next.run(req).await;
    if is_close && res.status() == StatusCode::ACCEPTED {
        *res.status_mut() = StatusCode::NO_CONTENT;
        *res.body_mut() = axum::body::Body::empty();
    }
    res
}

/// Resolve the caller and attach an [`Identity`] to the request (REST
/// handlers and MCP tools both read it).
pub(crate) async fn auth(State(s): State<AppState>, mut req: Request, next: Next) -> Response {
    let unauthorized = |msg: &str| {
        s.metrics.auth_failures.fetch_add(1, Ordering::Relaxed);
        let mut resp = (StatusCode::UNAUTHORIZED, Json(json!({ "error": msg }))).into_response();
        if let Some(o) = &s.oauth
            && let Ok(v) = HeaderValue::from_str(&o.config.www_authenticate())
        {
            resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
        }
        resp
    };
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .map(String::from);
    let path = req.uri().path().to_owned();
    let unsafe_method = !req.method().is_safe();

    let mut read_only = false;
    let identity = match bearer {
        Some(t) if s.token.as_ref().is_some_and(|tok| constant_time_eq(t.as_bytes(), tok.as_bytes())) => {
            Identity::root()
        }
        Some(t) if t.starts_with("mdo_") => match s.store.authenticate_access(&t, now_ms()).await {
            Ok(Some(auth)) => {
                read_only = auth.read_only;
                auth.identity
            }
            Ok(None) => return unauthorized("invalid, expired or revoked access token"),
            Err(e) => return ApiError(e).into_response(),
        },
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
    if read_only && unsafe_method && path.starts_with("/v1/") {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "read-only token cannot write (scope memdusa:read)" })))
            .into_response();
    }
    if let Some(slot) = req.extensions().get::<UserSlot>()
        && let Ok(mut u) = slot.0.lock()
    {
        *u = Some(identity.user.clone().unwrap_or_else(|| "root".into()));
    }
    if let Some(limiter) = &s.limiter {
        let key = match &identity.user {
            Some(u) => format!("user:{u}"),
            None => req
                .extensions()
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map_or_else(|| "anonymous".into(), |c| format!("ip:{}", c.0.ip())),
        };
        if let Err(wait) = limiter.check(&key) {
            s.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
            let secs = wait.as_secs_f64().ceil().max(1.0) as u64;
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, secs.to_string())],
                Json(json!({ "error": format!("rate limit exceeded; retry in {secs}s") })),
            )
                .into_response();
        }
    }
    let policy = match s.store.access_policy(identity.user.as_deref(), s.default_access).await {
        Ok(p) => p,
        Err(e) => return ApiError(e).into_response(),
    };
    req.extensions_mut().insert(identity);
    req.extensions_mut().insert(AccessCache { policy, store: s.store.clone(), read_only });
    next.run(req).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------- handlers

async fn whoami(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
) -> ApiResult<Value> {
    let backend = match s.store.backend() {
        memdusa_core::Backend::Sqlite => "sqlite",
        memdusa_core::Backend::Postgres => "postgres",
    };
    // semantic search status; a failing count must not break whoami
    let semantic = s.store.embedding_stats().await.unwrap_or_default();
    Ok(Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "backend": backend,
        "user": id.user,
        "admin": id.admin,
        "default_access": s.default_access.as_str(),
        "grants": access.policy.grants(),
        "semantic": { "model": semantic.model, "embedded": semantic.embedded, "total": semantic.total },
    })))
}

#[derive(Deserialize, Default)]
struct BackfillBody {
    #[serde(default)]
    all: bool,
    /// Only this project; missing means the whole corpus.
    project: Option<String>,
}

/// Embed memories that have no vector for the server's embedding model (admin).
async fn embeddings_backfill(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    body: Option<Json<BackfillBody>>,
) -> ApiResult<memdusa_core::embed::BackfillReport> {
    admin(&id)?;
    let body = body.map(|Json(b)| b).unwrap_or_default();
    if let Some(project) = &body.project {
        check(&access, &id, project, Access::Write, "backfill").await?;
    }
    let report = s.store.embeddings_backfill(body.all, body.project.as_deref()).await?;
    s.store.audit(NewAuditEvent::ok(id.actor(), "backfill").project(body.project.as_deref().unwrap_or(""))).await?;
    Ok(Json(report))
}

async fn hook(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Path(event): Path<String>,
    Json(req): Json<HookRequest>,
) -> ApiResult<HookResponse> {
    check(&access, &id, &req.project, Access::Write, "hook").await?;
    let event: HookEvent = event.parse().map_err(|e: String| Error::Invalid(format!("unknown hook event: {e}")))?;
    s.metrics.hook(event.arg());
    Ok(Json(hooks::handle_request(&s.store, event, req, &id).await?))
}

#[derive(Deserialize)]
struct BatchBody {
    items: Vec<HookBatchItem>,
}

/// Replay of hooks queued on a client while the server was unreachable.
/// Items are applied in order; invalid ones are reported and skipped (so one
/// bad entry can't block a queue forever), database errors abort the batch
/// so the client retries it.
async fn hook_batch(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Json(b): Json<BatchBody>,
) -> ApiResult<HookBatchResponse> {
    if b.items.len() > 1000 {
        return Err(Error::Invalid("at most 1000 items per batch".into()).into());
    }
    let mut resp = HookBatchResponse::default();
    for (i, mut item) in b.items.into_iter().enumerate() {
        check(&access, &id, &item.request.project, Access::Write, "hook.batch").await?;
        // replayed prompts can't inject anything any more
        item.request.config.recall = false;
        let event: HookEvent = match item.event.parse() {
            Ok(e) => e,
            Err(e) => {
                resp.rejected.push(format!("#{i}: unknown event {e}"));
                continue;
            }
        };
        s.metrics.hook(event.arg());
        match hooks::handle_request(&s.store, event, item.request, &id).await {
            Ok(_) => resp.processed += 1,
            Err(Error::Invalid(m)) => resp.rejected.push(format!("#{i}: {m}")),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Json(resp))
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

// ---------------------------------------------------------------- grants & audit

#[derive(Deserialize)]
struct GrantBody {
    pattern: String,
    user: String,
    role: Role,
}

async fn admin_grant_create(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Json(b): Json<GrantBody>,
) -> ApiResult<ProjectGrant> {
    admin(&id)?;
    let g = s.store.grant_create(&b.pattern, &b.user, b.role, id.actor()).await?;
    s.store
        .audit(
            NewAuditEvent::ok(id.actor(), "grant.create")
                .project(&g.pattern)
                .target(&g.id)
                .detail(json!({"user": g.user, "role": g.role.as_str(), "pattern": g.pattern})),
        )
        .await?;
    Ok(Json(g))
}

async fn admin_grants(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Query(q): Query<KeysQ>,
) -> ApiResult<Vec<ProjectGrant>> {
    admin(&id)?;
    Ok(Json(s.store.grants_for_user(q.user.as_deref()).await?))
}

async fn admin_grant_delete(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Path(gid): Path<String>,
) -> ApiResult<ProjectGrant> {
    admin(&id)?;
    let g = s.store.grant_delete(&gid).await?;
    s.store.audit(NewAuditEvent::ok(id.actor(), "grant.delete").project(&g.pattern).target(&g.id)).await?;
    Ok(Json(g))
}

async fn admin_audit(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Vec<AuditEvent>> {
    admin(&id)?;
    Ok(Json(s.store.audit_query(q).await?))
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
    let key = s.store.key_create(me(&id)?, &b.name).await?;
    s.store.audit(NewAuditEvent::ok(id.actor(), "key.create").target(&key.key.id)).await?;
    Ok(Json(key))
}

async fn my_key_revoke(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Path(key): Path<String>,
) -> ApiResult<ApiKey> {
    let revoked = s.store.key_revoke(&key, Some(me(&id)?)).await?;
    s.store.audit(NewAuditEvent::ok(id.actor(), "key.revoke").target(&revoked.id)).await?;
    Ok(Json(revoked))
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
    s.store
        .audit(NewAuditEvent::ok(id.actor(), "user.create").target(&user.name).detail(json!({"admin": user.admin})))
        .await?;
    Ok(Json(NewUserResponse { user, key }))
}

async fn admin_user_disable(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Path(name): Path<String>,
) -> ApiResult<User> {
    admin(&id)?;
    let user = s.store.user_set_disabled(&name, true).await?;
    s.store.audit(NewAuditEvent::ok(id.actor(), "user.disable").target(&name)).await?;
    Ok(Json(user))
}

async fn admin_user_enable(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Path(name): Path<String>,
) -> ApiResult<User> {
    admin(&id)?;
    let user = s.store.user_set_disabled(&name, false).await?;
    s.store.audit(NewAuditEvent::ok(id.actor(), "user.enable").target(&name)).await?;
    Ok(Json(user))
}

async fn admin_key_create(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Path(name): Path<String>,
    Json(b): Json<KeyBody>,
) -> ApiResult<CreatedKey> {
    admin(&id)?;
    let key = s.store.key_create(&name, &b.name).await?;
    s.store.audit(NewAuditEvent::ok(id.actor(), "key.create").target(&key.key.id)).await?;
    Ok(Json(key))
}

#[derive(Deserialize)]
struct ExportQ {
    project: Option<String>,
}

async fn admin_export(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Query(q): Query<ExportQ>,
) -> Result<Response, ApiError> {
    // A single project only needs read access; the full corpus stays admin-only.
    match &q.project {
        Some(project) => check(&access, &id, project, Access::Read, "export").await?,
        None => admin(&id)?,
    }
    let body = transfer::to_jsonl(&s.store.export(q.project.as_deref()).await?);
    s.store
        .audit(
            NewAuditEvent::ok(id.actor(), "export")
                .project(q.project.as_deref().unwrap_or(""))
                .detail(json!({"full": q.project.is_none()})),
        )
        .await?;
    Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], body).into_response())
}

async fn admin_import(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    body: String,
) -> ApiResult<transfer::ImportReport> {
    admin(&id)?;
    let report = s.store.import(transfer::parse_jsonl(&body)?).await?;
    s.store.audit(NewAuditEvent::ok(id.actor(), "import")).await?;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct PruneBody {
    #[serde(default)]
    policy: RetentionPolicy,
    project: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

async fn admin_prune(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Json(b): Json<PruneBody>,
) -> ApiResult<PruneReport> {
    admin(&id)?;
    let report = s.store.prune(&b.policy, b.project.as_deref(), b.dry_run).await?;
    s.store
        .audit(
            NewAuditEvent::ok(id.actor(), "prune")
                .project(b.project.as_deref().unwrap_or(""))
                .detail(json!({"dry_run": b.dry_run})),
        )
        .await?;
    Ok(Json(report))
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
    let revoked = s.store.key_revoke(&key, None).await?;
    s.store.audit(NewAuditEvent::ok(id.actor(), "key.revoke").target(&revoked.id)).await?;
    Ok(Json(revoked))
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
    /// A worker's delegated task: the brief covers that task only.
    task: Option<String>,
}

async fn resume(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Query(q): Query<ResumeQ>,
) -> ApiResult<memdusa_core::Brief> {
    check(&access, &id, &q.project, Access::Read, "resume").await?;
    let git: Option<GitState> =
        q.git.map(|g| serde_json::from_str(&g)).transpose().map_err(|e| Error::Invalid(format!("git: {e}")))?;
    let task = q.task.as_deref().map(str::trim).filter(|t| !t.is_empty());
    Ok(Json(brief::build(&s.store, &q.project, id.user.as_deref(), git.as_ref(), task).await?))
}

/// Attribution always comes from the credential, never from the request body.
fn attribute(id: &Identity, source: Option<String>) -> Option<String> {
    source.map(|s| id.qualify(&s)).or_else(|| id.user.clone())
}

async fn memory_save(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Json(mut m): Json<NewMemory>,
) -> ApiResult<Memory> {
    // Global memories (project NULL) keep their private-to-owner rule.
    if let Some(project) = &m.project {
        check(&access, &id, project, Access::Write, "memory.save").await?;
    }
    m.source = attribute(&id, m.source);
    m.user = id.user;
    Ok(Json(s.store.memory_save(m).await?))
}

async fn memory_update(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Path(mid): Path<String>,
    Json(mut p): Json<MemoryPatch>,
) -> ApiResult<Memory> {
    if let Some(project) = s.store.memory_project(&mid).await? {
        check(&access, &id, &project, Access::Write, "memory.update").await?;
    }
    p.source = attribute(&id, None);
    Ok(Json(s.store.memory_update_as(&mid, p, &id).await?))
}

async fn memory_search(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Query(mut q): Query<MemoryQuery>,
) -> ApiResult<Vec<Memory>> {
    q.allowed_projects = match &q.project {
        Some(project) => {
            check(&access, &id, project, Access::Read, "memory.search").await?;
            None
        }
        None => Some(access.readable_projects(&s.store, &id).await?),
    };
    q.user = id.user;
    Ok(Json(s.store.memory_search(q).await?))
}

#[derive(serde::Deserialize)]
struct ReviewQuery {
    project: String,
}

async fn memory_review(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Query(q): Query<ReviewQuery>,
) -> ApiResult<memdusa_core::review::MemoryReview> {
    check(&access, &id, &q.project, Access::Read, "memory.review").await?;
    Ok(Json(s.store.memory_review(&q.project, id.user.as_deref()).await?))
}

async fn memory_forget(
    State(s): State<AppState>,
    Extension(who): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Path(mid): Path<String>,
) -> Result<StatusCode, ApiError> {
    let project = s.store.memory_project(&mid).await?;
    if let Some(project) = &project {
        check(&access, &who, project, Access::Write, "memory.forget").await?;
    }
    s.store.memory_forget_as(&mid, &who).await?;
    s.store
        .audit(NewAuditEvent::ok(who.actor(), "memory.forget").project(project.as_deref().unwrap_or("")).target(&mid))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn event_append(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Json(mut e): Json<NewEvent>,
) -> ApiResult<Event> {
    check(&access, &id, &e.project, Access::Write, "event.append").await?;
    e.source = attribute(&id, e.source);
    Ok(Json(s.store.event_append(e).await?))
}

async fn events_recent(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Query(q): Query<ProjectQ>,
) -> ApiResult<Vec<Event>> {
    check(&access, &id, &q.project, Access::Read, "events.recent").await?;
    Ok(Json(s.store.events_recent(&q.project, q.session.as_deref(), q.limit.unwrap_or(50)).await?))
}

async fn checkpoint_save(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Json(mut c): Json<NewCheckpoint>,
) -> ApiResult<Checkpoint> {
    check(&access, &id, &c.project, Access::Write, "checkpoint.save").await?;
    c.source = attribute(&id, c.source);
    Ok(Json(s.store.checkpoint_save(c).await?))
}

async fn checkpoints_latest(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Query(q): Query<ProjectQ>,
) -> ApiResult<Vec<Checkpoint>> {
    check(&access, &id, &q.project, Access::Read, "checkpoints.latest").await?;
    Ok(Json(s.store.checkpoints_latest(&q.project, q.limit.unwrap_or(10)).await?))
}

async fn run_start(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Json(mut r): Json<NewRun>,
) -> ApiResult<RunWithSteps> {
    check(&access, &id, &r.project, Access::Write, "run.start").await?;
    r.owner = Some(id.qualify(r.owner.as_deref().unwrap_or("anonymous")));
    Ok(Json(s.store.run_start(r).await?))
}

#[derive(Deserialize)]
struct DelegateBody {
    project: String,
    name: String,
    tasks: Vec<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
}

/// A coordinator splits work into tasks: one durable run with a pending step per task.
async fn delegate(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Json(b): Json<DelegateBody>,
) -> ApiResult<RunWithSteps> {
    check(&access, &id, &b.project, Access::Write, "delegate").await?;
    Ok(Json(s.store.run_delegate(&b.project, &b.name, b.tasks, b.idempotency_key).await?))
}

#[derive(Deserialize)]
struct RunListQ {
    project: Option<String>,
    status: Option<RunStatus>,
    limit: Option<i64>,
}

async fn run_list(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Query(q): Query<RunListQ>,
) -> ApiResult<Vec<Run>> {
    let allowed = match &q.project {
        Some(project) => {
            check(&access, &id, project, Access::Read, "run.list").await?;
            None
        }
        None => Some(access.readable_projects(&s.store, &id).await?),
    };
    Ok(Json(s.store.run_list_visible(q.project.as_deref(), allowed.as_deref(), q.status, q.limit.unwrap_or(50)).await?))
}

async fn run_get(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Path(rid): Path<String>,
) -> ApiResult<RunWithSteps> {
    let project = s.store.run_project(&rid).await?;
    check(&access, &id, &project, Access::Read, "run.get").await?;
    Ok(Json(s.store.run_get(&rid).await?))
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
    Extension(access): Extension<AccessCache>,
    Path(rid): Path<String>,
    Json(b): Json<ClaimBody>,
) -> ApiResult<RunWithSteps> {
    let project = s.store.run_project(&rid).await?;
    check(&access, &who, &project, Access::Write, "run.claim").await?;
    Ok(Json(s.store.run_claim(&rid, &who.qualify(&b.owner), b.lease_secs, b.force).await?))
}

#[derive(Deserialize)]
struct FinishBody {
    outcome: RunOutcome,
    output: Option<Value>,
    error: Option<String>,
}

async fn run_finish(
    State(s): State<AppState>,
    Extension(who): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Path(rid): Path<String>,
    Json(b): Json<FinishBody>,
) -> ApiResult<RunWithSteps> {
    let project = s.store.run_project(&rid).await?;
    check(&access, &who, &project, Access::Write, "run.finish").await?;
    if let (Some(user), Some(owner)) = (who.user.as_deref(), s.store.run_owner(&rid).await?)
        && owner != user
        && !owner.starts_with(&format!("{user}/"))
    {
        s.store.audit(NewAuditEvent::ok(who.actor(), "run.finish_foreign").project(&project).target(&rid)).await?;
    }
    Ok(Json(s.store.run_finish(&rid, b.outcome, b.output, b.error).await?))
}

#[derive(Deserialize)]
struct BeginBody {
    owner: String,
    input: Option<Value>,
}

async fn step_begin(
    State(s): State<AppState>,
    Extension(who): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Path((rid, key)): Path<(String, String)>,
    Json(b): Json<BeginBody>,
) -> ApiResult<StepDecision> {
    let project = s.store.run_project(&rid).await?;
    check(&access, &who, &project, Access::Write, "step.begin").await?;
    Ok(Json(s.store.step_begin(&rid, &key, b.input, &who.qualify(&b.owner)).await?))
}

#[derive(Deserialize)]
struct CompleteBody {
    output: Option<Value>,
}

async fn step_complete(
    State(s): State<AppState>,
    Extension(who): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Path((rid, key)): Path<(String, String)>,
    Json(b): Json<CompleteBody>,
) -> ApiResult<Step> {
    let project = s.store.run_project(&rid).await?;
    check(&access, &who, &project, Access::Write, "step.complete").await?;
    Ok(Json(s.store.step_complete(&rid, &key, b.output).await?))
}

#[derive(Deserialize)]
struct FailBody {
    error: String,
}

async fn step_fail(
    State(s): State<AppState>,
    Extension(who): Extension<Identity>,
    Extension(access): Extension<AccessCache>,
    Path((rid, key)): Path<(String, String)>,
    Json(b): Json<FailBody>,
) -> ApiResult<Step> {
    let project = s.store.run_project(&rid).await?;
    check(&access, &who, &project, Access::Write, "step.fail").await?;
    Ok(Json(s.store.step_fail(&rid, &key, &b.error).await?))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::config::Remote;
    use crate::hooks::HookConfig;
    use crate::remote::Client;

    async fn spawn_with(token: Option<&str>, default_access: DefaultPolicy) -> (String, Store) {
        let dir = std::env::temp_dir().join(format!("memdusa-http-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::connect(&memdusa_core::store::sqlite_url(&dir.join("t.db"))).await.unwrap();
        let opts = HttpOptions {
            addr: String::new(),
            token: token.map(String::from),
            allowed_hosts: vec![],
            auto_prune: None,
            rate_limit: 0.0,
            metrics: true,
            default_access,
            oauth: None,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = app(store.clone(), opts);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, store)
    }

    async fn spawn(token: Option<&str>) -> (String, Store) {
        spawn_with(token, DefaultPolicy::Write).await
    }

    fn client(url: &str, token: Option<&str>) -> Client {
        Client::new(Remote::new(url, token.map(String::from)).unwrap(), Duration::from_secs(5)).unwrap()
    }

    #[tokio::test]
    async fn remote_hooks_carry_the_delegated_task() {
        let (url, store) = spawn(None).await;
        let c = client(&url, None);
        let req = |task: Option<&str>, input| HookRequest {
            project: "shop".into(),
            config: HookConfig { agent: "codex".into(), journal_prompts: true, stop_guard_mins: 15, recall: true },
            input,
            git: None,
            at: None,
            task: task.map(String::from),
            parent: task.map(|_| "lead".into()),
            run: None,
        };
        c.hook(HookEvent::SessionStart, &req(Some("docs"), json!({"session_id": "W", "source": "startup"})))
            .await
            .unwrap();
        let edit =
            json!({"session_id": "W", "cwd": "/r", "tool_name": "Write", "tool_input": {"file_path": "/r/a.rs"}});
        c.hook(HookEvent::PostToolUse, &req(Some("docs"), edit)).await.unwrap();
        c.hook(HookEvent::SessionEnd, &req(Some("docs"), json!({"session_id": "W", "reason": "logout"})))
            .await
            .unwrap();

        let started = store.events_recent_kind("shop", None, "delegated_session", 10).await.unwrap();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].data.as_ref().unwrap()["task"], "docs");
        assert_eq!(started[0].data.as_ref().unwrap()["parent"], "lead");
        assert_eq!(store.checkpoints_latest("shop", 1).await.unwrap()[0].task.as_deref(), Some("docs"));

        // an old client sends neither field: still accepted, nothing delegated
        let old = json!({"project": "shop", "agent": "x", "journal_prompts": true, "stop_guard_mins": 0,
                         "input": {"session_id": "O", "source": "startup"}});
        let r: HookRequest = serde_json::from_value(old).unwrap();
        assert!(r.task.is_none() && r.parent.is_none());
    }

    #[tokio::test]
    async fn remote_hooks_round_trip_with_auth() {
        let (url, store) = spawn(Some("s3cret")).await;
        assert!(client(&url, Some("wrong")).whoami().await.is_err(), "bad token rejected");
        let c = client(&url, Some("s3cret"));
        assert_eq!(c.whoami().await.unwrap()["ok"], true);

        let req = |input| HookRequest {
            project: "github.com/acme/shop".into(),
            config: HookConfig {
                agent: "claude-code".into(),
                journal_prompts: true,
                stop_guard_mins: 15,
                recall: true,
            },
            input,
            git: None,
            at: None,
            task: None,
            parent: None,
            run: None,
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
            allowed_projects: None,
        };
        assert_eq!(c.memory_search(&q).await.unwrap().len(), 1);
        assert!(c.resume("p", None, None).await.unwrap()["markdown"].as_str().unwrap().contains("use Stripe"));
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
    async fn patch_updates_a_memory_and_post_reports_merges() {
        use reqwest::Method as M;
        let (url, _) = spawn(None).await;
        let u = |p: &str| format!("{url}{p}");

        let first = json!({"project": "p", "content": "The API uses JWT tokens for auth"});
        let (st, saved) = call(M::POST, &u("/v1/memories"), None, Some(first)).await;
        assert_eq!(st, 200);
        assert!(saved.get("merged").is_none(), "plain saves stay backward compatible: {saved}");
        let id = saved["id"].as_str().unwrap().to_owned();

        let near = json!({"project": "p", "content": "API uses JWT tokens for authentication"});
        let (_, merged) = call(M::POST, &u("/v1/memories"), None, Some(near)).await;
        assert_eq!((merged["id"].clone(), merged["merged"].clone()), (json!(id), json!(true)));

        let patch = json!({"content": "Sessions use opaque cookies", "kind": "decision", "tags": ["auth"]});
        let (st, m) = call(M::PATCH, &u(&format!("/v1/memories/{id}")), None, Some(patch)).await;
        assert_eq!(st, 200, "{m}");
        assert_eq!(
            (m["content"].clone(), m["kind"].clone(), m["tags"].clone()),
            (json!("Sessions use opaque cookies"), json!("decision"), json!(["auth"]))
        );

        let (st, _) = call(M::PATCH, &u("/v1/memories/missing"), None, Some(json!({"content": "x"}))).await;
        assert_eq!(st, 404);
        let (st, _) = call(M::PATCH, &u(&format!("/v1/memories/{id}")), None, Some(json!({"content": "  "}))).await;
        assert_eq!(st, 400);
    }

    #[tokio::test]
    async fn global_memories_can_only_be_changed_by_owner_or_admin() {
        use reqwest::Method as M;
        let (url, store) = spawn(Some("shared")).await;
        let u = |p: &str| format!("{url}{p}");
        let mut keys = std::collections::HashMap::new();
        for (name, admin) in [("alice", false), ("bob", false), ("carol", true)] {
            store.user_create(name, admin).await.unwrap();
            keys.insert(name, store.key_create(name, "k").await.unwrap().secret);
        }
        let save = |who: &'static str, content: &'static str, project: Option<&'static str>| {
            let (u, k) = (u("/v1/memories"), keys[who].clone());
            async move { call(M::POST, &u, Some(&k), Some(json!({"content": content, "project": project}))).await.1 }
        };
        let patch = |who: &str, id: &str| {
            let (url, key) = (u(&format!("/v1/memories/{id}")), keys[who].clone());
            async move { call(M::PATCH, &url, Some(&key), Some(json!({"content": "edited"}))).await }
        };
        let forget = |who: &str, id: &str| {
            let (url, key) = (u(&format!("/v1/memories/{id}")), keys[who].clone());
            async move { call(M::DELETE, &url, Some(&key), None).await }
        };

        let g = save("alice", "alice's private preference", None).await["id"].as_str().unwrap().to_owned();
        assert_eq!(patch("bob", &g).await.0, 403, "bob can't update alice's global");
        assert_eq!(forget("bob", &g).await.0, 403, "bob can't forget alice's global");
        assert_eq!(patch("alice", &g).await.0, 200, "owner can update");
        assert_eq!(patch("carol", &g).await.0, 200, "admin can update");
        assert_eq!(patch("bob", "missing").await.0, 404);
        assert_eq!(forget("bob", &g).await.0, 403, "still there after the failed attempts");
        assert_eq!(forget("alice", &g).await.0, 204, "owner can forget");
        let g2 = save("alice", "another global", None).await["id"].as_str().unwrap().to_owned();
        assert_eq!(forget("carol", &g2).await.0, 204, "admin can forget");

        // project memories stay editable by any authenticated user
        let p = save("alice", "API uses JWT", Some("shop")).await["id"].as_str().unwrap().to_owned();
        assert_eq!(patch("bob", &p).await.0, 200);
        assert_eq!(forget("bob", &p).await.0, 204);

        // the shared token (root) keeps full access
        let g3 = save("alice", "third global", None).await["id"].as_str().unwrap().to_owned();
        let (st, _) = call(M::DELETE, &u(&format!("/v1/memories/{g3}")), Some("shared"), None).await;
        assert_eq!(st, 204);
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

    #[tokio::test]
    async fn offline_queue_replays_in_order_once_server_is_back() {
        use crate::hooks::HookBatchItem;
        use crate::spool::Spool;

        let dir = std::env::temp_dir().join(format!("memdusa-spool-{}", new_id()));
        let spool = Spool::at(dir.clone()).unwrap();
        let req = |input: Value, at: i64| HookRequest {
            project: "shop".into(),
            config: HookConfig {
                agent: "claude-code".into(),
                journal_prompts: true,
                stop_guard_mins: 15,
                recall: true,
            },
            input,
            git: None,
            at: Some(at),
            task: None,
            parent: None,
            run: None,
        };
        let item = |event: &str, input: Value, at: i64| HookBatchItem { event: event.into(), request: req(input, at) };
        let t0 = now_ms() - 60_000;
        // queued out of order on purpose: replay must sort by fire time
        let edit = json!({"session_id": "A", "cwd": "/r", "tool_name": "Edit", "tool_input": {"file_path": "/r/a.rs"}});
        spool
            .push("http://dead", item("session-end", json!({"session_id": "A", "reason": "logout"}), t0 + 2000))
            .unwrap();
        spool.push("http://dead", item("post-tool-use", edit, t0 + 1000)).unwrap();
        spool
            .push("http://dead", item("user-prompt-submit", json!({"session_id": "A", "prompt": "fix a.rs"}), t0))
            .unwrap();
        assert_eq!(spool.len(), 3);

        // server down: retryable error, nothing lost
        let dead = Client::new(Remote::new("http://127.0.0.1:9", None).unwrap(), Duration::from_secs(1)).unwrap();
        let err = spool.flush(&dead, "http://dead").await.unwrap_err();
        assert!(err.retryable, "{err}");
        assert_eq!(spool.len(), 3, "entries survive a failed flush");

        // server up (entries were queued for "http://dead": re-address them via a fresh spool line check)
        let (url, store) = spawn(None).await;
        assert_eq!(spool.flush(&client(&url, None), &url).await.unwrap(), 0, "entries for another server aren't sent");
        assert_eq!(spool.len(), 3);
        let text: String = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| std::fs::read_to_string(e.unwrap().path()).ok())
            .collect::<String>()
            .replace("http://dead", &url);
        for f in std::fs::read_dir(&dir).unwrap() {
            std::fs::remove_file(f.unwrap().path()).unwrap();
        }
        std::fs::write(dir.join("queue.jsonl"), text).unwrap();
        assert_eq!(spool.flush(&client(&url, None), &url).await.unwrap(), 3);
        assert_eq!(spool.len(), 0, "delivered entries are removed");

        // sorted replay => the session-end saw the prompt and the edit
        let cp = &store.checkpoints_latest("shop", 1).await.unwrap()[0];
        assert_eq!((cp.goal.as_str(), cp.files.clone()), ("fix a.rs", vec!["a.rs".to_string()]));
        assert_eq!(cp.created_at, t0 + 2000, "original fire time kept");
    }

    #[tokio::test]
    async fn rate_limit_body_limit_and_metrics() {
        use reqwest::Method as M;
        let dir = std::env::temp_dir().join(format!("memdusa-ops-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::connect(&memdusa_core::store::sqlite_url(&dir.join("t.db"))).await.unwrap();
        // 1 req/s sustained, bursts of 10
        let opts = HttpOptions {
            addr: String::new(),
            token: None,
            allowed_hosts: vec![],
            auto_prune: None,
            rate_limit: 1.0,
            metrics: true,
            default_access: DefaultPolicy::Write,
            oauth: None,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = app(store, opts);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        for i in 0..10 {
            assert_eq!(call(M::GET, &format!("{url}/v1/whoami"), None, None).await.0, 200, "request {i} within burst");
        }
        let resp = reqwest::get(format!("{url}/v1/whoami")).await.unwrap();
        assert_eq!(resp.status(), 429);
        assert!(resp.headers().get("retry-after").is_some());
        assert_eq!(reqwest::get(format!("{url}/health")).await.unwrap().status(), 200, "health isn't rate limited");

        // fresh limiter-free server for the body limit
        let (url2, store2) = spawn(None).await;
        let big = "x".repeat(2 * 1024 * 1024);
        // The server answers 413 and closes the connection while the client may still be uploading,
        // so under load the client can see a connection reset instead of the 413. Either way the
        // request must be refused and nothing stored.
        let sent =
            reqwest::Client::new().post(format!("{url2}/v1/memories")).json(&json!({"content": big})).send().await;
        match sent {
            Ok(resp) => assert_eq!(resp.status().as_u16(), 413, "1 MB default body limit"),
            Err(e) => assert!(e.is_request() || e.is_body() || e.is_connect(), "refused mid-upload: {e}"),
        }
        let stored = store2.memory_search(MemoryQuery { limit: Some(5), ..Default::default() }).await.unwrap();
        assert!(stored.is_empty(), "the oversized memory must not be stored");

        let text = reqwest::get(format!("{url}/metrics")).await.unwrap().text().await.unwrap();
        assert!(
            text.contains(r#"memdusa_http_requests_total{method="GET",route="/v1/whoami",status="200"} 10"#),
            "{text}"
        );
        assert!(text.contains(r#"route="/v1/whoami",status="429"} 1"#), "{text}");
        assert!(text.contains("memdusa_rate_limited_total 1"), "{text}");
        assert!(text.contains("memdusa_runs_running 0"), "{text}");
        assert!(!text.contains("127.0.0.1"), "no addresses or ids in labels");
    }

    // ---------------------------------------------------------------- access control

    async fn admin_and_user(store: &Store, user: &str, admin: bool) -> (String, String) {
        let a = store.user_create("admin", true).await.unwrap();
        let admin_key = store.key_create(&a.name, "k").await.unwrap().secret;
        let u = store.user_create(user, admin).await.unwrap();
        let key = store.key_create(&u.name, "k").await.unwrap().secret;
        (admin_key, key)
    }

    #[tokio::test]
    async fn read_grant_allows_reads_but_not_writes_and_audits_denials() {
        use reqwest::Method as M;
        let (url, store) = spawn_with(Some("root-token"), DefaultPolicy::None).await;
        let (admin, alice) = admin_and_user(&store, "alice", false).await;
        let g = store.grant_create("proj/read/*", "alice", Role::Read, "admin").await.unwrap();
        let u = |p: &str| format!("{url}{p}");

        assert_eq!(call(M::GET, &u("/v1/resume?project=proj/read/app"), Some(&alice), None).await.0, 200);
        assert_eq!(call(M::GET, &u("/v1/memories?project=proj/read/app"), Some(&alice), None).await.0, 200);

        let (st, _) =
            call(M::POST, &u("/v1/memories"), Some(&alice), Some(json!({"project": "proj/read/app", "content": "x"})))
                .await;
        assert_eq!(st, 403, "read grant can't write memories");
        let hook = json!({
            "project": "proj/read/app",
            "agent": "codex", "journal_prompts": false, "stop_guard_mins": 0,
            "input": {}
        });
        assert_eq!(call(M::POST, &u("/v1/hooks/post-tool-use"), Some(&alice), Some(hook)).await.0, 403, "hook writes");
        assert_eq!(call(M::GET, &u("/v1/resume?project=proj/secret"), Some(&alice), None).await.0, 403);

        // whoami shows the effective grants and the server default
        let (_, me) = call(M::GET, &u("/v1/whoami"), Some(&alice), None).await;
        assert_eq!(me["default_access"], "none");
        assert_eq!(me["grants"][0]["id"].as_str(), Some(g.id.as_str()));

        // every denial is in the audit log, with project and needed access
        let (_, audit) = call(M::GET, &u("/v1/admin/audit?actor=alice"), Some(&admin), None).await;
        let audit = audit.as_array().unwrap();
        assert!(audit.iter().all(|e| e["outcome"] == "denied"), "{audit:?}");
        assert!(audit.iter().any(|e| e["project"] == "proj/read/app" && e["detail"]["needed"] == "write"), "{audit:?}");
        assert!(audit.iter().any(|e| e["project"] == "proj/secret"), "{audit:?}");
    }

    #[tokio::test]
    async fn no_grant_under_none_is_denied_everywhere() {
        use reqwest::Method as M;
        let (url, store) = spawn_with(Some("root-token"), DefaultPolicy::None).await;
        let (admin, bob) = admin_and_user(&store, "bob", false).await;
        let u = |p: &str| format!("{url}{p}");

        for (method, path, body) in [
            (M::GET, "/v1/resume?project=any/project", None),
            (M::GET, "/v1/memories?project=any/project", None),
            (M::GET, "/v1/events?project=any/project", None),
            (M::GET, "/v1/checkpoints?project=any/project", None),
            (M::GET, "/v1/runs?project=any/project", None),
            (M::POST, "/v1/checkpoints", Some(json!({"project": "any/project", "goal": "x"}))),
            (M::POST, "/v1/delegate", Some(json!({"project": "any/project", "name": "x", "tasks": ["t"]}))),
        ] {
            let (st, res) = call(method.clone(), &u(path), Some(&bob), body).await;
            assert_eq!(st, 403, "{path}: {res}");
        }
        // non-project endpoints still work
        assert_eq!(call(M::GET, &u("/v1/whoami"), Some(&bob), None).await.0, 200);
        let (_, audit) = call(M::GET, &u("/v1/admin/audit?actor=bob&limit=50"), Some(&admin), None).await;
        assert_eq!(audit.as_array().unwrap().len(), 7, "one denial per request");
    }

    #[tokio::test]
    async fn run_endpoints_authorize_against_the_runs_stored_project() {
        use reqwest::Method as M;
        let (url, store) = spawn_with(Some("root-token"), DefaultPolicy::None).await;
        let (_, alice) = admin_and_user(&store, "alice", false).await;
        store.grant_create("proj/a", "alice", Role::Write, "admin").await.unwrap();
        let run_a = store
            .run_start(NewRun { project: "proj/a".into(), name: "a".into(), lease_secs: Some(0), ..Default::default() })
            .await
            .unwrap()
            .run;
        let run_b = store
            .run_start(NewRun { project: "proj/b".into(), name: "b".into(), ..Default::default() })
            .await
            .unwrap()
            .run;
        let u = |p: &str| format!("{url}{p}");

        assert_eq!(call(M::GET, &u(&format!("/v1/runs/{}", run_a.id)), Some(&alice), None).await.0, 200);
        assert_eq!(
            call(
                M::POST,
                &u(&format!("/v1/runs/{}/steps/s/begin", run_a.id)),
                Some(&alice),
                Some(json!({"owner": "w"}))
            )
            .await
            .0,
            200
        );

        // a caller-supplied project (query or body) can't override the stored one
        let denied = [
            (M::GET, format!("/v1/runs/{}?project=proj/a", run_b.id), None),
            (
                M::POST,
                format!("/v1/runs/{}/steps/s/begin?project=proj/a", run_b.id),
                Some(json!({"owner": "w", "project": "proj/a"})),
            ),
            (M::POST, format!("/v1/runs/{}/claim", run_b.id), Some(json!({"owner": "w", "project": "proj/a"}))),
            (
                M::POST,
                format!("/v1/runs/{}/finish", run_b.id),
                Some(json!({"outcome": "completed", "project": "proj/a"})),
            ),
        ];
        for (method, path, body) in denied {
            let (st, res) = call(method, &u(&path), Some(&alice), body).await;
            assert_eq!(st, 403, "{path}: {res}");
        }
    }

    #[tokio::test]
    async fn list_endpoints_only_return_readable_projects() {
        use reqwest::Method as M;
        let (url, store) = spawn_with(Some("root-token"), DefaultPolicy::None).await;
        let (_, alice) = admin_and_user(&store, "alice", false).await;
        store.grant_create("proj/a", "alice", Role::Read, "admin").await.unwrap();
        for p in ["proj/a", "proj/b"] {
            store
                .memory_save(NewMemory {
                    project: Some(p.into()),
                    content: format!("fact in {p}"),
                    ..Default::default()
                })
                .await
                .unwrap();
            store
                .run_start(NewRun { project: p.into(), name: format!("run in {p}"), ..Default::default() })
                .await
                .unwrap();
        }
        let u = |p: &str| format!("{url}{p}");

        let (st, memories) = call(M::GET, &u("/v1/memories?limit=50"), Some(&alice), None).await;
        assert_eq!(st, 200);
        let contents: Vec<_> = memories.as_array().unwrap().iter().map(|m| m["content"].as_str().unwrap()).collect();
        assert!(contents.contains(&"fact in proj/a") && !contents.contains(&"fact in proj/b"), "{memories}");

        let (st, runs) = call(M::GET, &u("/v1/runs"), Some(&alice), None).await;
        assert_eq!(st, 200);
        let projects: Vec<_> = runs.as_array().unwrap().iter().map(|r| r["project"].as_str().unwrap()).collect();
        assert_eq!(projects, vec!["proj/a"], "{runs}");

        assert_eq!(call(M::GET, &u("/v1/memories?project=proj/b"), Some(&alice), None).await.0, 403);
    }

    #[tokio::test]
    async fn grants_api_is_admin_only_and_default_write_is_compatible() {
        use reqwest::Method as M;
        let (url, store) = spawn_with(Some("root-token"), DefaultPolicy::Write).await;
        let (admin, alice) = admin_and_user(&store, "alice", false).await;
        let u = |p: &str| format!("{url}{p}");

        assert_eq!(call(M::GET, &u("/v1/admin/grants"), Some(&alice), None).await.0, 403);
        assert_eq!(
            call(
                M::POST,
                &u("/v1/admin/grants"),
                Some(&alice),
                Some(json!({"pattern":"p","user":"alice","role":"read"}))
            )
            .await
            .0,
            403
        );
        assert_eq!(call(M::GET, &u("/v1/admin/audit"), Some(&alice), None).await.0, 403);

        let (st, g) = call(
            M::POST,
            &u("/v1/admin/grants"),
            Some(&admin),
            Some(json!({"pattern": "p/*", "user": "alice", "role": "read"})),
        )
        .await;
        assert_eq!(st, 200, "{g}");
        let gid = g["id"].as_str().unwrap();
        let (_, list) = call(M::GET, &u("/v1/admin/grants?user=alice"), Some(&admin), None).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(call(M::DELETE, &u(&format!("/v1/admin/grants/{gid}")), Some(&admin), None).await.0, 200);
        assert_eq!(call(M::GET, &u("/v1/admin/grants?user=alice"), Some(&admin), None).await.0, 200);

        // default write: no grant still means today's behaviour
        let (st, _) = call(
            M::POST,
            &u("/v1/memories"),
            Some(&alice),
            Some(json!({"project": "no/grant", "content": "still writable"})),
        )
        .await;
        assert_eq!(st, 200);
        let (_, me) = call(M::GET, &u("/v1/whoami"), Some(&alice), None).await;
        assert_eq!(me["default_access"], "write");
    }
}
