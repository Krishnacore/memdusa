//! OAuth 2.1 authorization server (H2a): metadata, dynamic client
//! registration, API-key sign-in + consent, authorization code with PKCE,
//! refresh rotation with theft detection, revocation and session listing.
//!
//! External identity providers and the CLI browser/device flows (H2b/H2c) can
//! hook into `POST /oauth/signin` and `oauth_grant_find_or_create`: no other
//! part of the server needs to change.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::to_bytes;
use axum::extract::{ConnectInfo, Extension, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Form, Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use memdusa_core::Error;
use memdusa_core::auth::generate_secret;
use memdusa_core::model::*;
use memdusa_core::oauth::*;
use reqwest::Url;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::http::{ApiError, AppState};
use crate::oauth_upstream::{self, GITHUB, GOOGLE, Upstream, UpstreamConfig};

pub const ACCESS_SECS: i64 = 3600;
pub const REFRESH_SECS: i64 = 30 * 24 * 3600;
pub const ABSOLUTE_SECS: i64 = 365 * 24 * 3600;
pub const CODE_SECS: i64 = 600;
pub const TXN_SECS: i64 = 600;
pub const DEVICE_SECS: i64 = 600;
pub const DEVICE_INTERVAL_SECS: i64 = 5;

pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
pub const SUPPORTED_SCOPES: &[&str] = &["memdusa", "memdusa:read"];
const CLIENT_PREFIX: &str = "mdoc_";
const CSRF_COOKIE: &str = "memdusa_oauth";
const SIGNIN_FAIL_LIMIT: usize = 10;
const SIGNIN_WINDOW: Duration = Duration::from_secs(300);

/// IP-keyed back-pressure for all OAuth endpoints: 10 sustained requests/s
/// per caller, with a burst of 100 (independent of the API rate limit).
pub(crate) const OAUTH_RATE_PER_SEC: f64 = 10.0;
pub(crate) const OAUTH_RATE_BURST: f64 = 100.0;
/// Client registration and device authorization create rows without authentication: 10 at once, then one
/// every 10 seconds per caller.
pub(crate) const OAUTH_CREATE_PER_SEC: f64 = 0.1;
pub(crate) const OAUTH_CREATE_BURST: f64 = 10.0;

/// Server configuration, built from env when `MEMDUSA_PUBLIC_URL` is set.
#[derive(Clone, Debug)]
pub struct OAuthConfig {
    /// The external https base URL; also the token issuer.
    pub issuer: String,
    pub access_secs: i64,
    pub refresh_secs: i64,
    pub absolute_secs: i64,
    pub code_secs: i64,
    pub txn_secs: i64,
    pub device_secs: i64,
    /// How often a device client may poll the token endpoint (RFC 8628).
    pub device_interval_secs: i64,
    /// Upstream GitHub/Google providers (enabled by their credentials).
    pub upstream: UpstreamConfig,
}

impl OAuthConfig {
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let Ok(url) = std::env::var("MEMDUSA_PUBLIC_URL") else { return Ok(None) };
        let url = url.trim().trim_end_matches('/');
        if url.is_empty() {
            return Ok(None);
        }
        validate_issuer(url)?;
        let secs = |name: &str, default: i64| -> i64 {
            std::env::var(name).ok().and_then(|v| v.trim().parse::<i64>().ok()).filter(|v| *v > 0).unwrap_or(default)
        };
        let env_list = |name: &str| -> Vec<String> {
            std::env::var(name)
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let env = |name: &str| std::env::var(name).unwrap_or_default().trim().to_owned();
        use crate::oauth_upstream::{GitHubConfig, GoogleConfig};
        Ok(Some(Self {
            issuer: url.into(),
            access_secs: secs("MEMDUSA_OAUTH_ACCESS_SECS", ACCESS_SECS),
            refresh_secs: secs("MEMDUSA_OAUTH_REFRESH_SECS", REFRESH_SECS),
            absolute_secs: secs("MEMDUSA_OAUTH_ABSOLUTE_SECS", ABSOLUTE_SECS),
            code_secs: secs("MEMDUSA_OAUTH_CODE_SECS", CODE_SECS),
            txn_secs: secs("MEMDUSA_OAUTH_SIGNIN_SECS", TXN_SECS),
            device_secs: secs("MEMDUSA_OAUTH_DEVICE_SECS", DEVICE_SECS),
            device_interval_secs: secs("MEMDUSA_OAUTH_DEVICE_INTERVAL_SECS", DEVICE_INTERVAL_SECS),
            upstream: UpstreamConfig {
                github: GitHubConfig {
                    client_id: env("MEMDUSA_OAUTH_GITHUB_CLIENT_ID"),
                    client_secret: env("MEMDUSA_OAUTH_GITHUB_CLIENT_SECRET"),
                    // Test-only base-URL overrides (documented in the README).
                    auth_url: env("MEMDUSA_TEST_GITHUB_AUTH_URL"),
                    token_url: env("MEMDUSA_TEST_GITHUB_TOKEN_URL"),
                    api_url: env("MEMDUSA_TEST_GITHUB_API_URL"),
                    allowed_orgs: env_list("MEMDUSA_OAUTH_ALLOW_GITHUB_ORGS"),
                },
                google: GoogleConfig {
                    client_id: env("MEMDUSA_OAUTH_GOOGLE_CLIENT_ID"),
                    client_secret: env("MEMDUSA_OAUTH_GOOGLE_CLIENT_SECRET"),
                    // Test-only endpoint overrides (documented in the README).
                    discovery_url: env("MEMDUSA_TEST_GOOGLE_DISCOVERY_URL"),
                    jwks_url: env("MEMDUSA_TEST_GOOGLE_JWKS_URL"),
                    userinfo_url: env("MEMDUSA_TEST_GOOGLE_USERINFO_URL"),
                    allowed_domains: env_list("MEMDUSA_OAUTH_ALLOW_GOOGLE_DOMAINS"),
                },
            },
        }))
        .inspect(|cfg| {
            // The overrides point sign-in at another identity provider and, in test mode, accept
            // symmetric ID-token keys: fine for the test suite, dangerous on a real server.
            if let Some(cfg) = cfg
                && (cfg.upstream.github.has_overrides() || cfg.upstream.google.has_overrides())
            {
                tracing::warn!(
                    "MEMDUSA_TEST_* upstream overrides are set: GitHub/Google sign-in is pointed at test endpoints \
                     and accepts test keys. Never set these on a real server."
                );
            }
        })
    }

    /// Callback URLs to register with GitHub/Google for the enabled providers.
    pub fn callback_urls(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if self.upstream.github.enabled() {
            out.push((GITHUB.into(), format!("{}/oauth/callback/github", self.issuer)));
        }
        if self.upstream.google.enabled() {
            out.push((GOOGLE.into(), format!("{}/oauth/callback/google", self.issuer)));
        }
        out
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.issuer)
    }

    pub fn resource_metadata_url(&self) -> String {
        self.url("/.well-known/oauth-protected-resource")
    }

    pub fn www_authenticate(&self) -> String {
        // `scope` tells MCP clients what to request (spec: scope from the challenge first, else every
        // `scopes_supported`); without it they asked for `memdusa memdusa:read` together.
        format!("Bearer resource_metadata=\"{}\", scope=\"memdusa\"", self.resource_metadata_url())
    }

    pub fn is_secure(&self) -> bool {
        self.issuer.starts_with("https://")
    }
}

/// An issuer must be an absolute URL, https except for local development.
pub fn validate_issuer(url: &str) -> anyhow::Result<()> {
    let parsed = Url::parse(url)?;
    let https = parsed.scheme() == "https";
    let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    anyhow::ensure!(https || loopback, "MEMDUSA_PUBLIC_URL must use https (http allowed only on localhost/127.0.0.1)");
    anyhow::ensure!(parsed.username().is_empty() && parsed.password().is_none(), "issuer must not contain credentials");
    anyhow::ensure!(
        parsed.query().is_none() && parsed.fragment().is_none(),
        "issuer must not contain a query or fragment"
    );
    Ok(())
}

/// Shared state for OAuth endpoints: config + a tiny per-IP failed-sign-in limiter.
#[derive(Clone)]
pub struct OAuthRuntime {
    pub config: OAuthConfig,
    /// Upstream provider client and JWKS/discovery caches.
    pub upstream: Upstream,
    failures: Arc<Mutex<HashMap<String, (usize, Instant)>>>,
}

impl OAuthRuntime {
    pub fn new(config: OAuthConfig) -> Self {
        let upstream = Upstream::new(config.upstream.clone());
        Self { config, upstream, failures: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Record a failed sign-in; returns true when this IP is now blocked.
    fn failed(&self, ip: &str) -> bool {
        let mut hits = self.failures.lock().expect("sign-in limiter poisoned");
        let now = Instant::now();
        let entry = hits.entry(ip.to_owned()).or_insert((0, now));
        if now.duration_since(entry.1) > SIGNIN_WINDOW {
            *entry = (0, now);
        }
        entry.0 += 1;
        entry.0 > SIGNIN_FAIL_LIMIT
    }

    fn succeeded(&self, ip: &str) {
        self.failures.lock().expect("sign-in limiter poisoned").remove(ip);
    }
}

fn cfg(s: &AppState) -> &OAuthRuntime {
    s.oauth.as_ref().expect("OAuth routes are only mounted when OAuth is enabled")
}

pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/.well-known/oauth-protected-resource", get(protected_resource))
        .route("/.well-known/oauth-authorization-server", get(server_metadata))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/signin", post(signin))
        .route("/oauth/signin/{provider}", get(upstream_start))
        .route("/oauth/callback/{provider}", get(upstream_callback))
        .route("/oauth/consent", post(consent))
        .route("/oauth/device_authorization", post(device_authorization))
        .route("/oauth/device", get(device_page).post(device_code_submit))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
}

/// IP-keyed limiter for every `/oauth/*` route. These callers have no bearer
/// identity to key on, so use the connecting IP with a fixed fallback.
pub async fn rate_limit(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let key = req
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map_or_else(|| "unknown".to_owned(), |c| format!("ip:{}", c.0.ip()));
    let creates = matches!(req.uri().path(), "/oauth/register" | "/oauth/device_authorization");
    let checked =
        s.oauth_limiter.check(&key).and_then(|()| if creates { s.oauth_create_limiter.check(&key) } else { Ok(()) });
    match checked {
        Ok(()) => next.run(req).await,
        Err(wait) => {
            s.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
            let secs = wait.as_secs_f64().ceil().max(1.0) as u64;
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, secs.to_string())],
                Json(json!({
                    "error": "rate_limited",
                    "error_description": format!("rate limit exceeded; retry in {secs}s"),
                })),
            )
                .into_response()
        }
    }
}

/// Clickjacking / privacy headers on every HTML response from the OAuth
/// router. Applied as a layer so no individual handler return can forget them.
pub async fn html_security_headers(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    if resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"))
    {
        let h = resp.headers_mut();
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("frame-ancestors 'none'"));
        h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
        h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    resp
}

/// Session endpoints need the normal bearer-auth middleware (they produce the
/// same `Identity` every other `/v1` handler sees).
pub fn session_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/me/sessions", get(my_sessions))
        .route("/v1/me/sessions/{id}", delete(my_session_revoke))
        .route("/v1/admin/sessions", get(admin_sessions))
        .route("/v1/admin/sessions/{id}", delete(admin_session_revoke))
}

/// Identity linking endpoints (admin-only; all handlers check `Identity.admin`).
pub fn identity_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/admin/users/{name}/identities", get(identity_list).post(identity_link))
        .route("/v1/admin/users/{name}/identities/{provider}/{handle}", delete(identity_unlink))
}

// ---------------------------------------------------------------- metadata

async fn protected_resource(State(s): State<AppState>) -> Response {
    let c = &cfg(&s).config;
    Json(json!({
        "resource": c.url("/mcp"),
        "authorization_servers": [c.issuer],
        "bearer_methods_supported": ["header"],
        "scopes_supported": SUPPORTED_SCOPES,
    }))
    .into_response()
}

