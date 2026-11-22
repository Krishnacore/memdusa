//! Mock-upstream integration tests for GitHub/Google sign-in (H2b). No real
//! network: an in-process axum app implements the provider endpoints, and the
//! server's upstream URLs are pointed at it through test-only config fields.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use memdusa_core::Store;
use memdusa_core::auth::generate_secret;
use memdusa_core::model::*;
use memdusa_core::store::sqlite_url;
use serde::Serialize;
use serde_json::{Value, json};

use crate::http::{HttpOptions, app};
use crate::oauth::{OAuthConfig, SUPPORTED_SCOPES};
use crate::oauth_upstream::{GitHubConfig, GoogleConfig, UpstreamConfig};

const JWT_SECRET: &[u8] = b"memdusa-test-hs256-secret-do-not-use-in-production";

// ------------------------------------------------------------- mock upstream

#[derive(Default)]
struct MockData {
    base: String,
    gh_id: String,
    gh_login: String,
    gh_email: Option<String>,
    gh_orgs: Vec<String>,
    gh_challenges: HashMap<String, String>,
    gh_authorize_error: bool,
    gh_token_error: bool,
    bad_pkce: bool,
    google_sub: String,
    google_email: String,
    google_verified: bool,
    google_hd: Option<String>,
    google_challenges: HashMap<String, String>,
    google_nonces: HashMap<String, String>,
    google_aud: String,
    bad_nonce: bool,
    google_token_error: bool,
}

impl MockData {
    fn realistic() -> Self {
        Self {
            gh_id: "424242".into(),
            gh_login: "octocat".into(),
            gh_email: Some("octocat@example.com".into()),
            google_sub: "google-123".into(),
            google_email: "alice@acme.com".into(),
            google_verified: true,
            ..Default::default()
        }
    }
}

type Shared = Arc<Mutex<MockData>>;

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn mock_upstream() -> (String, Shared) {
    let data = Arc::new(Mutex::new(MockData::default()));
    let app = Router::new()
        .route("/login/oauth/authorize", get(gh_authorize))
        .route("/login/oauth/access_token", post(gh_token))
        .route("/user", get(gh_user))
        .route("/user/memberships/orgs", get(gh_orgs))
        .route("/.well-known/openid-configuration", get(google_discovery))
        .route("/authorize", get(google_authorize))
        .route("/token", post(google_token))
        .route("/jwks", get(google_jwks))
        .route("/userinfo", get(google_userinfo))
        .with_state(data.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    data.lock().unwrap().base = base.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, data)
}

async fn gh_authorize(State(d): State<Shared>, Query(q): Query<HashMap<String, String>>) -> axum::response::Response {
    let state = q.get("state").cloned().unwrap_or_default();
    let challenge = q.get("code_challenge").cloned().unwrap_or_default();
    if q.get("code_challenge_method").map(String::as_str) != Some("S256") {
        return (StatusCode::BAD_REQUEST, "S256 required").into_response();
    }
    let mut d = d.lock().unwrap();
    let redirect = q.get("redirect_uri").cloned().unwrap_or_default();
    if d.gh_authorize_error {
        let location = format!("{redirect}?error=access_denied&state={}", pct(&state));
        return (StatusCode::FOUND, [(header::LOCATION, location)]).into_response();
    }
    d.gh_challenges.insert(state.clone(), challenge);
    let location = format!("{redirect}?code=gh-{}&state={}", pct(&state), pct(&state));
    (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
}

async fn gh_token(State(d): State<Shared>, Form(f): Form<HashMap<String, String>>) -> axum::response::Response {
    let code = f.get("code").cloned().unwrap_or_default();
    let state = code.strip_prefix("gh-").unwrap_or_default();
    let d = d.lock().unwrap();
    if d.gh_token_error {
        return (StatusCode::INTERNAL_SERVER_ERROR, "token failure").into_response();
    }
    let expected = d.gh_challenges.get(state).map(String::as_str).unwrap_or("");
    let challenge = sha256_b64(f.get("code_verifier").map(String::as_str).unwrap_or(""));
    if d.bad_pkce || expected != challenge {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))).into_response();
    }
    Json(json!({"access_token": "gh-token", "token_type": "bearer"})).into_response()
}