async fn server_metadata(State(s): State<AppState>) -> Response {
    let c = &cfg(&s).config;
    Json(json!({
        "issuer": c.issuer,
        "authorization_endpoint": c.url("/oauth/authorize"),
        "token_endpoint": c.url("/oauth/token"),
        "registration_endpoint": c.url("/oauth/register"),
        "revocation_endpoint": c.url("/oauth/revoke"),
        "device_authorization_endpoint": c.url("/oauth/device_authorization"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token", DEVICE_GRANT],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "revocation_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": SUPPORTED_SCOPES,
    }))
    .into_response()
}

// ------------------------------------------------------------- registration

#[derive(Debug, Deserialize)]
struct RegisterRequest {
    #[serde(default)]
    redirect_uris: Vec<String>,
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
    #[serde(default)]
    grant_types: Option<Vec<String>>,
    #[serde(default)]
    response_types: Option<Vec<String>>,
    #[serde(default)]
    scope: Option<String>,
}

/// https redirects, plus loopback http with any port (RFC 8252).
fn valid_redirect(uri: &str) -> bool {
    let Ok(u) = Url::parse(uri) else { return false };
    if !u.username().is_empty() || u.password().is_some() || u.fragment().is_some() {
        return false;
    }
    match u.scheme() {
        "https" => u.host_str().is_some_and(|h| !h.is_empty()),
        "http" => matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        _ => false,
    }
}

async fn register(State(s): State<AppState>, Json(b): Json<RegisterRequest>) -> Response {
    let grant_types = b.grant_types.unwrap_or_else(|| vec!["authorization_code".into(), "refresh_token".into()]);
    if grant_types.is_empty()
        || !grant_types.iter().all(|g| matches!(g.as_str(), "authorization_code" | "refresh_token" | DEVICE_GRANT))
    {
        return invalid("unsupported grant_type");
    }
    // Bound unauthenticated dynamic registration: at most 10 redirects, each
    // short enough to stop unbounded rows/audit details from a single caller.
    if b.redirect_uris.len() > 10 {
        return invalid("at most 10 redirect_uris are allowed");
    }
    if b.redirect_uris.iter().any(|u| u.chars().count() > 2000) {
        return invalid("redirect_uri must be at most 2000 characters");
    }
    // RFC 8628 clients have no redirect: only they may register without one.
    let device_only = grant_types.iter().all(|g| g == DEVICE_GRANT);
    if (b.redirect_uris.is_empty() && !device_only) || !b.redirect_uris.iter().all(|u| valid_redirect(u)) {
        return invalid("redirect_uris must be https URLs or loopback http URLs");
    }
    if b.token_endpoint_auth_method.as_deref().is_some_and(|m| m != "none") {
        return invalid("only public clients (token_endpoint_auth_method=none) are supported");
    }
    if let Some(types) = &b.response_types
        && (types.len() != 1 || types[0] != "code")
    {
        return invalid("only response_type=code is supported");
    }
    if let Some(scope) = &b.scope
        && !valid_scope(scope)
    {
        return invalid("unsupported scope");
    }
    // Echo what the client asked for (default: full access). Advertising every supported scope made
    // clients (e.g. the MCP Python SDK) request `memdusa memdusa:read` when they only wanted `memdusa`.
    // The scope a token actually gets is decided (and shown on the consent page) at /oauth/authorize.
    let registered_scope = b.scope.clone().unwrap_or_else(|| "memdusa".into());
    let name = b.client_name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "MCP client".into());
    let name = name.chars().take(200).collect::<String>();
    let client = OAuthClient {
        id: new_id(),
        client_id: generate_secret(CLIENT_PREFIX).unwrap_or_else(|_| format!("{CLIENT_PREFIX}{}", new_id())),
        name,
        redirect_uris: b.redirect_uris,
        grant_types,
        created_at: now_ms(),
    };
    if let Err(e) = s.store.oauth_client_create(&client).await {
        return internal(&e);
    }
    if let Err(e) = s
        .store
        .audit(NewAuditEvent::ok("anonymous", "oauth.register").target(&client.client_id).detail(json!({
            "name": client.name, "redirect_uris": client.redirect_uris
        })))
        .await
    {
        tracing::warn!("could not audit client registration: {e}");
    }
    Json(json!({
        "client_id": client.client_id,
        "client_id_issued_at": client.created_at / 1000,
        "client_name": client.name,
        "redirect_uris": client.redirect_uris,
        "token_endpoint_auth_method": "none",
        "grant_types": client.grant_types,
        "response_types": ["code"],
        "scope": registered_scope,
    }))
    .into_response()
}

// ---------------------------------------------------------------- authorize

#[derive(Debug, Deserialize)]
struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    scope: Option<String>,
    resource: Option<String>,
}

async fn authorize(State(s): State<AppState>, Query(q): Query<AuthorizeQuery>) -> Response {
    let (Some(client_id), Some(redirect_uri), Some(challenge), Some(state)) =
        (q.client_id, q.redirect_uri, q.code_challenge, q.state)
    else {
        return invalid("client_id, redirect_uri, code_challenge and state are required");
    };
    if q.response_type.as_deref() != Some("code") {
        return invalid("response_type must be code");
    }
    if q.code_challenge_method.as_deref() != Some("S256") {
        return invalid("code_challenge_method must be S256 (plain is not supported)");
    }
    if challenge.len() < 43
        || challenge.len() > 128
        || !challenge.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return invalid("invalid code_challenge");
    }
    if state.is_empty() {
        return invalid("state must not be empty");
    }
    let scope = match q.scope {
        Some(s) if !s.trim().is_empty() => s.trim().to_owned(),
        _ => "memdusa".into(),
    };
    if !valid_scope(&scope) {
        return invalid("unsupported scope");
    }
    let Some(client) = (match s.store.oauth_client_get(&client_id).await {
        Ok(c) => c,
        Err(e) => return internal(&e),
    }) else {
        return invalid("unknown client_id");
    };
    if !client.redirect_uris.contains(&redirect_uri) {
        return invalid("redirect_uri is not registered for this client");
    }
    let txn = OAuthTransaction {
        csrf: generate_secret("csrf_").unwrap_or_else(|_| format!("csrf_{}", new_id())),
        client_id,
        redirect_uri,
        code_challenge: challenge,
        scope,
        resource: q.resource.filter(|r| !r.is_empty()),
        state,
        user_name: None,
        created_at: now_ms(),
        upstream: None,
        signin_method: "api_key".into(),
    };
    if let Err(e) = s.store.oauth_txn_create(&txn).await {
        return internal(&e);
    }
    let secs = cfg(&s).config.txn_secs;
    let cookie = cookie_header(&s, &txn.csrf, false, secs);
    let html = signin_html(&txn, &client.name, None, &cfg(&s).config.upstream);
    ([(header::SET_COOKIE, cookie), (header::CACHE_CONTROL, "no-store".to_owned())], Html(html)).into_response()
}

#[derive(Debug, Deserialize)]
struct SignInForm {
    csrf: String,
    api_key: String,
}

async fn signin(
    State(s): State<AppState>,
    headers: HeaderMap,
    ip: Result<ConnectInfo<std::net::SocketAddr>, axum::extract::rejection::ExtensionRejection>,
    Form(f): Form<SignInForm>,
) -> Response {
    let txn = match check_csrf(&s, &headers, &f.csrf).await {
        Some(t) => t,
        None => return invalid("missing or expired sign-in cookie"),
    };
    let client_name = s.store.oauth_client_get(&txn.client_id).await.ok().flatten().map(|c| c.name).unwrap_or_default();
    let ip_key = ip.ok().map_or_else(|| "unknown".into(), |c| c.0.ip().to_string());
    let auth = s.store.authenticate(&f.api_key).await;
    let identity = match auth {
        Ok(Some(id)) => id,
        Ok(None) => {
            let blocked = cfg(&s).failed(&ip_key);
            if let Err(e) = s
                .store
                .audit(
                    NewAuditEvent::denied("anonymous", "oauth.signin")
                        .detail(json!({"ip": ip_key, "blocked": blocked})),
                )
                .await
            {
                tracing::warn!("could not audit failed sign-in: {e}");
            }
            if blocked {
                return err(StatusCode::TOO_MANY_REQUESTS, "slow_down", "too many failed sign-ins; try again later");
            }
            let html =
                signin_html(&txn, &client_name, Some("That API key didn't work. Try again."), &cfg(&s).config.upstream);
            return (StatusCode::UNAUTHORIZED, Html(html)).into_response();
        }
        Err(e) => return internal(&e),
    };
    let user = identity.user.clone().unwrap_or_default();
    cfg(&s).succeeded(&ip_key);
    if txn.redirect_uri.is_empty() {
        // Device flow: remember who signed in, then show the same consent page.
        if let Err(e) = s.store.oauth_device_set_user(&txn.csrf, &user).await {
            return internal(&e);
        }
        if let Err(e) = s
            .store
            .audit(
                NewAuditEvent::ok(&user, "oauth.device_signin")
                    .target(&txn.client_id)
                    .detail(json!({"method": "api_key"})),
            )
            .await
        {
            tracing::warn!("could not audit device sign-in: {e}");
        }
        return Html(consent_html(&user, &client_name, "this device", &txn.scope, &txn.csrf, &device_providers()))
            .into_response();
    }
    if let Err(e) = s.store.oauth_txn_set_user(&txn.csrf, &user).await {
        return internal(&e);
    }
    if let Err(e) = s
        .store
        .audit(NewAuditEvent::ok(&user, "oauth.signin").target(&txn.client_id).detail(json!({"method": "api_key"})))
        .await
    {
        tracing::warn!("could not audit sign-in: {e}");
    }
    Html(consent_html(&user, &client_name, &txn.redirect_uri, &txn.scope, &txn.csrf, &cfg(&s).config.upstream))
        .into_response()
}

#[derive(Debug, Deserialize)]
struct ConsentForm {
    csrf: String,
    decision: String,
}

async fn consent(State(s): State<AppState>, headers: HeaderMap, Form(f): Form<ConsentForm>) -> Response {
    let txn = match check_csrf(&s, &headers, &f.csrf).await {
        Some(t) => t,
        None => return invalid("missing or expired sign-in cookie"),
    };
    let Some(user) = txn.user_name.clone() else { return invalid("sign in first") };
    let clear = cookie_header(&s, "", true, 0);
    if txn.redirect_uri.is_empty() {
        let allowed = f.decision == "allow";
        let status = if allowed { device_status::APPROVED } else { device_status::DENIED };
        if let Err(e) = s.store.oauth_device_set_status(&txn.csrf, status).await {
            return internal(&e);
        }
        if allowed
            && let Err(e) = s
                .store
                .oauth_grant_find_or_create(&user, &txn.client_id, &txn.scope, txn.resource.as_deref(), "device")
                .await
        {
            return internal(&e);
        }
        if let Err(e) = s
            .store
            .audit(
                NewAuditEvent::ok(&user, if allowed { "oauth.device_approved" } else { "oauth.device_denied" })
                    .target(&txn.client_id),
            )
            .await
        {
            tracing::warn!("could not audit device consent: {e}");
        }
        return ([(header::SET_COOKIE, clear)], Html(device_done_html(allowed))).into_response();
    }
    if let Err(e) = s.store.oauth_txn_delete(&txn.csrf).await {
        return internal(&e);
    }
    if f.decision != "allow" {
        return (
            StatusCode::FOUND,
            [
                (
                    header::LOCATION,
                    with_params(&txn.redirect_uri, &[("error", "access_denied"), ("state", &txn.state)]),
                ),
                (header::SET_COOKIE, clear),
            ],
        )
            .into_response();
    }
    let grant = match s
        .store
        .oauth_grant_find_or_create(&user, &txn.client_id, &txn.scope, txn.resource.as_deref(), "api_key")
        .await
    {
        Ok(g) => g,
        Err(e) => return internal(&e),
    };
    let secret = generate_secret(CODE_PREFIX).unwrap_or_else(|_| format!("{CODE_PREFIX}{}", new_id()));
    let code = OAuthCode {
        id: new_id(),
        secret: secret.clone(),
        client_id: txn.client_id.clone(),
        redirect_uri: txn.redirect_uri.clone(),
        code_challenge: txn.code_challenge.clone(),
        scope: txn.scope.clone(),
        resource: txn.resource.clone(),
        grant_id: grant.id.clone(),
        used_at: None,
        expires_at: now_ms() + cfg(&s).config.code_secs * 1000,
        created_at: now_ms(),
    };
    if let Err(e) = s.store.oauth_code_create(&code).await {
        return internal(&e);
    }
    let location = with_params(&txn.redirect_uri, &[("code", &secret), ("state", &txn.state)]);
    (
        StatusCode::FOUND,
        [(header::LOCATION, location), (header::SET_COOKIE, clear), (header::CACHE_CONTROL, "no-store".to_owned())],
    )
        .into_response()
}