async fn gh_user(State(d): State<Shared>, req: Request) -> axum::response::Response {
    if req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) != Some("Bearer gh-token") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let d = d.lock().unwrap();
    Json(json!({"id": d.gh_id.parse::<i64>().unwrap_or(0), "login": d.gh_login, "email": d.gh_email})).into_response()
}

async fn gh_orgs(State(d): State<Shared>, req: Request) -> axum::response::Response {
    if req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) != Some("Bearer gh-token") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let d = d.lock().unwrap();
    let rows: Vec<Value> = d.gh_orgs.iter().map(|o| json!({"state": "active", "organization": {"login": o}})).collect();
    Json(rows).into_response()
}

async fn google_discovery(State(d): State<Shared>) -> axum::response::Response {
    let base = d.lock().unwrap().base.clone();
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "jwks_uri": format!("{base}/jwks"),
        "userinfo_endpoint": format!("{base}/userinfo"),
    }))
    .into_response()
}

async fn google_authorize(
    State(d): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::Response {
    let state = q.get("state").cloned().unwrap_or_default();
    let challenge = q.get("code_challenge").cloned().unwrap_or_default();
    let nonce = q.get("nonce").cloned().unwrap_or_default();
    if q.get("code_challenge_method").map(String::as_str) != Some("S256") || nonce.is_empty() {
        return (StatusCode::BAD_REQUEST, "S256 + nonce required").into_response();
    }
    let mut d = d.lock().unwrap();
    d.google_challenges.insert(state.clone(), challenge);
    d.google_nonces.insert(state.clone(), nonce);
    d.google_aud = q.get("client_id").cloned().unwrap_or_default();
    let redirect = q.get("redirect_uri").cloned().unwrap_or_default();
    let location = format!("{redirect}?code=go-{}&state={}", pct(&state), pct(&state));
    (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
}

async fn google_token(State(d): State<Shared>, Form(f): Form<HashMap<String, String>>) -> axum::response::Response {
    let code = f.get("code").cloned().unwrap_or_default();
    let state = code.strip_prefix("go-").unwrap_or_default();
    let d = d.lock().unwrap();
    if d.google_token_error {
        return (StatusCode::INTERNAL_SERVER_ERROR, "token failure").into_response();
    }
    let expected = d.google_challenges.get(state).cloned().unwrap_or_default();
    let challenge = sha256_b64(f.get("code_verifier").map(String::as_str).unwrap_or(""));
    if d.bad_pkce || expected != challenge {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))).into_response();
    }
    let nonce =
        if d.bad_nonce { "wrong-nonce".to_owned() } else { d.google_nonces.get(state).cloned().unwrap_or_default() };
    let audience = d.google_aud.clone();
    #[derive(Serialize)]
    struct Claims {
        iss: String,
        aud: String,
        sub: String,
        exp: u64,
        iat: u64,
        nonce: String,
        email: String,
        email_verified: bool,
        hd: Option<String>,
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let claims = Claims {
        iss: d.base.clone(),
        aud: audience,
        sub: d.google_sub.clone(),
        exp: now + 3600,
        iat: now,
        nonce,
        email: d.google_email.clone(),
        email_verified: d.google_verified,
        hd: d.google_hd.clone(),
    };
    let id_token = encode(&Header::new(Algorithm::HS256), &claims, &EncodingKey::from_secret(JWT_SECRET)).unwrap();
    Json(json!({"access_token":"g-token", "id_token": id_token, "token_type": "Bearer", "expires_in": 3600}))
        .into_response()
}

async fn google_jwks() -> axum::response::Response {
    Json(json!({"keys":[{
        "kty": "oct", "alg": "HS256", "kid": "test",
        "k": URL_SAFE_NO_PAD.encode(JWT_SECRET)
    }]}))
    .into_response()
}

async fn google_userinfo(State(d): State<Shared>, req: Request) -> axum::response::Response {
    if req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) != Some("Bearer g-token") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let d = d.lock().unwrap();
    Json(json!({
        "sub": d.google_sub,
        "email": d.google_email,
        "email_verified": d.google_verified,
        "hd": d.google_hd,
    }))
    .into_response()
}

fn sha256_b64(s: &str) -> String {
    use sha2::{Digest, Sha256};
    URL_SAFE_NO_PAD.encode(Sha256::digest(s.as_bytes()))
}

// -------------------------------------------------------------- test harness

struct Harness {
    url: String,
    store: Store,
    mock: Shared,
}