// ----------------------------------------------------------- device flow (H2c)

/// Generate a human-friendly code: 8 characters from an alphabet without
/// look-alikes, in `XXXX-XXXX` form. Both halves are random.
fn generate_user_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        // Still unique enough (storage enforces it); only randomness is degraded.
        let n = now_ms().unsigned_abs();
        bytes = std::array::from_fn(|i| (n >> (i * 8)) as u8);
    }
    let chars: String = bytes.iter().map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char).collect();
    format!("{}-{}", &chars[..4], &chars[4..])
}

/// The device sign-in/consent pages reuse the ordinary ones by presenting the
/// device request as a transaction with no redirect URI.
fn device_txn(d: &OAuthDeviceCode) -> OAuthTransaction {
    OAuthTransaction {
        csrf: d.id.clone(),
        client_id: d.client_id.clone(),
        redirect_uri: String::new(),
        code_challenge: String::new(),
        scope: d.scope.clone(),
        resource: d.resource.clone(),
        state: String::new(),
        user_name: d.user_name.clone(),
        created_at: d.created_at,
        // Device sign-in is API-key only for now: the GitHub/Google round trip keys its state on a
        // stored transaction, and a device request isn't one (see `device_providers`).
        upstream: None,
        signin_method: "api_key".into(),
    }
}

/// Providers offered on the device pages: none yet (API-key sign-in only), see [`device_txn`].
fn device_providers() -> UpstreamConfig {
    UpstreamConfig::default()
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorizationRequest {
    client_id: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

async fn form_or_json<T: serde::de::DeserializeOwned>(req: Request, limit: usize) -> Option<T> {
    let bytes = to_bytes(req.into_body(), limit).await.ok()?;
    serde_urlencoded::from_bytes(&bytes).ok().or_else(|| serde_json::from_slice(&bytes).ok())
}

async fn device_authorization(State(s): State<AppState>, req: Request) -> Response {
    let Some(b): Option<DeviceAuthorizationRequest> = form_or_json(req, 64 * 1024).await else {
        return invalid("expected form-encoded or JSON body");
    };
    if b.client_id.trim().is_empty() {
        return invalid("client_id is required");
    }
    let client = match s.store.oauth_client_get(&b.client_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return invalid("unknown client_id"),
        Err(e) => return internal(&e),
    };
    if !client.grant_types.iter().any(|g| g == DEVICE_GRANT) {
        return err(StatusCode::BAD_REQUEST, "unauthorized_client", "client cannot use the device flow");
    }
    let scope = b.scope.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "memdusa".into());
    if !valid_scope(&scope) {
        return invalid("unsupported scope");
    }
    let c = &cfg(&s).config;
    let device_code = generate_secret(DEVICE_PREFIX).unwrap_or_else(|_| format!("{DEVICE_PREFIX}{}", new_id()));
    let user_code = generate_user_code();
    let device = OAuthDeviceCode {
        id: new_id(),
        user_code: user_code.clone(),
        client_id: client.client_id.clone(),
        scope: scope.clone(),
        resource: b.resource.filter(|r| !r.is_empty()),
        expires_at: now_ms() + c.device_secs * 1000,
        interval: c.device_interval_secs,
        status: device_status::PENDING.into(),
        user_name: None,
        last_polled_at: None,
        created_at: now_ms(),
    };
    if let Err(e) = s.store.oauth_device_create(&device, &device_code).await {
        return internal(&e);
    }
    if let Err(e) = s
        .store
        .audit(NewAuditEvent::ok("anonymous", "oauth.device_authorization").target(&client.client_id).detail(json!({
            "scope": scope, "user_code": user_code
        })))
        .await
    {
        tracing::warn!("could not audit device authorization: {e}");
    }
    Json(json!({
        "device_code": device_code,
        "user_code": user_code,
        "verification_uri": c.url("/oauth/device"),
        "verification_uri_complete": with_params(&c.url("/oauth/device"), &[("user_code", &user_code)]),
        "expires_in": c.device_secs,
        "interval": c.device_interval_secs,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct DevicePageQuery {
    user_code: Option<String>,
}

async fn device_page(State(s): State<AppState>, Query(q): Query<DevicePageQuery>) -> Response {
    let csrf = generate_secret("csrf_").unwrap_or_else(|_| format!("csrf_{}", new_id()));
    let secs = cfg(&s).config.device_secs;
    let (cookie, html) = match q.user_code.filter(|c| !c.trim().is_empty()) {
        Some(code) => match s.store.oauth_device_get_by_user_code(&code).await {
            Ok(Some(d)) if d.status == device_status::PENDING && d.expires_at > now_ms() => {
                let name =
                    s.store.oauth_client_get(&d.client_id).await.ok().flatten().map(|c| c.name).unwrap_or_default();
                (cookie_header(&s, &d.id, false, secs), signin_html(&device_txn(&d), &name, None, &device_providers()))
            }
            _ => (
                cookie_header(&s, &csrf, false, secs),
                device_code_html(&csrf, Some("That code is invalid, already used, or has expired.")),
            ),
        },
        None => (cookie_header(&s, &csrf, false, secs), device_code_html(&csrf, None)),
    };
    ([(header::SET_COOKIE, cookie), (header::CACHE_CONTROL, "no-store".to_owned())], Html(html)).into_response()
}

#[derive(Debug, Deserialize)]
struct DeviceCodeForm {
    csrf: String,
    user_code: String,
}

async fn device_code_submit(State(s): State<AppState>, headers: HeaderMap, Form(f): Form<DeviceCodeForm>) -> Response {
    if cookie_value(&headers).as_deref() != Some(f.csrf.as_str()) {
        return invalid("missing or expired device cookie");
    }
    match s.store.oauth_device_get_by_user_code(&f.user_code).await {
        Ok(Some(d)) if d.status == device_status::PENDING && d.expires_at > now_ms() => {
            let name = s.store.oauth_client_get(&d.client_id).await.ok().flatten().map(|c| c.name).unwrap_or_default();
            let cookie = cookie_header(&s, &d.id, false, cfg(&s).config.device_secs);
            (
                [(header::SET_COOKIE, cookie), (header::CACHE_CONTROL, "no-store".to_owned())],
                Html(signin_html(&device_txn(&d), &name, None, &device_providers())),
            )
                .into_response()
        }
        Ok(_) => (StatusCode::BAD_REQUEST, Html(device_code_html(&f.csrf, Some("That code is invalid or expired."))))
            .into_response(),
        Err(e) => internal(&e),
    }
}

fn device_code_html(csrf: &str, error: Option<&str>) -> String {
    let error = error.map(|e| format!("<p class=\"err\">{}</p>", esc(e))).unwrap_or_default();
    html_page(
        "Sign in · memdusa",
        format!(
            "<h1>Sign in to memdusa</h1><p>Enter the code shown on your device.</p>{}\
             <form method=\"post\" action=\"/oauth/device\">\
             <input type=\"hidden\" name=\"csrf\" value=\"{}\">\
             <label for=\"user_code\">Device code</label>\
             <input id=\"user_code\" name=\"user_code\" autocomplete=\"off\" required>\
             <button type=\"submit\">Continue</button></form>",
            error,
            esc(csrf)
        ),
    )
}

fn device_done_html(approved: bool) -> String {
    let (title, body) = if approved {
        ("Device approved", "This device is approved. You can close this window and return to your terminal.")
    } else {
        ("Device access denied", "No access was granted. You can close this window.")
    };
    html_page("memdusa", format!("<h1>{title}</h1><p>{body}</p>"))
}

// ------------------------------------------------------------ upstream (H2b)

#[derive(Debug, Deserialize)]
struct UpstreamStartQuery {
    csrf: String,
}

/// Start a GitHub/Google sign-in from the sign-in page (or a link from the
/// consent page). The pending memdusa transaction is carried through in the
/// `state` parameter so the callback can bind back to it.
async fn upstream_start(
    State(s): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(provider): axum::extract::Path<String>,
    Query(q): Query<UpstreamStartQuery>,
) -> Response {
    let Some(txn) = check_csrf(&s, &headers, &q.csrf).await else {
        return invalid("missing or expired sign-in cookie");
    };
    let provider = provider.to_lowercase();
    let upstream = &cfg(&s).upstream;
    if !upstream.enabled(&provider) {
        return info_page("Not configured", &format!("Sign-in with {provider} is not configured on this server."));
    }
    let state = format!("{}.{}", txn.csrf, generate_secret("st_").unwrap_or_else(|_| format!("st_{}", new_id())));
    let verifier = generate_secret("pkce_").unwrap_or_else(|_| format!("pkce_{}", new_id()));
    let nonce = if provider == GOOGLE {
        Some(generate_secret("nonce_").unwrap_or_else(|_| format!("nonce_{}", new_id())))
    } else {
        None
    };
    let callback = upstream.callback_url(&cfg(&s).config.issuer, &provider);
    let url = match upstream.authorize_url(&provider, &callback, &state, &verifier, nonce.as_deref()).await {
        Ok(url) => url,
        Err(e) => {
            tracing::warn!("could not build {provider} authorize URL: {e}");
            return info_page("Sign-in failed", "Could not start the upstream sign-in. Please try again.");
        }
    };
    let bound = UpstreamTxn { provider: provider.clone(), state, verifier, nonce };
    if let Err(e) = s.store.oauth_txn_set_upstream(&txn.csrf, &bound, &provider).await {
        return internal(&e);
    }
    (StatusCode::FOUND, [(header::LOCATION, url), (header::CACHE_CONTROL, "no-store".to_owned())]).into_response()
}

#[derive(Debug, Deserialize)]
struct UpstreamCallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

async fn upstream_callback(
    State(s): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(provider): axum::extract::Path<String>,
    Query(q): Query<UpstreamCallbackQuery>,
) -> Response {
    let provider = provider.to_lowercase();
    let state = q.state.as_deref().unwrap_or_default();
    let (csrf, _) = match state.split_once('.') {
        Some(pair) => pair,
        None => return invalid("invalid upstream state"),
    };
    let Some(txn) = check_csrf(&s, &headers, csrf).await else {
        return invalid("missing or expired sign-in cookie");
    };
    let Some(bound) = txn.upstream.as_ref().filter(|u| u.provider == provider && u.state == state) else {
        // State mismatch (wrong provider, replay, or CSRF) is refused.
        tracing::warn!("upstream state mismatch for {provider}");
        let _ = s
            .store
            .audit(NewAuditEvent::denied("anonymous", "oauth.upstream_state_mismatch").target(&provider).detail(
                json!({
                    "provider": provider
                }),
            ))
            .await;
        return info_page("Sign-in failed", "The upstream response did not match this sign-in. Please try again.");
    };
    let bound = bound.clone();

    if let Some(e) = q.error {
        tracing::warn!("{provider} authorize error: {} / {}", e, q.error_description.as_deref().unwrap_or(""));
        let _ = s
            .store
            .audit(NewAuditEvent::denied("anonymous", "oauth.upstream_error").target(&provider).detail(json!({
                "provider": provider, "error": e
            })))
            .await;
        return info_page("Sign-in failed", "The identity provider could not complete the sign-in. Please try again.");
    }
    let Some(code) = q.code else { return invalid("missing upstream code") };

    let upstream = &cfg(&s).upstream;
    let callback = upstream.callback_url(&cfg(&s).config.issuer, &provider);
    let identity = match upstream.exchange(&provider, &code, &bound.verifier, &callback, bound.nonce.as_deref()).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("{provider} identity exchange failed: {e}");
            let _ = s
                .store
                .audit(NewAuditEvent::denied("anonymous", "oauth.upstream_exchange_failed").target(&provider).detail(
                    json!({
                        "provider": provider, "error": e.to_string()
                    }),
                ))
                .await;
            return info_page("Sign-in failed", "Could not verify your identity with the provider. Please try again.");
        }
    };

    let signed_in = txn.user_name.as_deref();
    let user = match oauth_upstream::resolve_identity(&s.store, &identity, upstream, signed_in).await {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("identity resolution failed: {e}");
            let _ = s
                .store
                .audit(NewAuditEvent::denied("anonymous", "oauth.identity_link_conflict").target(&provider).detail(
                    json!({
                        "provider": provider, "subject": identity.subject, "error": e.to_string()
                    }),
                ))
                .await;
            return info_page("Sign-in failed", &format!("{e}"));
        }
    };

    let Some(user) = user else {
        let _ = s
            .store
            .audit(NewAuditEvent::denied("anonymous", "oauth.identity_unknown").target(&provider).detail(json!({
                "provider": provider, "subject": identity.subject, "display": identity.display
            })))
            .await;
        return not_linked_html(&provider, &identity.display);
    };

    if let Err(e) = s.store.oauth_txn_set_user(&txn.csrf, &user).await {
        return internal(&e);
    }
    if let Err(e) = s
        .store
        .audit(NewAuditEvent::ok(&user, "oauth.signin").target(&txn.client_id).detail(json!({
            "method": provider, "provider": provider, "subject": identity.subject
        })))
        .await
    {
        tracing::warn!("could not audit upstream sign-in: {e}");
    }
    let client_name = s.store.oauth_client_get(&txn.client_id).await.ok().flatten().map(|c| c.name).unwrap_or_default();
    Html(consent_html(&user, &client_name, &txn.redirect_uri, &txn.scope, &txn.csrf, &cfg(&s).config.upstream))
        .into_response()
}

// -------------------------------------------------------------- identities

async fn identity_list(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    if !id.admin {
        return ApiError(Error::Forbidden("admin only".into())).into_response();
    }
    match s.store.identity_list(&name).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => ApiError(e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct LinkBody {
    provider: String,
    identity: String,
}

async fn identity_link(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    axum::extract::Path(name): axum::extract::Path<String>,
    Json(b): Json<LinkBody>,
) -> Response {
    if !id.admin {
        return ApiError(Error::Forbidden("admin only".into())).into_response();
    }
    let (provider, handle) = match oauth_upstream::parse_link(&format!("{}:{}", b.provider, b.identity)) {
        Ok(v) => v,
        Err(e) => return invalid(&e.to_string()),
    };
    let (subject, login, email) = if oauth_upstream::is_stable_subject(&handle) {
        (handle.clone(), None, None)
    } else if provider == GITHUB {
        (format!("{}{}", oauth_upstream::PENDING_PREFIX, handle), Some(handle.clone()), None)
    } else {
        (format!("{}{}", oauth_upstream::PENDING_PREFIX, handle), None, Some(handle.clone()))
    };
    match s.store.identity_insert(&name, &provider, &subject, email.as_deref(), login.as_deref()).await {
        Ok(inserted) => {
            if let Err(e) = s
                .store
                .audit(NewAuditEvent::ok(id.actor(), "oauth.identity_link").target(&name).detail(json!({
                    "provider": provider, "handle": handle
                })))
                .await
            {
                tracing::warn!("could not audit identity link: {e}");
            }
            Json(inserted).into_response()
        }
        Err(e) => ApiError(e).into_response(),
    }
}

async fn identity_unlink(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    axum::extract::Path((name, provider, handle)): axum::extract::Path<(String, String, String)>,
) -> Response {
    if !id.admin {
        return ApiError(Error::Forbidden("admin only".into())).into_response();
    }
    match s.store.identity_unlink(&name, &provider, &handle).await {
        Ok(n) => {
            if let Err(e) = s
                .store
                .audit(NewAuditEvent::ok(id.actor(), "oauth.identity_unlink").target(&name).detail(json!({
                    "provider": provider, "handle": handle, "removed": n
                })))
                .await
            {
                tracing::warn!("could not audit identity unlink: {e}");
            }
            Json(json!({ "unlinked": n })).into_response()
        }
        Err(e) => ApiError(e).into_response(),
    }
}

// -------------------------------------------------------------------- token

#[derive(Debug, Deserialize)]
struct TokenRequest {
    grant_type: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    device_code: Option<String>,
    #[serde(default)]
    redirect_uri: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    code_verifier: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

async fn token(State(s): State<AppState>, req: Request) -> Response {
    let Ok(bytes) = to_bytes(req.into_body(), 1024 * 1024).await else { return invalid("body too large") };
    let body: TokenRequest = match serde_urlencoded::from_bytes(&bytes) {
        Ok(v) => v,
        Err(_) => match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => return invalid("expected form-encoded or JSON body"),
        },
    };
    match body.grant_type.as_str() {
        "authorization_code" => code_to_token(&s, &body).await,
        "refresh_token" => refresh_to_token(&s, &body).await,
        g if g == DEVICE_GRANT => device_to_token(&s, &body).await,
        _ => err(StatusCode::BAD_REQUEST, "unsupported_grant_type", "unsupported grant_type"),
    }
}

async fn code_to_token(s: &AppState, b: &TokenRequest) -> Response {
    let (Some(code), Some(verifier), Some(client_id), Some(redirect_uri)) =
        (b.code.as_deref(), b.code_verifier.as_deref(), b.client_id.as_deref(), b.redirect_uri.as_deref())
    else {
        return invalid("code, code_verifier, client_id and redirect_uri are required");
    };
    let Some(code) = (match s.store.oauth_code_get(code).await {
        Ok(c) => c,
        Err(e) => return internal(&e),
    }) else {
        return invalid_grant("unknown authorization code");
    };
    if code.used_at.is_some() {
        return invalid_grant("authorization code already used");
    }
    if code.expires_at <= now_ms() {
        return invalid_grant("authorization code expired");
    }
    if code.client_id != client_id || code.redirect_uri != redirect_uri {
        return invalid_grant("client_id or redirect_uri mismatch");
    }
    if b.resource.as_deref().is_some() && b.resource != code.resource {
        return invalid_grant("resource mismatch");
    }
    if !pkce_ok(verifier, &code.code_challenge) {
        return invalid_grant("PKCE verification failed");
    }
    // Claim atomically: two concurrent exchanges can't both mint tokens.
    let Some(secret) = b.code.as_deref() else { return invalid_grant("missing code") };
    let claimed = match s.store.oauth_code_claim(secret, now_ms()).await {
        Ok(v) => v,
        Err(e) => return internal(&e),
    };
    if !claimed {
        return invalid_grant("authorization code already used");
    }
    let Some(grant) = (match s.store.oauth_grant_get(&code.grant_id).await {
        Ok(g) => g,
        Err(e) => return internal(&e),
    }) else {
        return invalid_grant("grant no longer exists");
    };
    if grant.revoked_at.is_some() {
        return invalid_grant("grant revoked");
    }
    issue_tokens(s, &grant, None).await
}

async fn device_to_token(s: &AppState, b: &TokenRequest) -> Response {
    let (Some(device_code), Some(client_id)) = (b.device_code.as_deref(), b.client_id.as_deref()) else {
        return invalid("device_code and client_id are required");
    };
    let Some(d) = (match s.store.oauth_device_get(device_code).await {
        Ok(d) => d,
        Err(e) => return internal(&e),
    }) else {
        return invalid_grant("unknown device_code");
    };
    if d.client_id != client_id {
        return invalid_grant("client_id mismatch");
    }
    if b.resource.as_deref().is_some() && b.resource != d.resource {
        return invalid_grant("resource mismatch");
    }
    let now = now_ms();
    if d.expires_at <= now {
        return err(StatusCode::BAD_REQUEST, "expired_token", "device code expired");
    }
    let too_fast = match s.store.oauth_device_poll(&d.id, now, d.interval.max(1) * 1000).await {
        Ok(v) => v,
        Err(e) => return internal(&e),
    };
    if too_fast {
        return err(StatusCode::BAD_REQUEST, "slow_down", "poll the token endpoint less often");
    }
    match d.status.as_str() {
        device_status::PENDING | device_status::AUTHENTICATED => {
            err(StatusCode::BAD_REQUEST, "authorization_pending", "waiting for the user to approve")
        }
        device_status::DENIED => err(StatusCode::BAD_REQUEST, "access_denied", "the user denied access"),
        device_status::ISSUED => invalid_grant("device code already used"),
        device_status::APPROVED => {
            let Some(user) = d.user_name.clone() else { return invalid_grant("device code has no signed-in user") };
            // Single use (RFC 8628): only the first exchange of an approved code gets tokens.
            match s.store.oauth_device_claim_issue(&d.id).await {
                Ok(true) => {}
                Ok(false) => return invalid_grant("device code already used"),
                Err(e) => return internal(&e),
            }
            let grant = match s
                .store
                .oauth_grant_find_or_create(&user, &d.client_id, &d.scope, d.resource.as_deref(), "device")
                .await
            {
                Ok(g) => g,
                Err(e) => return internal(&e),
            };
            if let Err(e) = s
                .store
                .audit(NewAuditEvent::ok(user, "oauth.device_token").target(&grant.id).detail(json!({
                    "client_id": d.client_id
                })))
                .await
            {
                tracing::warn!("could not audit device token issuance: {e}");
            }
            issue_tokens(s, &grant, None).await
        }
        _ => invalid_grant("unknown device code state"),
    }
}

async fn refresh_to_token(s: &AppState, b: &TokenRequest) -> Response {
    let Some(refresh) = b.refresh_token.as_deref() else { return invalid("refresh_token is required") };
    let Some(token) = (match s.store.oauth_token_get(refresh).await {
        Ok(t) => t,
        Err(e) => return internal(&e),
    }) else {
        return invalid_grant("unknown refresh token");
    };
    if token.kind != TokenKind::Refresh {
        return invalid_grant("not a refresh token");
    }
    let now = now_ms();
    if token.revoked_at.is_some() {
        // Theft detection: an old, already-rotated refresh token was replayed.
        return refresh_reuse(s, &token.grant_id, now).await;
    }
    if token.expires_at <= now {
        return invalid_grant("refresh token expired");
    }
    let Some(grant) = (match s.store.oauth_grant_get(&token.grant_id).await {
        Ok(g) => g,
        Err(e) => return internal(&e),
    }) else {
        return invalid_grant("grant no longer exists");
    };
    if grant.revoked_at.is_some() || now - grant.last_interactive_at >= cfg(s).config.absolute_secs * 1000 {
        return invalid_grant("session expired; sign in again");
    }
    if b.resource.as_deref().is_some() && b.resource != grant.resource {
        return invalid_grant("resource mismatch");
    }
    // Rotate: the presented token dies, a fresh one takes its place. The claim is atomic: of two requests
    // presenting the same token at once, one rotates and the other is reuse (a thief racing the client).
    match s.store.oauth_token_claim_rotation(&token.id, now).await {
        Ok(true) => {}
        Ok(false) => return refresh_reuse(s, &token.grant_id, now).await,
        Err(e) => return internal(&e),
    }
    issue_tokens(s, &grant, Some(token.id.clone())).await
}

/// A rotated refresh token was presented again: revoke the whole grant and audit it as theft.
async fn refresh_reuse(s: &AppState, grant_id: &str, now: Millis) -> Response {
    if let Err(e) = s.store.oauth_grant_revoke(grant_id, now).await {
        return internal(&e);
    }
    let who =
        s.store.oauth_grant_get(grant_id).await.ok().flatten().map(|g| g.user_name).unwrap_or_else(|| "unknown".into());
    if let Err(e) = s.store.audit(NewAuditEvent::denied(who, "oauth.refresh_reuse").target(grant_id)).await {
        tracing::warn!("could not audit refresh reuse: {e}");
    }
    invalid_grant("refresh token reuse detected; grant revoked")
}

async fn issue_tokens(s: &AppState, grant: &OAuthGrant, rotated_from: Option<String>) -> Response {
    let c = &cfg(s).config;
    let now = now_ms();
    let access_secret = match generate_secret(ACCESS_PREFIX) {
        Ok(v) => v,
        Err(e) => return internal(&e),
    };
    let refresh_secret = match generate_secret(REFRESH_PREFIX) {
        Ok(v) => v,
        Err(e) => return internal(&e),
    };
    let access = OAuthToken {
        id: new_id(),
        secret: access_secret.clone(),
        grant_id: grant.id.clone(),
        kind: TokenKind::Access,
        prefix: access_secret[..12].into(),
        scope: grant.scope.clone(),
        resource: grant.resource.clone(),
        expires_at: now + c.access_secs * 1000,
        rotated_from: None,
        revoked_at: None,
        last_used_at: None,
        created_at: now,
    };
    let refresh = OAuthToken {
        id: new_id(),
        secret: refresh_secret.clone(),
        grant_id: grant.id.clone(),
        kind: TokenKind::Refresh,
        prefix: refresh_secret[..12].into(),
        scope: grant.scope.clone(),
        resource: grant.resource.clone(),
        expires_at: now + c.refresh_secs * 1000,
        rotated_from,
        revoked_at: None,
        last_used_at: None,
        created_at: now,
    };
    for t in [&access, &refresh] {
        if let Err(e) = s.store.oauth_token_create(t).await {
            return internal(&e);
        }
    }
    Json(json!({
        "access_token": access.secret,
        "token_type": "Bearer",
        "expires_in": c.access_secs,
        "refresh_token": refresh.secret,
        "scope": grant.scope,
    }))
    .into_response()
}

// ------------------------------------------------------------------- revoke

#[derive(Debug, Deserialize)]
struct RevokeRequest {
    token: String,
    #[serde(default)]
    token_type_hint: Option<String>,
}

async fn revoke(State(s): State<AppState>, req: Request) -> Response {
    let Ok(bytes) = to_bytes(req.into_body(), 64 * 1024).await else { return invalid("body too large") };
    let body: RevokeRequest = match serde_urlencoded::from_bytes(&bytes) {
        Ok(v) => v,
        Err(_) => match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => return invalid("expected form-encoded or JSON body"),
        },
    };
    let now = now_ms();
    if let Ok(Some(token)) = s.store.oauth_token_get(&body.token).await {
        if token.kind == TokenKind::Refresh {
            if let Err(e) = s.store.oauth_grant_revoke(&token.grant_id, now).await {
                return internal(&e);
            }
        } else if let Err(e) = s.store.oauth_token_revoke(&token.id, now).await {
            return internal(&e);
        }
        let who = s
            .store
            .oauth_grant_get(&token.grant_id)
            .await
            .ok()
            .flatten()
            .map(|g| g.user_name)
            .unwrap_or_else(|| "unknown".into());
        let detail = json!({ "kind": token.kind.as_str(), "hint": body.token_type_hint });
        if let Err(e) =
            s.store.audit(NewAuditEvent::ok(who, "oauth.revoke").target(&token.grant_id).detail(detail)).await
        {
            tracing::warn!("could not audit revocation: {e}");
        }
    }
    // RFC 7009: always 200, even for unknown tokens.
    StatusCode::OK.into_response()
}

// ------------------------------------------------------------------ sessions

async fn my_sessions(State(s): State<AppState>, Extension(id): Extension<Identity>) -> Response {
    match user_of(&id) {
        Ok(u) => json_sessions(&s, Some(u)).await,
        Err(e) => ApiError(e).into_response(),
    }
}

async fn admin_sessions(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    Query(q): Query<KeysQ>,
) -> Response {
    if !id.admin {
        return ApiError(Error::Forbidden("admin only".into())).into_response();
    }
    json_sessions(&s, q.user.as_deref()).await
}

async fn json_sessions(s: &AppState, user: Option<&str>) -> Response {
    match s.store.oauth_sessions_list(user).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => ApiError(e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct KeysQ {
    user: Option<String>,
}

async fn my_session_revoke(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    axum::extract::Path(sid): axum::extract::Path<String>,
) -> Response {
    match user_of(&id) {
        Ok(u) => revoke_session(&s, &sid, Some(u)).await,
        Err(e) => ApiError(e).into_response(),
    }
}

async fn admin_session_revoke(
    State(s): State<AppState>,
    Extension(id): Extension<Identity>,
    axum::extract::Path(sid): axum::extract::Path<String>,
) -> Response {
    if !id.admin {
        return ApiError(Error::Forbidden("admin only".into())).into_response();
    }
    revoke_session(&s, &sid, None).await
}

async fn revoke_session(s: &AppState, id: &str, user: Option<&str>) -> Response {
    let actor = user.unwrap_or("root").to_owned();
    match s.store.oauth_session_revoke(id, user, now_ms()).await {
        Ok(n) => {
            if let Err(e) = s
                .store
                .audit(NewAuditEvent::ok(actor, "oauth.session_revoke").target(id).detail(json!({"tokens": n})))
                .await
            {
                tracing::warn!("could not audit session revocation: {e}");
            }
            Json(json!({ "revoked": n })).into_response()
        }
        Err(e) => ApiError(e).into_response(),
    }
}

fn user_of(id: &Identity) -> Result<&str, Error> {
    id.user
        .as_deref()
        .ok_or_else(|| Error::Invalid("this credential isn't tied to a user (shared token / open server)".into()))
}

// ----------------------------------------------------------------- helpers

fn cookie_value(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k == CSRF_COOKIE).then(|| v.to_owned())
    })
}

async fn check_csrf(s: &AppState, headers: &HeaderMap, csrf: &str) -> Option<OAuthTransaction> {
    if cookie_value(headers).as_deref() != Some(csrf) {
        return None;
    }
    if let Some(txn) = s.store.oauth_txn_get(csrf).await.ok().flatten()
        && now_ms() - txn.created_at <= cfg(s).config.txn_secs * 1000
    {
        return Some(txn);
    }
    // Device sign-ins have no authorization transaction; their id is the CSRF.
    let d = s.store.oauth_device_get_by_csrf(csrf).await.ok().flatten()?;
    (now_ms() - d.created_at <= cfg(s).config.device_secs * 1000 && d.expires_at > now_ms()).then(|| device_txn(&d))
}

fn cookie_header(s: &AppState, csrf: &str, clear: bool, secs: i64) -> String {
    let mut v =
        format!("{CSRF_COOKIE}={csrf}; Path=/oauth; HttpOnly; SameSite=Lax; Max-Age={}", if clear { 0 } else { secs });
    if cfg(s).config.is_secure() {
        v.push_str("; Secure");
    }
    v
}

fn valid_scope(scope: &str) -> bool {
    !scope.is_empty() && scope.split_whitespace().all(|s| SUPPORTED_SCOPES.contains(&s))
}

fn pkce_ok(verifier: &str, challenge: &str) -> bool {
    if verifier.len() < 43 || verifier.len() > 128 {
        return false;
    }
    if !verifier.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b)) {
        return false;
    }
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) == challenge
}

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn with_params(uri: &str, pairs: &[(&str, &str)]) -> String {
    let sep = if uri.contains('?') { "&" } else { "?" };
    let query = pairs.iter().map(|(k, v)| format!("{}={}", pct(k), pct(v))).collect::<Vec<_>>().join("&");
    format!("{uri}{sep}{query}")
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn html_page(title: &str, body: String) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" \
         content=\"width=device-width,initial-scale=1\"><title>{title}</title><style>\
         body{{font:16px system-ui,sans-serif;max-width:32rem;margin:4rem auto;padding:0 1rem;color:#222}}\
         input{{width:100%;box-sizing:border-box;padding:.6rem;margin:.3rem 0 1rem}}\
         button{{padding:.6rem 1rem;margin-right:.5rem;cursor:pointer}}\
         a.provider{{display:inline-block;padding:.6rem 1rem;margin:.2rem .5rem .2rem 0;border:1px solid #888;border-radius:.3rem;text-decoration:none;color:#222}}\
         .alt{{opacity:.55}}.err{{color:#b00020}}</style></head><body>{body}</body></html>"
    )
}