async fn spawn(github: Option<GitHubConfig>, google: Option<GoogleConfig>) -> Harness {
    let (base, mock) = mock_upstream().await;
    let dir = std::env::temp_dir().join(format!("memdusa-oauth-h2b-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::connect(&sqlite_url(&dir.join("t.db"))).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut gh = github.unwrap_or_default();
    if gh.enabled() {
        gh.auth_url = format!("{base}/login/oauth/authorize");
        gh.token_url = format!("{base}/login/oauth/access_token");
        gh.api_url = base.clone();
    }
    let mut g = google.unwrap_or_default();
    if g.enabled() {
        g.discovery_url = format!("{base}/.well-known/openid-configuration");
        g.jwks_url = format!("{base}/jwks");
        g.userinfo_url = format!("{base}/userinfo");
    }
    let cfg = OAuthConfig {
        issuer: url.clone(),
        access_secs: 3600,
        refresh_secs: 30 * 24 * 3600,
        absolute_secs: 365 * 24 * 3600,
        code_secs: 600,
        txn_secs: 600,
        upstream: UpstreamConfig { github: gh, google: g },
    };
    let opts = HttpOptions {
        addr: String::new(),
        token: None,
        allowed_hosts: vec![],
        auto_prune: None,
        rate_limit: 0.0,
        metrics: false,
        default_access: DefaultPolicy::Write,
        oauth: Some(cfg),
    };
    let app = app(store.clone(), opts);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Harness { url, store, mock }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap()
}

impl Harness {
    async fn new_user(&self, name: &str) -> String {
        self.store.user_create(name, false).await.unwrap();
        self.store.key_create(name, "signin").await.unwrap().secret
    }

    async fn register(&self, redirect_uri: &str) -> String {
        let r = client()
            .post(format!("{}/oauth/register", self.url))
            .json(&json!({"client_name":"Test Harness","redirect_uris":[redirect_uri],"token_endpoint_auth_method":"none"}))
            .send().await.unwrap();
        assert_eq!(r.status(), 200);
        r.json::<Value>().await.unwrap()["client_id"].as_str().unwrap().to_owned()
    }

    /// Step 1: begin an authorization request; returns (csrf, cookie).
    async fn authorize(&self, client_id: &str, verifier: &str) -> (String, String) {
        // PKCE challenge matches the verifier used at /oauth/token by the
        // ordinary H2a flow; here we only need a syntactically valid challenge.
        let challenge = sha256_b64(verifier);
        let r = client()
            .get(format!("{}/oauth/authorize", self.url))
            .query(&[
                ("response_type", "code"),
                ("client_id", client_id),
                ("redirect_uri", "http://127.0.0.1:54321/cb"),
                ("code_challenge", challenge.as_str()),
                ("code_challenge_method", "S256"),
                ("state", "client-state"),
                ("scope", SUPPORTED_SCOPES[0]),
            ])
            .send()
            .await
            .unwrap();
        let status = r.status();
        let cookie =
            r.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().split(';').next().unwrap().to_owned();
        let html = r.text().await.unwrap();
        assert_eq!(status, 200, "{html}");
        let marker = "name=\"csrf\" value=\"";
        let csrf = html.split_once(marker).unwrap().1.split_once('"').unwrap().0.to_owned();
        (csrf, cookie)
    }

    /// Drive an upstream sign-in from the sign-in (or consent) page. Returns
    /// the final response from our callback (consent page or error page).
    async fn upstream_signin(&self, provider: &str, csrf: &str, cookie: &str, tamper_state: bool) -> reqwest::Response {
        let r = client()
            .get(format!("{}/oauth/signin/{}?csrf={}", self.url, provider, csrf))
            .header(header::COOKIE, cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 302, "{}", r.text().await.unwrap_or_default());
        let authorize_url = r.headers().get(header::LOCATION).unwrap().to_str().unwrap().to_owned();
        assert!(authorize_url.contains("code_challenge_method=S256"), "{authorize_url}");

        let r = client().get(&authorize_url).send().await.unwrap();
        assert_eq!(r.status(), 302, "upstream authorize failed");
        let mut callback = r.headers().get(header::LOCATION).unwrap().to_str().unwrap().to_owned();
        if tamper_state {
            // Alter only the random suffix, keeping the csrf prefix intact so
            // the request reaches the upstream-state comparison.
            let start = callback.find("state=").unwrap() + "state=".len();
            let end = callback[start..].find('&').map(|i| start + i).unwrap_or(callback.len());
            callback.replace_range(start..end, &format!("{csrf}.st_tampered"));
        }
        client().get(callback).header(header::COOKIE, cookie).send().await.unwrap()
    }

    /// Approve the consent page and exchange the code at /oauth/token.
    async fn approve_and_token(&self, csrf: &str, cookie: &str, client_id: &str, verifier: &str) -> (u16, Value) {
        let r = client()
            .post(format!("{}/oauth/consent", self.url))
            .header(header::COOKIE, cookie)
            .form(&[("csrf", csrf), ("decision", "allow")])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 302, "{}", r.text().await.unwrap_or_default());
        let location = r.headers().get(header::LOCATION).unwrap().to_str().unwrap();
        let code = location.split_once('?').unwrap().1.split('&').find_map(|p| p.strip_prefix("code=")).unwrap();
        let r = client()
            .post(format!("{}/oauth/token", self.url))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", "http://127.0.0.1:54321/cb"),
                ("client_id", client_id),
                ("code_verifier", verifier),
            ])
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or_default())
    }

    async fn audit_actions(&self) -> Vec<AuditEvent> {
        self.store.audit_query(AuditQuery { limit: Some(200), ..Default::default() }).await.unwrap()
    }
}