fn signin_html(txn: &OAuthTransaction, client_name: &str, error: Option<&str>, providers: &UpstreamConfig) -> String {
    let error = error.map(|e| format!("<p class=\"err\">{}</p>", esc(e))).unwrap_or_default();
    let mut buttons = String::new();
    if providers.github.enabled() {
        buttons.push_str(&format!(
            "<a class=\"provider\" href=\"/oauth/signin/github?csrf={}\">Continue with GitHub</a>",
            pct(&txn.csrf)
        ));
    }
    if providers.google.enabled() {
        buttons.push_str(&format!(
            "<a class=\"provider\" href=\"/oauth/signin/google?csrf={}\">Continue with Google</a>",
            pct(&txn.csrf)
        ));
    }
    let alt = if buttons.is_empty() { String::new() } else { format!("<hr><p class=\"alt\">Or:</p><p>{buttons}</p>") };
    html_page(
        "Sign in · memdusa",
        format!(
            "<h1>Sign in to memdusa</h1><p><strong>{}</strong> is requesting access.</p>{}\
             <form method=\"post\" action=\"/oauth/signin\">\
             <input type=\"hidden\" name=\"csrf\" value=\"{}\">\
             <label for=\"api_key\">API key</label>\
             <input id=\"api_key\" type=\"password\" name=\"api_key\" autocomplete=\"off\" required>\
             <button type=\"submit\">Sign in</button></form>{}",
            esc(client_name),
            error,
            esc(&txn.csrf),
            alt
        ),
    )
}

fn consent_html(
    user: &str,
    client_name: &str,
    redirect_uri: &str,
    scope: &str,
    csrf: &str,
    providers: &UpstreamConfig,
) -> String {
    let host = Url::parse(redirect_uri)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| redirect_uri.into());
    let mut links = String::new();
    if providers.github.enabled() {
        links.push_str(&format!(
            "<a class=\"alt\" href=\"/oauth/signin/github?csrf={}\">Link GitHub to {}</a> ",
            pct(csrf),
            esc(user)
        ));
    }
    if providers.google.enabled() {
        links.push_str(&format!(
            "<a class=\"alt\" href=\"/oauth/signin/google?csrf={}\">Link Google to {}</a>",
            pct(csrf),
            esc(user)
        ));
    }
    let links = if links.is_empty() { String::new() } else { format!("<hr><p>{links}</p>") };
    html_page(
        "Authorize · memdusa",
        format!(
            "<h1>Authorize {}</h1><p>Signed in as <strong>{}</strong>.</p>\
             <ul><li>Client: {}</li><li>Redirects to: {}</li><li>Scopes: {}</li></ul>\
             <form method=\"post\" action=\"/oauth/consent\">\
             <input type=\"hidden\" name=\"csrf\" value=\"{}\">\
             <button type=\"submit\" name=\"decision\" value=\"allow\">Allow</button>\
             <button type=\"submit\" name=\"decision\" value=\"deny\">Deny</button></form>{}",
            esc(client_name),
            esc(user),
            esc(client_name),
            esc(&host),
            esc(scope),
            esc(csrf),
            links
        ),
    )
}

fn not_linked_html(provider: &str, display: &str) -> Response {
    Html(html_page(
        "Not linked · memdusa",
        format!(
            "<h1>Not linked to a memdusa user</h1>\
             <p>Your <strong>{}</strong> identity (<code>{}</code>) is not linked to a memdusa user.</p>\
             <p>Ask your administrator to run <code>memdusa user link &lt;user&gt; {}:&lt;handle&gt;</code>, \
             or sign in with your API key and link it from the consent page.</p>",
            esc(provider),
            esc(display),
            esc(provider)
        ),
    ))
    .into_response()
}

fn info_page(title: &str, message: &str) -> Response {
    Html(html_page(title, format!("<h1>{title}</h1><p>{}</p>", esc(message)))).into_response()
}

fn err(status: StatusCode, code: &str, desc: &str) -> Response {
    (status, Json(json!({ "error": code, "error_description": desc }))).into_response()
}

fn invalid(desc: &str) -> Response {
    err(StatusCode::BAD_REQUEST, "invalid_request", desc)
}

fn invalid_grant(desc: &str) -> Response {
    err(StatusCode::BAD_REQUEST, "invalid_grant", desc)
}

fn internal(e: &impl std::fmt::Display) -> Response {
    tracing::error!("oauth: database error: {e}");
    err(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "internal error")
}

#[cfg(test)]
mod tests {
    use memdusa_core::Store;
    use memdusa_core::store::sqlite_url;
    use serde_json::Value;

    use super::*;
    use crate::http::HttpOptions;

    fn config() -> OAuthConfig {
        OAuthConfig {
            issuer: "http://127.0.0.1:1".into(),
            access_secs: 3600,
            refresh_secs: 30 * 24 * 3600,
            absolute_secs: 365 * 24 * 3600,
            code_secs: 600,
            txn_secs: 600,
            device_secs: DEVICE_SECS,
            device_interval_secs: DEVICE_INTERVAL_SECS,
            upstream: UpstreamConfig::default(),
        }
    }