fn github_cfg(orgs: &[&str]) -> Option<GitHubConfig> {
    Some(GitHubConfig {
        client_id: "gh-client".into(),
        client_secret: "gh-secret".into(),
        allowed_orgs: orgs.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    })
}

fn google_cfg(domains: &[&str]) -> Option<GoogleConfig> {
    Some(GoogleConfig {
        client_id: "go-client".into(),
        client_secret: "go-secret".into(),
        allowed_domains: domains.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    })
}

fn setup_data(data: &Shared) {
    let mut d = data.lock().unwrap();
    let base = d.base.clone();
    *d = MockData::realistic();
    d.base = base;
}

// --------------------------------------------------------------------- tests

#[tokio::test]
async fn github_full_flow_to_consent_code_and_token() {
    let h = spawn(github_cfg(&[]), None).await;
    setup_data(&h.mock);
    h.new_user("alice").await;
    h.store.identity_insert("alice", "github", "pending:octocat", None, Some("octocat")).await.unwrap();

    let client_id = h.register("http://127.0.0.1:54321/cb").await;
    let verifier = generate_secret("v").unwrap();
    let (csrf, cookie) = h.authorize(&client_id, &verifier).await;
    let callback = h.upstream_signin("github", &csrf, &cookie, false).await;
    let html = callback.text().await.unwrap();
    assert!(html.contains("Authorize") && html.contains("alice"), "{html}");

    let (status, tokens) = h.approve_and_token(&csrf, &cookie, &client_id, &verifier).await;
    assert_eq!(status, 200, "{tokens}");
    let access = tokens["access_token"].as_str().unwrap();
    let me = client().get(format!("{}/v1/whoami", h.url)).bearer_auth(access).send().await.unwrap();
    assert_eq!(me.json::<Value>().await.unwrap()["user"], json!("alice"));

    let identities = h.store.identity_list("alice").await.unwrap();
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0].subject, "424242", "pending link bound to the real subject");
    let audit = h.audit_actions().await;
    let signin = audit
        .iter()
        .find(|e| e.action == "oauth.signin" && e.detail.as_ref().is_some_and(|d| d.to_string().contains("github")))
        .unwrap();
    assert!(signin.detail.as_ref().unwrap().to_string().contains("424242"));
}

#[tokio::test]
async fn unknown_identity_is_rejected_and_audited() {
    let h = spawn(github_cfg(&[]), None).await;
    setup_data(&h.mock);
    h.new_user("admin").await;

    let client_id = h.register("http://127.0.0.1:54321/cb").await;
    let verifier = generate_secret("v").unwrap();
    let (csrf, cookie) = h.authorize(&client_id, &verifier).await;
    let callback = h.upstream_signin("github", &csrf, &cookie, false).await;
    let html = callback.text().await.unwrap();
    assert!(html.contains("Not linked to a memdusa user"), "{html}");
    assert!(h.store.user_list().await.unwrap().len() == 1, "no user auto-provisioned");
    assert!(
        h.audit_actions()
            .await
            .iter()
            .any(|e| e.action == "oauth.identity_unknown" && e.outcome == AuditOutcome::Denied)
    );
}