    async fn spawn(cfg: OAuthConfig, default_access: DefaultPolicy) -> (String, Store) {
        let dir = std::env::temp_dir().join(format!("memdusa-oauth-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::connect(&sqlite_url(&dir.join("t.db"))).await.unwrap();
        let opts = HttpOptions {
            addr: String::new(),
            token: None,
            allowed_hosts: vec![],
            auto_prune: None,
            rate_limit: 0.0,
            metrics: false,
            default_access,
            oauth: Some(cfg),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::http::app(store.clone(), opts);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, store)
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap()
    }

    fn hidden(html: &str, name: &str) -> String {
        let marker = format!("name=\"{name}\" value=\"");
        let rest = html.split_once(&marker).unwrap_or_else(|| panic!("no hidden field {name}: {html}")).1;
        rest.split_once('"').unwrap().0.to_owned()
    }

    fn param(location: &str, key: &str) -> String {
        let query = location.split_once('?').expect("redirect has no query").1;
        query
            .split('&')
            .filter_map(|p| p.split_once('='))
            .find(|(k, _)| *k == key)
            .unwrap_or_else(|| panic!("no {key} in {location}"))
            .1
            .to_owned()
    }

    fn pkce() -> (String, String) {
        let verifier = "p".repeat(43);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        (verifier, challenge)
    }

    struct Flow {
        client_id: String,
        redirect_uri: String,
        verifier: String,
        code: String,
        state: String,
    }

    impl Flow {
        async fn token(&self, url: &str) -> (u16, Value) {
            let r = client()
                .post(format!("{url}/oauth/token"))
                .form(&[
                    ("grant_type", "authorization_code"),
                    ("code", self.code.as_str()),
                    ("redirect_uri", self.redirect_uri.as_str()),
                    ("client_id", self.client_id.as_str()),
                    ("code_verifier", self.verifier.as_str()),
                ])
                .send()
                .await
                .unwrap();
            let status = r.status().as_u16();
            (status, r.json().await.unwrap_or_default())
        }
    }

    async fn register(url: &str, redirect_uri: &str) -> String {
        let r = client()
            .post(format!("{url}/oauth/register"))
            .json(&json!({
                "client_name": "Test Harness",
                "redirect_uris": [redirect_uri],
                "token_endpoint_auth_method": "none",
            }))
            .send()
            .await
            .unwrap();
        let status = r.status();
        let body: Value = r.json().await.unwrap_or_default();
        assert_eq!(status, 200, "{body}");
        body["client_id"].as_str().unwrap().to_owned()
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_flow(url: &str, api_key: &str, scope: &str, resource: Option<&str>, redirect_uri: &str) -> Flow {
        let client_id = register(url, redirect_uri).await;
        let (verifier, challenge) = pkce();
        let mut q = vec![
            ("response_type", "code"),
            ("client_id", client_id.as_str()),
            ("redirect_uri", redirect_uri),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", "state-1"),
            ("scope", scope),
        ];
        if let Some(r) = resource {
            q.push(("resource", r));
        }
        let r = client().get(format!("{url}/oauth/authorize")).query(&q).send().await.unwrap();
        let status = r.status();
        let cookie =
            r.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().split(';').next().unwrap().to_owned();
        let html = r.text().await.unwrap();
        assert_eq!(status, 200, "{html}");
        let csrf = hidden(&html, "csrf");

        let r = client()
            .post(format!("{url}/oauth/signin"))
            .header(header::COOKIE, &cookie)
            .form(&[("csrf", csrf.as_str()), ("api_key", api_key)])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
        let html = r.text().await.unwrap();
        assert!(html.contains("Test Harness") && !html.contains("state-1"));
        assert!(html.contains("Authorize") && html.contains("Allow"));

        let r = client()
            .post(format!("{url}/oauth/consent"))
            .header(header::COOKIE, &cookie)
            .form(&[("csrf", csrf.as_str()), ("decision", "allow")])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 302, "{}", r.text().await.unwrap_or_default());
        let location = r.headers().get(header::LOCATION).unwrap().to_str().unwrap().to_owned();
        Flow {
            client_id,
            redirect_uri: redirect_uri.to_owned(),
            verifier,
            code: param(&location, "code"),
            state: param(&location, "state"),
        }
    }

    async fn call_status(url: &str, method: reqwest::Method, path: &str, body: Option<Value>, key: &str) -> u16 {
        let mut r = client().request(method, format!("{url}{path}")).bearer_auth(key);
        if let Some(b) = body {
            r = r.json(&b);
        }
        r.send().await.unwrap().status().as_u16()
    }

    async fn new_user(store: &Store, name: &str, admin: bool) -> String {
        store.user_create(name, admin).await.unwrap();
        store.key_create(name, "signin").await.unwrap().secret
    }

    // ------------------------------------------------------------------ tests

    /// Security review: two refreshes racing with the same token can't both rotate; the loser is reuse.
    #[tokio::test]
    async fn concurrent_refreshes_with_one_token_trip_reuse_detection() {
        let (url, store) = spawn(config(), DefaultPolicy::Write).await;
        let key = new_user(&store, "alice", false).await;
        let flow =
            start_flow(&url, &key, "memdusa", Some("http://127.0.0.1:1/mcp"), "http://127.0.0.1:54321/callback").await;
        let (st, tokens) = flow.token(&url).await;
        assert_eq!(st, 200, "{tokens}");
        let refresh = tokens["refresh_token"].as_str().unwrap().to_owned();
        let one = || {
            let (url, refresh) = (url.clone(), refresh.clone());
            async move {
                client()
                    .post(format!("{url}/oauth/token"))
                    .form(&[("grant_type", "refresh_token"), ("refresh_token", refresh.as_str())])
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            }
        };
        let results = futures::future::join_all((0..8).map(|_| tokio::spawn(one()))).await;
        let ok = results.iter().filter(|r| *r.as_ref().unwrap() == 200).count();
        assert!(ok <= 1, "at most one concurrent refresh may rotate, got {ok}");
        let audit = store.audit_query(AuditQuery { limit: Some(100), ..Default::default() }).await.unwrap();
        assert!(audit.iter().any(|e| e.action == "oauth.refresh_reuse"), "the race is reported as reuse");
    }

    #[tokio::test]
    async fn full_flow_refresh_reuse_revoke_and_mcp() {
        let (url, store) = spawn(config(), DefaultPolicy::Write).await;
        let key = new_user(&store, "alice", false).await;
        let flow =
            start_flow(&url, &key, "memdusa", Some("http://127.0.0.1:1/mcp"), "http://127.0.0.1:54321/callback").await;
        assert_eq!(flow.state, "state-1");

        let (st, tokens) = flow.token(&url).await;
        assert_eq!(st, 200, "{tokens}");
        let access = tokens["access_token"].as_str().unwrap().to_owned();
        let refresh = tokens["refresh_token"].as_str().unwrap().to_owned();
        assert!(access.starts_with("mdo_") && refresh.starts_with("mdr_"));
        assert_eq!(tokens["scope"], json!("memdusa"));

        let me = client().get(format!("{url}/v1/whoami")).bearer_auth(&access).send().await.unwrap();
        assert_eq!(me.status(), 200);
        let me: Value = me.json().await.unwrap();
        assert_eq!((me["user"].as_str(), me["admin"].as_bool()), (Some("alice"), Some(false)));

        // MCP accepts the same identity.
        let mcp = client()
            .post(format!("{url}/mcp"))
            .bearer_auth(&access)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}))
            .send()
            .await
            .unwrap();
        assert_ne!(mcp.status(), 401, "access token must work on /mcp");
        assert_ne!(mcp.status(), 403);