#[tokio::test]
async fn pending_link_binds_by_login_then_resolves_by_subject() {
    let h = spawn(github_cfg(&[]), None).await;
    setup_data(&h.mock);
    h.new_user("alice").await;
    h.store.identity_insert("alice", "github", "pending:octocat", None, Some("octocat")).await.unwrap();

    // First sign-in binds by login → subject.
    let client_id = h.register("http://127.0.0.1:54321/cb").await;
    let verifier = generate_secret("v").unwrap();
    let (csrf, cookie) = h.authorize(&client_id, &verifier).await;
    let r = h.upstream_signin("github", &csrf, &cookie, false).await;
    assert_eq!(r.status(), 200);
    assert_eq!(h.store.identity_list("alice").await.unwrap()[0].subject, "424242");

    // Change the login upstream: the second sign-in resolves by subject.
    h.mock.lock().unwrap().gh_login = "renamed-user".into();
    let verifier2 = generate_secret("v").unwrap();
    let (csrf2, cookie2) = h.authorize(&client_id, &verifier2).await;
    let r = h.upstream_signin("github", &csrf2, &cookie2, false).await;
    let status = r.status();
    let html = r.text().await.unwrap();
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("alice"));
}

#[tokio::test]
async fn google_full_flow_and_unverified_email_rejected() {
    let h = spawn(None, google_cfg(&[])).await;
    setup_data(&h.mock);
    h.new_user("alice").await;
    h.store.identity_insert("alice", "google", "pending:alice@acme.com", Some("alice@acme.com"), None).await.unwrap();

    let client_id = h.register("http://127.0.0.1:54321/cb").await;
    let verifier = generate_secret("v").unwrap();
    let (csrf, cookie) = h.authorize(&client_id, &verifier).await;
    let r = h.upstream_signin("google", &csrf, &cookie, false).await;
    let html = r.text().await.unwrap();
    assert!(html.contains("Authorize") && html.contains("alice"), "{html}");
    let (status, tokens) = h.approve_and_token(&csrf, &cookie, &client_id, &verifier).await;
    assert_eq!(status, 200, "{tokens}");
    assert_eq!(h.store.identity_list("alice").await.unwrap()[0].subject, "google-123");

    // email_verified=false is refused at the token/ID-token stage.
    h.mock.lock().unwrap().google_verified = false;
    let verifier2 = generate_secret("v").unwrap();
    let (csrf2, cookie2) = h.authorize(&client_id, &verifier2).await;
    let r = h.upstream_signin("google", &csrf2, &cookie2, false).await;
    let html = r.text().await.unwrap();
    assert!(html.contains("Sign-in failed"), "{html}");
    assert!(h.audit_actions().await.iter().any(|e| e.action == "oauth.upstream_exchange_failed"));
}

#[tokio::test]
async fn allowlists_auto_provision_non_admin_users() {
    // GitHub org allowlist.
    let h = spawn(github_cfg(&["acme"]), None).await;
    setup_data(&h.mock);
    h.mock.lock().unwrap().gh_orgs = vec!["acme".into()];
    let client_id = h.register("http://127.0.0.1:54321/cb").await;
    let verifier = generate_secret("v").unwrap();
    let (csrf, cookie) = h.authorize(&client_id, &verifier).await;
    let r = h.upstream_signin("github", &csrf, &cookie, false).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
    let user = h.store.user_list().await.unwrap().into_iter().find(|u| u.name == "octocat").unwrap();
    assert!(!user.admin, "auto-provisioned users are never admins");
    assert_eq!(h.store.identity_list("octocat").await.unwrap()[0].subject, "424242");

    // Google domain allowlist via `hd` (email domain deliberately different).
    let h = spawn(None, google_cfg(&["acme.com"])).await;
    setup_data(&h.mock);
    h.mock.lock().unwrap().google_email = "alice@gmail.com".into();
    h.mock.lock().unwrap().google_hd = Some("acme.com".into());
    let client_id = h.register("http://127.0.0.1:54321/cb").await;
    let verifier = generate_secret("v").unwrap();
    let (csrf, cookie) = h.authorize(&client_id, &verifier).await;
    let r = h.upstream_signin("google", &csrf, &cookie, false).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
    let user = h.store.user_list().await.unwrap().into_iter().find(|u| u.name == "alice").unwrap();
    assert!(!user.admin);
}

#[tokio::test]
async fn state_nonce_and_pkce_mismatches_are_rejected() {
    // Tampered state.
    let h = spawn(github_cfg(&[]), None).await;
    setup_data(&h.mock);
    h.new_user("alice").await;
    h.store.identity_insert("alice", "github", "pending:octocat", None, Some("octocat")).await.unwrap();
    let client_id = h.register("http://127.0.0.1:54321/cb").await;
    let verifier = generate_secret("v").unwrap();
    let (csrf, cookie) = h.authorize(&client_id, &verifier).await;
    let r = h.upstream_signin("github", &csrf, &cookie, true).await;
    let html = r.text().await.unwrap();
    assert!(html.contains("did not match"), "{html}");
    assert!(h.audit_actions().await.iter().any(|e| e.action == "oauth.upstream_state_mismatch"));

    // Bad nonce (Google).
    let h = spawn(None, google_cfg(&[])).await;
    setup_data(&h.mock);
    h.new_user("alice").await;
    h.store.identity_insert("alice", "google", "pending:alice@acme.com", Some("alice@acme.com"), None).await.unwrap();
    h.mock.lock().unwrap().bad_nonce = true;
    let client_id = h.register("http://127.0.0.1:54321/cb").await;
    let verifier = generate_secret("v").unwrap();
    let (csrf, cookie) = h.authorize(&client_id, &verifier).await;
    let r = h.upstream_signin("google", &csrf, &cookie, false).await;
    assert!(r.text().await.unwrap().contains("Sign-in failed"));

    // Bad PKCE verifier at the upstream token endpoint.
    let h = spawn(github_cfg(&[]), None).await;
    setup_data(&h.mock);
    h.new_user("alice").await;
    h.store.identity_insert("alice", "github", "pending:octocat", None, Some("octocat")).await.unwrap();
    h.mock.lock().unwrap().bad_pkce = true;
    let client_id = h.register("http://127.0.0.1:54321/cb").await;
    let verifier = generate_secret("v").unwrap();
    let (csrf, cookie) = h.authorize(&client_id, &verifier).await;
    let r = h.upstream_signin("github", &csrf, &cookie, false).await;
    assert!(r.text().await.unwrap().contains("Sign-in failed"));
}

#[tokio::test]
async fn self_service_link_from_consent_page() {
    let h = spawn(github_cfg(&[]), None).await;
    setup_data(&h.mock);
    let key = h.new_user("alice").await;

    // Sign in with an API key, land on the consent page.
    let client_id = h.register("http://127.0.0.1:54321/cb").await;
    let verifier = generate_secret("v").unwrap();
    let (csrf, cookie) = h.authorize(&client_id, &verifier).await;
    let r = client()
        .post(format!("{}/oauth/signin", h.url))
        .header(header::COOKIE, &cookie)
        .form(&[("csrf", csrf.as_str()), ("api_key", key.as_str())])
        .send()
        .await
        .unwrap();
    let html = r.text().await.unwrap();
    assert!(html.contains("Link GitHub to alice"), "{html}");

    // Follow the "Link GitHub to alice" link through the upstream round trip.
    let r = h.upstream_signin("github", &csrf, &cookie, false).await;
    let html = r.text().await.unwrap();
    assert!(html.contains("Signed in as") && html.contains("alice"), "{html}");
    let identities = h.store.identity_list("alice").await.unwrap();
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0].subject, "424242");

    let (status, tokens) = h.approve_and_token(&csrf, &cookie, &client_id, &verifier).await;
    assert_eq!(status, 200, "{tokens}");
}

#[tokio::test]
async fn upstream_errors_are_handled_gracefully() {
    let h = spawn(github_cfg(&[]), None).await;
    setup_data(&h.mock);
    h.mock.lock().unwrap().gh_authorize_error = true;
    let client_id = h.register("http://127.0.0.1:54321/cb").await;
    let verifier = generate_secret("v").unwrap();
    let (csrf, cookie) = h.authorize(&client_id, &verifier).await;
    let r = h.upstream_signin("github", &csrf, &cookie, false).await;
    let html = r.text().await.unwrap();
    assert!(html.contains("Sign-in failed"), "{html}");
    assert!(h.audit_actions().await.iter().any(|e| e.action == "oauth.upstream_error"));
    let _ = (client_id, verifier);
}