        // Sessions show up for the user and for admins.
        let sessions = store.oauth_sessions_list(Some("alice")).await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].client_name, "Test Harness");

        // Refresh rotates; replaying the old token kills the new one too.
        let r = client()
            .post(format!("{url}/oauth/token"))
            .form(&[("grant_type", "refresh_token"), ("refresh_token", refresh.as_str())])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let fresh: Value = r.json().await.unwrap();
        let access2 = fresh["access_token"].as_str().unwrap().to_owned();
        assert_eq!(client().get(format!("{url}/v1/whoami")).bearer_auth(&access2).send().await.unwrap().status(), 200);

        let replay = client()
            .post(format!("{url}/oauth/token"))
            .form(&[("grant_type", "refresh_token"), ("refresh_token", refresh.as_str())])
            .send()
            .await
            .unwrap();
        assert_eq!(replay.status(), 400, "reuse is rejected");
        assert_eq!(replay.json::<Value>().await.unwrap()["error"], "invalid_grant");
        assert_eq!(
            client().get(format!("{url}/v1/whoami")).bearer_auth(&access2).send().await.unwrap().status(),
            401,
            "the whole grant chain was revoked"
        );

        // A fresh sign-in can be revoked through RFC 7009.
        let flow2 = start_flow(&url, &key, "memdusa", None, "http://localhost:9876/cb").await;
        let (st, tokens) = flow2.token(&url).await;
        assert_eq!(st, 200, "{tokens}");
        let access3 = tokens["access_token"].as_str().unwrap().to_owned();
        let revoke = client()
            .post(format!("{url}/oauth/revoke"))
            .form(&[("token", access3.as_str()), ("token_type_hint", "access_token")])
            .send()
            .await
            .unwrap();
        assert_eq!(revoke.status(), 200);
        assert_eq!(client().get(format!("{url}/v1/whoami")).bearer_auth(&access3).send().await.unwrap().status(), 401);

        let audit = store.audit_query(AuditQuery { limit: Some(100), ..Default::default() }).await.unwrap();
        for action in ["oauth.register", "oauth.signin", "oauth.refresh_reuse", "oauth.revoke"] {
            assert!(audit.iter().any(|e| e.action == action), "missing audit event {action}");
        }
    }

    #[tokio::test]
    async fn full_flow_over_postgres() {
        let Ok(url) = std::env::var("MEMDUSA_TEST_POSTGRES_URL") else { return };
        let dir = std::env::temp_dir().join(format!("memdusa-oauth-pg-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::connect(&url).await.unwrap();
        let opts = HttpOptions {
            addr: String::new(),
            token: None,
            allowed_hosts: vec![],
            auto_prune: None,
            rate_limit: 0.0,
            metrics: false,
            default_access: DefaultPolicy::Write,
            oauth: Some(config()),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::http::app(store.clone(), opts);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // Unique user name: Postgres tests share one database.
        let user = format!("pg{}", new_id()).replace('-', "");
        store.user_create(&user, false).await.unwrap();
        let key = store.key_create(&user, "signin").await.unwrap().secret;
        let flow =
            start_flow(&base, &key, "memdusa", Some("http://127.0.0.1:1/mcp"), "http://127.0.0.1:43210/cb").await;
        let (st, tokens) = flow.token(&base).await;
        assert_eq!(st, 200, "{tokens}");
        let access = tokens["access_token"].as_str().unwrap();
        let me = client().get(format!("{base}/v1/whoami")).bearer_auth(access).send().await.unwrap();
        assert_eq!(me.status(), 200);
        assert_eq!(me.json::<Value>().await.unwrap()["user"], json!(user));
    }

    #[tokio::test]
    async fn pkce_verifier_and_single_use_code() {
        let (url, store) = spawn(config(), DefaultPolicy::Write).await;
        let key = new_user(&store, "bob", false).await;
        let flow = start_flow(&url, &key, "memdusa", None, "http://127.0.0.1:4242/cb").await;

        let bad = client()
            .post(format!("{url}/oauth/token"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", flow.code.as_str()),
                ("redirect_uri", flow.redirect_uri.as_str()),
                ("client_id", flow.client_id.as_str()),
                ("code_verifier", "wrong-verifier-that-is-long-enough-to-be-valid-000"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);
        assert_eq!(bad.json::<Value>().await.unwrap()["error"], "invalid_grant");

        assert_eq!(flow.token(&url).await.0, 200, "code still usable after failed PKCE");
        let replay = flow.token(&url).await;
        assert_eq!(replay.0, 400);
        assert_eq!(replay.1["error"], "invalid_grant", "code is single-use");
    }

    #[tokio::test]
    async fn redirect_and_pkce_validation() {
        let (url, store) = spawn(config(), DefaultPolicy::Write).await;
        new_user(&store, "carol", false).await;
        let (_, challenge) = pkce();
        let client_id = register(&url, "http://127.0.0.1:5555/cb").await;
        let base = vec![
            ("response_type", "code"),
            ("client_id", client_id.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", "s"),
            ("scope", "memdusa"),
        ];

        let mut wrong = base.clone();
        wrong.push(("redirect_uri", "http://127.0.0.1:6666/cb"));
        assert_eq!(client().get(format!("{url}/oauth/authorize")).query(&wrong).send().await.unwrap().status(), 400);

        let mut plain = base.clone();
        plain.push(("redirect_uri", "http://127.0.0.1:5555/cb"));
        let plain: Vec<(&str, &str)> = plain.iter().map(|(k, v)| (*k, *v)).collect();
        let plain = plain
            .into_iter()
            .map(|(k, v)| if k == "code_challenge_method" { (k, "plain") } else { (k, v) })
            .collect::<Vec<_>>();
        assert_eq!(client().get(format!("{url}/oauth/authorize")).query(&plain).send().await.unwrap().status(), 400);

        // Non-loopback http redirect URIs are refused at registration.
        let bad = client()
            .post(format!("{url}/oauth/register"))
            .json(&json!({"redirect_uris": ["http://evil.example/cb"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);

        let flow = start_flow(
            &url,
            &store.key_create("carol", "k").await.unwrap().secret,
            "memdusa",
            None,
            "http://127.0.0.1:5555/cb",
        )
        .await;
        let mismatch = client()
            .post(format!("{url}/oauth/token"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", flow.code.as_str()),
                ("redirect_uri", "http://127.0.0.1:9999/cb"),
                ("client_id", flow.client_id.as_str()),
                ("code_verifier", flow.verifier.as_str()),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(mismatch.status(), 400);
    }

    /// Call one MCP tool over HTTP with `token`; the JSON-RPC response (SSE `data:` or plain JSON).
    async fn mcp_tool(url: &str, token: &str, tool: &str, args: Value) -> Value {
        let post = |body: Value, session: Option<String>| {
            let mut r = client()
                .post(format!("{url}/mcp"))
                .bearer_auth(token)
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .json(&body);
            if let Some(s) = session {
                r = r.header("mcp-session-id", s);
            }
            r.send()
        };
        let init = post(
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
            None,
        )
        .await
        .unwrap();
        let session = init.headers().get("mcp-session-id").map(|v| v.to_str().unwrap().to_owned());
        let _ = init.text().await;
        let _ = post(json!({"jsonrpc":"2.0","method":"notifications/initialized"}), session.clone()).await.unwrap();
        let r = post(
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool,"arguments":args}}),
            session,
        )
        .await
        .unwrap();
        let text = r.text().await.unwrap();
        let body = text
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim)
            .find(|l| l.starts_with('{'))
            .unwrap_or(text.trim())
            .to_owned();
        serde_json::from_str(&body).unwrap_or_else(|_| panic!("not JSON-RPC: {text}"))
    }

    /// Security review: a read-only token must not write global memories over MCP either.
    #[tokio::test]
    async fn read_only_token_cannot_write_global_memories_over_mcp() {
        let (url, store) = spawn(config(), DefaultPolicy::Write).await;
        let key = new_user(&store, "erin", false).await;
        let flow = start_flow(&url, &key, "memdusa:read", None, "http://127.0.0.1:7779/cb").await;
        let (_, tokens) = flow.token(&url).await;
        let read_only = tokens["access_token"].as_str().unwrap().to_owned();
        let r = mcp_tool(&url, &read_only, "memory_save", json!({"content":"sneaky global","global":true})).await;
        let failed = r.get("error").is_some() || r["result"]["isError"].as_bool() == Some(true);
        assert!(failed, "read-only global save must fail: {r}");
        assert!(r.to_string().contains("read-only"), "{r}");

        let flow = start_flow(&url, &key, "memdusa", None, "http://127.0.0.1:7780/cb").await;
        let (_, tokens) = flow.token(&url).await;
        let full = tokens["access_token"].as_str().unwrap().to_owned();
        let r = mcp_tool(&url, &full, "memory_save", json!({"content":"allowed global","global":true})).await;
        assert!(r.get("error").is_none() && r["result"]["isError"].as_bool() != Some(true), "{r}");
    }

    #[tokio::test]
    async fn read_only_scope_and_h1_grants() {
        let (url, store) = spawn(config(), DefaultPolicy::None).await;
        store.user_create("dave", false).await.unwrap();
        store.grant_create("proj/a", "dave", Role::Read, "root").await.unwrap();
        let key = store.key_create("dave", "k").await.unwrap().secret;

        let flow = start_flow(&url, &key, "memdusa:read", None, "http://127.0.0.1:7777/cb").await;
        let (st, tokens) = flow.token(&url).await;
        assert_eq!(st, 200, "{tokens}");
        let access = tokens["access_token"].as_str().unwrap();

        assert_eq!(call_status(&url, reqwest::Method::GET, "/v1/resume?project=proj/a", None, access).await, 200);
        assert_eq!(
            call_status(
                &url,
                reqwest::Method::POST,
                "/v1/memories",
                Some(json!({"project":"proj/a","content":"x"})),
                access
            )
            .await,
            403,
            "memdusa:read denies writes even with a write grant"
        );
        assert_eq!(
            call_status(&url, reqwest::Method::GET, "/v1/resume?project=proj/b", None, access).await,
            403,
            "H1 grants still apply"
        );

        // Full-scope token: H1 still decides project access.
        let flow = start_flow(&url, &key, "memdusa", None, "http://127.0.0.1:7778/cb").await;
        let (_, tokens) = flow.token(&url).await;
        let full = tokens["access_token"].as_str().unwrap();
        assert_eq!(call_status(&url, reqwest::Method::GET, "/v1/resume?project=proj/a", None, full).await, 200);
        assert_eq!(
            call_status(
                &url,
                reqwest::Method::POST,
                "/v1/memories",
                Some(json!({"project":"proj/a","content":"x"})),
                full
            )
            .await,
            403,
            "read grant denies writes"
        );
    }

    #[tokio::test]
    async fn expired_code_and_access_token() {
        let mut short = config();
        short.code_secs = 0;
        let (url, store) = spawn(short, DefaultPolicy::Write).await;
        let key = new_user(&store, "eve", false).await;
        let flow = start_flow(&url, &key, "memdusa", None, "http://127.0.0.1:8888/cb").await;
        let (st, body) = flow.token(&url).await;
        assert_eq!(st, 400);
        assert_eq!(body["error"], "invalid_grant", "expired code");

        let mut short = config();
        short.access_secs = 0;
        let (url2, store2) = spawn(short, DefaultPolicy::Write).await;
        let key2 = new_user(&store2, "frank", false).await;
        let flow2 = start_flow(&url2, &key2, "memdusa", None, "http://127.0.0.1:8889/cb").await;
        let (st, tokens) = flow2.token(&url2).await;
        assert_eq!(st, 200, "{tokens}");
        let access = tokens["access_token"].as_str().unwrap();
        assert_eq!(client().get(format!("{url2}/v1/whoami")).bearer_auth(access).send().await.unwrap().status(), 401);
    }

    #[tokio::test]
    async fn metadata_and_www_authenticate() {
        let (url, store) = spawn(config(), DefaultPolicy::Write).await;
        // Creating a user latches auth on, so unauthenticated calls are 401s.
        new_user(&store, "ian", false).await;
        let protected: Value = client()
            .get(format!("{url}/.well-known/oauth-protected-resource"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(protected["authorization_servers"][0], json!("http://127.0.0.1:1"));
        assert_eq!(protected["resource"], json!("http://127.0.0.1:1/mcp"));
        let as_doc: Value = client()
            .get(format!("{url}/.well-known/oauth-authorization-server"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(as_doc["code_challenge_methods_supported"], json!(["S256"]));
        assert_eq!(as_doc["token_endpoint_auth_methods_supported"], json!(["none"]));
        assert!(as_doc["grant_types_supported"].as_array().unwrap().iter().any(|g| g == DEVICE_GRANT));
        assert_eq!(as_doc["device_authorization_endpoint"], json!("http://127.0.0.1:1/oauth/device_authorization"));

        for path in ["/v1/whoami", "/mcp"] {
            let r = client().get(format!("{url}{path}")).send().await.unwrap();
            assert_eq!(r.status(), 401);
            assert_eq!(
                r.headers().get(header::WWW_AUTHENTICATE).unwrap().to_str().unwrap(),
                "Bearer resource_metadata=\"http://127.0.0.1:1/.well-known/oauth-protected-resource\", scope=\"memdusa\""
            );
        }
    }

    #[tokio::test]
    async fn csrf_and_session_api() {
        let (url, store) = spawn(config(), DefaultPolicy::Write).await;
        let key = new_user(&store, "grace", false).await;
        let client_id = register(&url, "http://127.0.0.1:9991/cb").await;
        let (_, challenge) = pkce();
        let r = client()
            .get(format!("{url}/oauth/authorize"))
            .query(&[
                ("response_type", "code"),
                ("client_id", client_id.as_str()),
                ("redirect_uri", "http://127.0.0.1:9991/cb"),
                ("code_challenge", challenge.as_str()),
                ("code_challenge_method", "S256"),
                ("state", "s"),
            ])
            .send()
            .await
            .unwrap();
        let cookie =
            r.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().split(';').next().unwrap().to_owned();
        let html = r.text().await.unwrap();
        let csrf = hidden(&html, "csrf");

        // No cookie: sign-in is refused.
        assert_eq!(
            client()
                .post(format!("{url}/oauth/signin"))
                .form(&[("csrf", csrf.as_str()), ("api_key", key.as_str())])
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        // Wrong csrf with a valid cookie is refused too.
        assert_eq!(
            client()
                .post(format!("{url}/oauth/signin"))
                .header(header::COOKIE, &cookie)
                .form(&[("csrf", "wrong"), ("api_key", key.as_str())])
                .send()
                .await
                .unwrap()
                .status(),
            400
        );

        let r = client()
            .post(format!("{url}/oauth/signin"))
            .header(header::COOKIE, &cookie)
            .form(&[("csrf", csrf.as_str()), ("api_key", key.as_str())])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        // Consent needs the same csrf.
        assert_eq!(
            client()
                .post(format!("{url}/oauth/consent"))
                .header(header::COOKIE, &cookie)
                .form(&[("csrf", "bad"), ("decision", "allow")])
                .send()
                .await
                .unwrap()
                .status(),
            400
        );

        let flow = start_flow(&url, &key, "memdusa", None, "http://127.0.0.1:9992/cb").await;
        let (_, tokens) = flow.token(&url).await;
        let access = tokens["access_token"].as_str().unwrap();

        let mine = client().get(format!("{url}/v1/me/sessions")).bearer_auth(access).send().await.unwrap();
        let mine_status = mine.status();
        let mine_text = mine.text().await.unwrap_or_default();
        assert_eq!(mine_status, 200, "{mine_text}");
        let mine_body: Value = serde_json::from_str(&mine_text).unwrap_or_default();
        assert_eq!(mine_body.as_array().unwrap().len(), 1);

        let sid = store.oauth_sessions_list(Some("grace")).await.unwrap()[0].id.clone();
        let revoked = client().delete(format!("{url}/v1/me/sessions/{sid}")).bearer_auth(access).send().await.unwrap();
        assert_eq!(revoked.status(), 200);
        assert_eq!(client().get(format!("{url}/v1/whoami")).bearer_auth(access).send().await.unwrap().status(), 401);
    }

    #[tokio::test]
    async fn failed_signins_are_rate_limited() {
        let (url, store) = spawn(config(), DefaultPolicy::Write).await;
        new_user(&store, "heidi", false).await;
        let client_id = register(&url, "http://127.0.0.1:9993/cb").await;
        let (_, challenge) = pkce();
        for i in 0..SIGNIN_FAIL_LIMIT + 2 {
            let r = client()
                .get(format!("{url}/oauth/authorize"))
                .query(&[
                    ("response_type", "code"),
                    ("client_id", client_id.as_str()),
                    ("redirect_uri", "http://127.0.0.1:9993/cb"),
                    ("code_challenge", challenge.as_str()),
                    ("code_challenge_method", "S256"),
                    ("state", "s"),
                ])
                .send()
                .await
                .unwrap();
            let cookie =
                r.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().split(';').next().unwrap().to_owned();
            let csrf = hidden(&r.text().await.unwrap(), "csrf");
            let st = client()
                .post(format!("{url}/oauth/signin"))
                .header(header::COOKIE, &cookie)
                .form(&[("csrf", csrf.as_str()), ("api_key", "mdk_wrong")])
                .send()
                .await
                .unwrap()
                .status()
                .as_u16();
            if i >= SIGNIN_FAIL_LIMIT {
                assert_eq!(st, 429, "failed sign-in {i} should be throttled");
            } else {
                assert_eq!(st, 401, "failed sign-in {i}");
            }
        }
    }

    async fn device_client(url: &str) -> String {
        let r = client()
            .post(format!("{url}/oauth/register"))
            .json(&json!({
                "client_name": "Device CLI",
                "token_endpoint_auth_method": "none",
                "grant_types": [DEVICE_GRANT],
            }))
            .send()
            .await
            .unwrap();
        let status = r.status();
        let body: Value = r.json().await.unwrap_or_default();
        assert_eq!(status, 200, "{body}");
        body["client_id"].as_str().unwrap().to_owned()
    }

    async fn start_device(url: &str, client_id: &str) -> Value {
        let r = client()
            .post(format!("{url}/oauth/device_authorization"))
            .form(&[("client_id", client_id), ("scope", "memdusa")])
            .send()
            .await
            .unwrap();
        let status = r.status();
        let body: Value = r.json().await.unwrap_or_default();
        assert_eq!(status, 200, "{body}");
        assert!(body["interval"].as_i64().is_some_and(|i| i >= 1));
        body
    }

    async fn poll_device(url: &str, client_id: &str, device_code: &str) -> (u16, Value) {
        let r = client()
            .post(format!("{url}/oauth/token"))
            .form(&[("grant_type", DEVICE_GRANT), ("client_id", client_id), ("device_code", device_code)])
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or_default())
    }

    /// Drive the human side through the same sign-in + consent pages a browser sees.
    async fn approve_device(url: &str, key: &str, user_code: &str, decision: &str) {
        let r = client().get(format!("{url}/oauth/device")).query(&[("user_code", user_code)]).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let cookie =
            r.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().split(';').next().unwrap().to_owned();
        let html = r.text().await.unwrap();
        assert!(html.contains("Sign in to memdusa"));
        let csrf = hidden(&html, "csrf");

        let r = client()
            .post(format!("{url}/oauth/signin"))
            .header(header::COOKIE, &cookie)
            .form(&[("csrf", csrf.as_str()), ("api_key", key)])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
        let html = r.text().await.unwrap();
        assert!(html.contains("Authorize") && html.contains("this device"));
        let csrf = hidden(&html, "csrf");

        let r = client()
            .post(format!("{url}/oauth/consent"))
            .header(header::COOKIE, &cookie)
            .form(&[("csrf", csrf.as_str()), ("decision", decision)])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
    }

    #[tokio::test]
    async fn device_flow_pending_approved_and_token() {
        let mut fast = config();
        fast.device_interval_secs = 1;
        let (url, store) = spawn(fast, DefaultPolicy::Write).await;
        let key = new_user(&store, "device-user", false).await;
        let client_id = device_client(&url).await;
        let d = start_device(&url, &client_id).await;
        let (code, user_code) = (d["device_code"].as_str().unwrap(), d["user_code"].as_str().unwrap());
        assert!(code.starts_with(DEVICE_PREFIX) && user_code.contains('-'));
        assert_eq!(d["verification_uri"], json!("http://127.0.0.1:1/oauth/device"));

        let (st, body) = poll_device(&url, &client_id, code).await;
        assert_eq!(st, 400);
        assert_eq!(body["error"], json!("authorization_pending"));

        approve_device(&url, &key, user_code, "allow").await;
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let (st, body) = poll_device(&url, &client_id, code).await;
        assert_eq!(st, 200, "{body}");
        let access = body["access_token"].as_str().unwrap();
        assert!(
            access.starts_with(ACCESS_PREFIX) && body["refresh_token"].as_str().unwrap().starts_with(REFRESH_PREFIX)
        );
        assert_eq!(client().get(format!("{url}/v1/whoami")).bearer_auth(access).send().await.unwrap().status(), 200);
        // Single use: polling again after the tokens were issued gets nothing new.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let (st, again) = poll_device(&url, &client_id, code).await;
        assert_eq!((st, again["error"].as_str()), (400, Some("invalid_grant")), "{again}");
        let audit = store.audit_query(AuditQuery { limit: Some(100), ..Default::default() }).await.unwrap();
        for action in
            ["oauth.device_authorization", "oauth.device_signin", "oauth.device_approved", "oauth.device_token"]
        {
            assert!(audit.iter().any(|e| e.action == action), "missing audit event {action}");
        }
    }

    #[tokio::test]
    async fn device_flow_denied_expired_and_slow_down() {
        let (url, store) = spawn(config(), DefaultPolicy::Write).await;
        let key = new_user(&store, "device-deny", false).await;
        let client_id = device_client(&url).await;
        let d = start_device(&url, &client_id).await;
        let code = d["device_code"].as_str().unwrap().to_owned();
        approve_device(&url, &key, d["user_code"].as_str().unwrap(), "deny").await;
        let (_, body) = poll_device(&url, &client_id, &code).await;
        assert_eq!(body["error"], json!("access_denied"));

        // Polling twice in quick succession is throttled.
        let d = start_device(&url, &client_id).await;
        let code = d["device_code"].as_str().unwrap().to_owned();
        assert_eq!(poll_device(&url, &client_id, &code).await.1["error"], json!("authorization_pending"));
        assert_eq!(poll_device(&url, &client_id, &code).await.1["error"], json!("slow_down"));

        let mut short = config();
        short.device_secs = 0;
        let (url2, store2) = spawn(short, DefaultPolicy::Write).await;
        let _key2 = new_user(&store2, "device-expired", false).await;
        let client_id2 = device_client(&url2).await;
        let d = start_device(&url2, &client_id2).await;
        let (_, body) = poll_device(&url2, &client_id2, d["device_code"].as_str().unwrap()).await;
        assert_eq!(body["error"], json!("expired_token"));
    }

    #[tokio::test]
    async fn device_flow_over_postgres() {
        let Ok(url) = std::env::var("MEMDUSA_TEST_POSTGRES_URL") else { return };
        let dir = std::env::temp_dir().join(format!("memdusa-device-pg-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::connect(&url).await.unwrap();
        let opts = HttpOptions {
            addr: String::new(),
            token: None,
            allowed_hosts: vec![],
            auto_prune: None,
            rate_limit: 0.0,
            metrics: false,
            default_access: DefaultPolicy::Write,
            oauth: Some(config()),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::http::app(store.clone(), opts);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let user = format!("dp{}", new_id()).replace('-', "");
        store.user_create(&user, false).await.unwrap();
        let key = store.key_create(&user, "signin").await.unwrap().secret;
        let client_id = device_client(&base).await;
        let d = start_device(&base, &client_id).await;
        approve_device(&base, &key, d["user_code"].as_str().unwrap(), "allow").await;
        let (st, body) = poll_device(&base, &client_id, d["device_code"].as_str().unwrap()).await;
        assert_eq!(st, 200, "{body}");
    }

    #[test]
    fn issuer_validation() {
        assert!(validate_issuer("https://memdusa.example.com").is_ok());
        assert!(validate_issuer("http://localhost:8080").is_ok());
        assert!(validate_issuer("http://127.0.0.1:7077").is_ok());
        assert!(validate_issuer("http://example.com").is_err());
        assert!(validate_issuer("https://user:pw@example.com").is_err());
        assert!(validate_issuer("not a url").is_err());
    }

    #[test]
    fn pkce_math() {
        let (v, c) = pkce();
        assert!(pkce_ok(&v, &c));
        assert!(!pkce_ok("too short", &c));
        assert!(!pkce_ok(&v, "plain-challenge"));
        assert!(valid_scope("memdusa memdusa:read"));
        assert!(!valid_scope("memdusa:admin"));
    }

    #[tokio::test]
    async fn registration_bounds() {
        let (url, _) = spawn(config(), DefaultPolicy::Write).await;
        let long_name = "x".repeat(250);
        let r = client()
            .post(format!("{url}/oauth/register"))
            .json(&json!({"client_name": long_name, "redirect_uris": ["http://127.0.0.1:6010/cb"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["client_name"].as_str().unwrap().chars().count(), 200, "client_name is truncated");

        let many: Vec<String> = (0..11).map(|i| format!("http://127.0.0.1:{}/cb", 6100 + i)).collect();
        let r =
            client().post(format!("{url}/oauth/register")).json(&json!({"redirect_uris": many})).send().await.unwrap();
        assert_eq!(r.status(), 400);
        assert_eq!(r.json::<Value>().await.unwrap()["error"], "invalid_request");

        let too_long = format!("https://example.com/{}", "x".repeat(2000));
        let r = client()
            .post(format!("{url}/oauth/register"))
            .json(&json!({"redirect_uris": [too_long]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "overlong redirect_uri is rejected");

        // Exactly 2000 characters is still accepted.
        let exact = format!("https://x.io/{}", "a".repeat(2000 - "https://x.io/".len()));
        assert_eq!(exact.chars().count(), 2000);
        let r = client()
            .post(format!("{url}/oauth/register"))
            .json(&json!({"redirect_uris": [exact]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    #[tokio::test]
    async fn oauth_routes_are_rate_limited() {
        let (url, _) = spawn(config(), DefaultPolicy::Write).await;
        let mut limited = false;
        for i in 0..(OAUTH_RATE_BURST as usize + 100) {
            let r = client()
                .post(format!("{url}/oauth/register"))
                .json(&json!({"redirect_uris": ["http://127.0.0.1:6020/cb"]}))
                .send()
                .await
                .unwrap();
            if r.status() == StatusCode::TOO_MANY_REQUESTS {
                assert!(r.headers().get(header::RETRY_AFTER).is_some(), "429 carries Retry-After");
                let body: Value = r.json().await.unwrap();
                assert_eq!(body["error"], "rate_limited");
                assert!(body["error_description"].as_str().is_some_and(|d| d.contains("retry")));
                limited = true;
                break;
            }
            assert_eq!(r.status(), 200, "request {i} should still be allowed");
        }
        assert!(limited, "expected a 429 after exhausting the per-IP OAuth burst");
    }

    #[tokio::test]
    async fn client_registration_has_a_tighter_limit() {
        let (url, _) = spawn(config(), DefaultPolicy::Write).await;
        let mut statuses = vec![];
        for _ in 0..(OAUTH_CREATE_BURST as usize + 2) {
            let r = client()
                .post(format!("{url}/oauth/register"))
                .json(&json!({"redirect_uris": ["http://127.0.0.1:6040/cb"]}))
                .send()
                .await
                .unwrap();
            statuses.push(r.status().as_u16());
        }
        assert!(statuses[..OAUTH_CREATE_BURST as usize].iter().all(|s| *s == 200), "{statuses:?}");
        assert_eq!(*statuses.last().unwrap(), 429, "{statuses:?}");
        // Other OAuth endpoints still answer.
        let r = client().get(format!("{url}/oauth/device")).send().await.unwrap();
        assert_ne!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    fn assert_security_headers(r: &reqwest::Response) {
        let h = r.headers();
        assert_eq!(h.get("content-security-policy").unwrap().to_str().unwrap(), "frame-ancestors 'none'");
        assert_eq!(h.get("x-frame-options").unwrap().to_str().unwrap(), "DENY");
        assert_eq!(h.get("referrer-policy").unwrap().to_str().unwrap(), "no-referrer");
        assert_eq!(h.get("cache-control").unwrap().to_str().unwrap(), "no-store");
    }

    #[tokio::test]
    async fn html_pages_carry_anti_framing_headers() {
        let (url, _) = spawn(config(), DefaultPolicy::Write).await;
        let client_id = register(&url, "http://127.0.0.1:6030/cb").await;
        let (_, challenge) = pkce();
        let r = client()
            .get(format!("{url}/oauth/authorize"))
            .query(&[
                ("response_type", "code"),
                ("client_id", client_id.as_str()),
                ("redirect_uri", "http://127.0.0.1:6030/cb"),
                ("code_challenge", challenge.as_str()),
                ("code_challenge_method", "S256"),
                ("state", "s"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_security_headers(&r);

        let r = client().get(format!("{url}/oauth/device")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_security_headers(&r);
    }
}
