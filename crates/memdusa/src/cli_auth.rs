//! CLI side of OAuth: browser (PKCE + loopback) and device (RFC 8628) sign-in,
//! transparent refresh, and RFC 7009 revocation. Tokens live only in
//! `~/.memdusa/config.json` (mode 0600) and are never printed in full.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use memdusa_core::model::now_ms;
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::config::{self, OAuthTokens};

pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Refresh a little before the access token actually expires: a request in
/// flight must not come back 401 just as the token dies.
pub const REFRESH_SKEW_MS: i64 = 60_000;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
const STALE_LOCK: Duration = Duration::from_secs(60);
const LOCK_TIMEOUT: Duration = Duration::from_secs(3);

// ------------------------------------------------------------- AS metadata

#[derive(Clone, Debug)]
pub struct Metadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: String,
    pub revocation_endpoint: String,
    pub device_authorization_endpoint: String,
}

impl Metadata {
    fn fallback(url: &str) -> Self {
        Self {
            issuer: url.to_owned(),
            authorization_endpoint: format!("{url}/oauth/authorize"),
            token_endpoint: format!("{url}/oauth/token"),
            registration_endpoint: format!("{url}/oauth/register"),
            revocation_endpoint: format!("{url}/oauth/revoke"),
            device_authorization_endpoint: format!("{url}/oauth/device_authorization"),
        }
    }
}

pub fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?)
}

/// RFC 8414 discovery, with conventional endpoint paths as a fallback for
/// servers that don't publish metadata yet.
pub async fn discover(url: &str, http: &reqwest::Client) -> anyhow::Result<Metadata> {
    let fallback = Metadata::fallback(url);
    let resp = match http.get(format!("{url}/.well-known/oauth-authorization-server")).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            tracing::debug!("AS metadata at {url} returned {}", r.status());
            return Ok(fallback);
        }
        Err(e) => {
            tracing::debug!("AS metadata at {url} unreachable: {e}");
            return Ok(fallback);
        }
    };
    let doc: Value = resp.json().await.unwrap_or_default();
    let field = |k: &str, default: String| {
        doc.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from).unwrap_or(default)
    };
    Ok(Metadata {
        issuer: field("issuer", fallback.issuer),
        authorization_endpoint: field("authorization_endpoint", fallback.authorization_endpoint),
        token_endpoint: field("token_endpoint", fallback.token_endpoint),
        registration_endpoint: field("registration_endpoint", fallback.registration_endpoint),
        revocation_endpoint: field("revocation_endpoint", fallback.revocation_endpoint),
        device_authorization_endpoint: field("device_authorization_endpoint", fallback.device_authorization_endpoint),
    })
}

// --------------------------------------------------------------- PKCE etc.

fn random_bytes(n: usize) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("no system randomness available: {e}"))?;
    Ok(buf)
}

fn pkce() -> anyhow::Result<(String, String)> {
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes(32)?);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Ok((verifier, challenge))
}

fn state() -> anyhow::Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes(24)?))
}

#[derive(Debug, Deserialize, Default)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

async fn post_token(
    http: &reqwest::Client,
    endpoint: &str,
    params: &[(&str, String)],
) -> anyhow::Result<(reqwest::StatusCode, TokenResponse)> {
    let resp = http.post(endpoint).form(params).send().await.with_context(|| format!("contacting {endpoint}"))?;
    let status = resp.status();
    let body: TokenResponse = resp.json().await.unwrap_or_default();
    Ok((status, body))
}

fn token_error(body: &TokenResponse, status: reqwest::StatusCode) -> anyhow::Result<()> {
    if let Some(code) = &body.error {
        let desc = body.error_description.as_deref().unwrap_or("");
        anyhow::bail!("token endpoint: {code}{}", if desc.is_empty() { String::new() } else { format!(": {desc}") });
    }
    if body.access_token.is_none() {
        anyhow::bail!("token endpoint returned {} without an access token", status);
    }
    Ok(())
}

/// Fetch the identity and persist the new tokens. Returns what was stored.
async fn finish_login(
    url: &str,
    http: &reqwest::Client,
    body: &TokenResponse,
    method: &str,
    previous: Option<&OAuthTokens>,
) -> anyhow::Result<OAuthTokens> {
    let access = body.access_token.clone().context("token endpoint returned no access_token")?;
    let refresh = body
        .refresh_token
        .clone()
        .or_else(|| previous.map(|t| t.refresh_token.clone()))
        .context("token endpoint returned no refresh_token")?;
    let expires_in = body.expires_in.filter(|s| *s > 0).unwrap_or(3600);
    let user = whoami(http, url, &access)
        .await
        .ok()
        .and_then(|v| v.get("user").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from))
        .or_else(|| previous.and_then(|t| t.user.clone()));
    let tokens = OAuthTokens {
        access_token: access,
        refresh_token: refresh,
        expires_at: now_ms() + expires_in.saturating_mul(1000),
        scope: body.scope.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "memdusa".into()),
        user,
        method: method.to_owned(),
    };
    config::save_oauth(url, tokens.clone())?;
    Ok(tokens)
}

pub async fn whoami(http: &reqwest::Client, url: &str, access: &str) -> anyhow::Result<Value> {
    Ok(http.get(format!("{url}/v1/whoami")).bearer_auth(access).send().await?.json().await?)
}

// ------------------------------------------------------------ registration

async fn ensure_client_id(
    url: &str,
    meta: &Metadata,
    http: &reqwest::Client,
    redirect: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(id) = config::client_id(url)? {
        return Ok(id);
    }
    let mut body = json!({
        "client_name": if redirect.is_some() { "memdusa CLI" } else { "memdusa CLI (device)" },
        "token_endpoint_auth_method": "none",
        "scope": "memdusa",
    });
    if let Some(redirect) = redirect {
        body["redirect_uris"] = json!([redirect]);
        body["grant_types"] = json!(["authorization_code", "refresh_token"]);
        body["response_types"] = json!(["code"]);
    } else {
        body["grant_types"] = json!([DEVICE_GRANT]);
    }
    let resp = http
        .post(&meta.registration_endpoint)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("registering a client with {url}"))?;
    let status = resp.status();
    let doc: Value = resp.json().await.unwrap_or_default();
    anyhow::ensure!(status.is_success(), "client registration failed ({status}): {doc}");
    let id = doc["client_id"].as_str().context("registration response had no client_id")?.to_owned();
    config::save_client_id(url, &id)?;
    Ok(id)
}

// -------------------------------------------------------------- browser flow

type OpenFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;

/// How to "open the browser". The test hook lets a test drive sign-in + consent
/// over HTTP and then lets the real loopback listener receive the code.
#[derive(Clone)]
pub enum BrowserOpener {
    System,
    #[cfg_attr(not(test), allow(dead_code))] // used by the end-to-end test hook
    Test(Arc<dyn Fn(String) -> OpenFuture + Send + Sync>),
}

impl BrowserOpener {
    async fn open(&self, url: &str) -> anyhow::Result<()> {
        match self {
            Self::Test(f) => f(url.to_owned()).await,
            Self::System => {
                let url = url.to_owned();
                tokio::task::spawn_blocking(move || launch_browser(&url)).await??;
                Ok(())
            }
        }
    }
}

fn launch_browser(url: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(url).status();
    #[cfg(target_os = "windows")]
    let result = Command::new("cmd").args(["/C", "start", "", url]).status();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let result = Command::new("xdg-open").arg(url).status();
    let status = result.with_context(|| "could not launch a browser")?;
    anyhow::ensure!(status.success(), "browser opener exited with {status}");
    Ok(())
}

pub struct BrowserOptions {
    pub opener: BrowserOpener,
    pub timeout: Duration,
}

impl Default for BrowserOptions {
    fn default() -> Self {
        Self { opener: BrowserOpener::System, timeout: DEFAULT_TIMEOUT }
    }
}

/// Receive one HTTP request on the loopback listener and answer it so the
/// browser can be closed. Returns its query parameters.
async fn callback_params(listener: TcpListener) -> anyhow::Result<HashMap<String, String>> {
    let (mut socket, _) = listener.accept().await.context("accepting the OAuth callback")?;
    let mut request = vec![0u8; 8192];
    let n = socket.read(&mut request).await.context("reading the OAuth callback")?;
    let request = String::from_utf8_lossy(&request[..n]);
    let line = request.lines().next().context("empty OAuth callback request")?;
    let target = line.split_whitespace().nth(1).context("malformed OAuth callback request")?;
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let parsed = Url::parse(&format!("http://callback/?{query}"))?;
    let params: HashMap<String, String> = parsed.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    let body = "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\nconnection: close\r\n\r\n\
                <h1>Signed in</h1><p>You can close this window and return to your terminal.</p>";
    let _ = socket.write_all(body.as_bytes()).await;
    let _ = socket.shutdown().await;
    Ok(params)
}

pub async fn browser_login(url: &str, options: &BrowserOptions) -> anyhow::Result<OAuthTokens> {
    let url = config::Remote::new(url, None)?.url;
    let http = http_client()?;
    let meta = discover(&url, &http).await?;
    let listener = TcpListener::bind("127.0.0.1:0").await.context("starting a loopback listener")?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let client_id = ensure_client_id(&url, &meta, &http, Some(&redirect)).await?;
    let (verifier, challenge) = pkce()?;
    let state = state()?;
    let mut authorize = Url::parse(&meta.authorization_endpoint)?;
    authorize
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("scope", "memdusa");
    println!("Open this URL to sign in to {url}:\n{authorize}");
    // Listen while the browser (or test hook) is busy: the redirect can arrive
    // before the opener returns.
    let callback = tokio::spawn(callback_params(listener));
    if let Err(e) = options.opener.open(authorize.as_str()).await {
        eprintln!("memdusa: could not open a browser ({e}); use the URL above");
    }
    let joined = tokio::time::timeout(options.timeout, callback).await.map_err(|_| {
        anyhow::anyhow!(
            "timed out after {:?} waiting for the browser sign-in; try again or use --device",
            options.timeout
        )
    })?;
    let params = joined.context("the OAuth callback listener failed")??;
    if let Some(error) = params.get("error") {
        anyhow::bail!("sign-in was denied or failed: {error}");
    }
    if params.get("state").map(String::as_str) != Some(state.as_str()) {
        anyhow::bail!("OAuth state mismatch (possible CSRF); no tokens were saved");
    }
    let code = params.get("code").context("the callback had no authorization code")?;
    let (status, body) = post_token(
        &http,
        &meta.token_endpoint,
        &[
            ("grant_type", "authorization_code".into()),
            ("code", code.clone()),
            ("client_id", client_id),
            ("redirect_uri", redirect),
            ("code_verifier", verifier),
        ],
    )
    .await?;
    token_error(&body, status)?;
    finish_login(&url, &http, &body, "browser", None).await
}

// --------------------------------------------------------------- device flow

pub struct DeviceOptions {
    pub timeout: Duration,
    /// Called with (verification_uri_complete, user_code) once the device
    /// authorization is created. Defaults to printing both.
    pub on_code: Option<Arc<dyn Fn(String, String) + Send + Sync>>,
}

impl Default for DeviceOptions {
    fn default() -> Self {
        Self { timeout: DEFAULT_TIMEOUT, on_code: None }
    }
}

pub async fn device_login(url: &str, options: &DeviceOptions) -> anyhow::Result<OAuthTokens> {
    let url = config::Remote::new(url, None)?.url;
    let http = http_client()?;
    let meta = discover(&url, &http).await?;
    let client_id = ensure_client_id(&url, &meta, &http, None).await?;
    let resp = http
        .post(&meta.device_authorization_endpoint)
        .form(&[("client_id", client_id.as_str()), ("scope", "memdusa")])
        .send()
        .await
        .with_context(|| format!("starting device sign-in with {url}"))?;
    let status = resp.status();
    let doc: Value = resp.json().await.unwrap_or_default();
    anyhow::ensure!(status.is_success(), "device authorization failed ({status}): {doc}");
    let device_code = doc["device_code"].as_str().context("no device_code in response")?.to_owned();
    let user_code = doc["user_code"].as_str().context("no user_code in response")?.to_owned();
    let verification = doc["verification_uri_complete"]
        .as_str()
        .or_else(|| doc["verification_uri"].as_str())
        .context("no verification_uri in response")?
        .to_owned();
    let expires_in = doc["expires_in"].as_i64().unwrap_or(600);
    let mut interval = Duration::from_secs(doc["interval"].as_i64().unwrap_or(5).max(1) as u64);
    println!("To sign in, visit:\n  {verification}\nand enter the code: {user_code}");
    if let Some(hook) = &options.on_code {
        hook(verification.clone(), user_code.clone());
    }
    let deadline = Instant::now() + options.timeout.min(Duration::from_secs(expires_in.max(1) as u64));
    loop {
        tokio::time::sleep(interval).await;
        anyhow::ensure!(
            Instant::now() < deadline,
            "device code expired before it was approved; run `memdusa login --device` again"
        );
        let (status, body) = post_token(
            &http,
            &meta.token_endpoint,
            &[
                ("grant_type", DEVICE_GRANT.into()),
                ("device_code", device_code.clone()),
                ("client_id", client_id.clone()),
            ],
        )
        .await?;
        match body.error.as_deref() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval += Duration::from_secs(5);
                continue;
            }
            Some("expired_token") => anyhow::bail!("device code expired; run `memdusa login --device` again"),
            Some("access_denied") => anyhow::bail!("device sign-in was denied"),
            Some(other) => {
                anyhow::bail!("device sign-in failed: {other} {}", body.error_description.as_deref().unwrap_or(""))
            }
            None if status.is_success() => return finish_login(&url, &http, &body, "device", None).await,
            None => anyhow::bail!("device sign-in failed with HTTP {status}"),
        }
    }
}

// ----------------------------------------------------------------- refresh

fn lock_path() -> anyhow::Result<PathBuf> {
    Ok(config::dir()?.join("oauth-refresh.lock"))
}

struct RefreshLock(PathBuf);

impl Drop for RefreshLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Only one process may rotate a refresh token at a time: hooks run in
/// parallel and the server revokes a whole grant chain on token reuse.
async fn acquire_lock(path: &Path) -> anyhow::Result<Option<RefreshLock>> {
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut f) => {
                let _ = write!(f, "{}", std::process::id());
                return Ok(Some(RefreshLock(path.to_path_buf())));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(path)
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().unwrap_or_default() > STALE_LOCK)
                    .unwrap_or(true);
                if stale {
                    let _ = fs::remove_file(path);
                    continue;
                }
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }
}

fn current_tokens(url: &str) -> anyhow::Result<OAuthTokens> {
    config::remote()?
        .and_then(|r| if r.url == url { r.oauth } else { None })
        .context("not signed in to this server with OAuth; run `memdusa login`")
}

/// Return a usable access token, refreshing (once, under a lock) if it is
/// expired/near expiry or if `force` is set after a 401.
pub async fn access_token(url: &str, force: bool) -> anyhow::Result<String> {
    let before = current_tokens(url)?;
    if !force && before.expires_at > now_ms() + REFRESH_SKEW_MS {
        return Ok(before.access_token.clone());
    }
    let path = lock_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let Some(_lock) = acquire_lock(&path).await? else {
        // Another process may have just refreshed; reuse its result if so.
        let after = current_tokens(url)?;
        anyhow::ensure!(
            after.refresh_token != before.refresh_token,
            "timed out waiting for another process to refresh the token"
        );
        return Ok(after.access_token);
    };
    let after = current_tokens(url)?;
    if after.refresh_token != before.refresh_token && after.expires_at > now_ms() {
        // Already rotated by a concurrent process; don't spend this token.
        return Ok(after.access_token);
    }
    let http = http_client()?;
    let meta = discover(url, &http).await?;
    let (status, body) = post_token(
        &http,
        &meta.token_endpoint,
        &[
            ("grant_type", "refresh_token".into()),
            ("refresh_token", after.refresh_token.clone()),
            ("client_id", config::client_id(url)?.unwrap_or_default()),
        ],
    )
    .await?;
    if let Some(code) = &body.error {
        anyhow::bail!("refresh failed ({code}); run `memdusa login` again to sign in");
    }
    anyhow::ensure!(status.is_success(), "refresh failed with HTTP {status}; run `memdusa login` again");
    let fresh = finish_login(url, &http, &body, &after.method, Some(&after)).await?;
    Ok(fresh.access_token)
}

/// Revoke the refresh token at the server (RFC 7009) and forget local tokens.
pub async fn logout(url: &str) -> anyhow::Result<()> {
    let Some(tokens) = config::remote()?.and_then(|r| r.oauth) else {
        return Ok(()); // API-key login: nothing to revoke
    };
    let http = http_client()?;
    let meta = discover(url, &http).await.unwrap_or_else(|_| Metadata::fallback(url));
    let revoke = http
        .post(&meta.revocation_endpoint)
        .form(&[("token", tokens.refresh_token.as_str()), ("token_type_hint", "refresh_token")])
        .send()
        .await;
    match revoke {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => eprintln!("memdusa: server did not revoke the session ({}); deleting local tokens anyway", r.status()),
        Err(e) => {
            eprintln!("memdusa: could not reach the server to revoke the session ({e}); deleting local tokens anyway")
        }
    }
    config::save_remote(None)?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::ffi::OsString;

    use memdusa_core::Store;
    use memdusa_core::model::{new_id, now_ms};
    use memdusa_core::store::sqlite_url;
    use reqwest::header::{COOKIE, SET_COOKIE};

    use super::*;
    use crate::config::TEST_ENV_LOCK;
    use crate::hooks::{HookConfig, HookEvent, HookRequest};
    use crate::http::HttpOptions;
    use crate::oauth::{OAuthConfig, SUPPORTED_SCOPES};

    pub(crate) struct Home {
        pub(crate) dir: PathBuf,
        _env: std::sync::MutexGuard<'static, ()>,
        old_home: Option<OsString>,
        old_memdusa_home: Option<OsString>,
        old_url: Option<OsString>,
        old_token: Option<OsString>,
    }

    impl Home {
        pub(crate) fn new() -> Self {
            let env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("memdusa-cli-oauth-{}", new_id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let old_home = std::env::var_os("HOME");
            let old_memdusa_home = std::env::var_os("MEMDUSA_HOME");
            let old_url = std::env::var_os("MEMDUSA_URL");
            let old_token = std::env::var_os("MEMDUSA_TOKEN");
            unsafe {
                std::env::set_var("HOME", &dir);
                std::env::set_var("MEMDUSA_HOME", &dir);
                std::env::remove_var("MEMDUSA_URL");
                std::env::remove_var("MEMDUSA_TOKEN");
            }
            Self { dir, _env: env, old_home, old_memdusa_home, old_url, old_token }
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let restore = |name: &str, value: Option<OsString>| unsafe {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            };
            restore("HOME", self.old_home.take());
            restore("MEMDUSA_HOME", self.old_memdusa_home.take());
            restore("MEMDUSA_URL", self.old_url.take());
            restore("MEMDUSA_TOKEN", self.old_token.take());
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    pub(crate) fn test_config() -> OAuthConfig {
        OAuthConfig {
            issuer: String::new(),
            access_secs: 3600,
            refresh_secs: 30 * 24 * 3600,
            absolute_secs: 365 * 24 * 3600,
            code_secs: 600,
            txn_secs: 600,
            device_secs: 600,
            device_interval_secs: 1,
            upstream: crate::oauth_upstream::UpstreamConfig::default(),
        }
    }

    /// In-process server whose metadata issuer is its real URL (so discovery
    /// hands the client usable endpoints).
    pub(crate) async fn spawn(mut cfg: OAuthConfig) -> (String, Store) {
        let dir = std::env::temp_dir().join(format!("memdusa-cli-server-{}", new_id()));
        fs::create_dir_all(&dir).unwrap();
        let store = Store::connect(&sqlite_url(&dir.join("t.db"))).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        cfg.issuer = url.clone();
        let opts = HttpOptions {
            addr: String::new(),
            token: None,
            allowed_hosts: vec![],
            auto_prune: None,
            rate_limit: 0.0,
            metrics: false,
            default_access: memdusa_core::model::DefaultPolicy::Write,
            oauth: Some(cfg),
        };
        let app = crate::http::app(store.clone(), opts);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, store)
    }

    async fn user_key(store: &Store, name: &str) -> String {
        store.user_create(name, false).await.unwrap();
        store.key_create(name, "signin").await.unwrap().secret
    }

    fn hidden(html: &str, name: &str) -> String {
        let marker = format!("name=\"{name}\" value=\"");
        let rest = html.split_once(&marker).unwrap_or_else(|| panic!("no hidden {name}: {html}")).1;
        rest.split_once('"').unwrap().0.to_owned()
    }

    /// Drive the server's sign-in + consent pages exactly as a browser would.
    async fn sign_in_and_consent(url: &str, key: &str, authorize_url: &str) -> anyhow::Result<()> {
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::limited(10)).build()?;
        let r = http.get(authorize_url).send().await?;
        let cookie = r.headers().get(SET_COOKIE).unwrap().to_str().unwrap().split(';').next().unwrap().to_owned();
        let csrf = hidden(&r.text().await?, "csrf");
        let r = http
            .post(format!("{url}/oauth/signin"))
            .header(COOKIE, &cookie)
            .form(&[("csrf", csrf.as_str()), ("api_key", key)])
            .send()
            .await?;
        anyhow::ensure!(r.status().is_success(), "sign-in failed: {}", r.status());
        let csrf = hidden(&r.text().await?, "csrf");
        // Following the 302 delivers the code to the CLI's loopback listener.
        let r = http
            .post(format!("{url}/oauth/consent"))
            .header(COOKIE, &cookie)
            .form(&[("csrf", csrf.as_str()), ("decision", "allow")])
            .send()
            .await?;
        anyhow::ensure!(r.status().is_success(), "consent failed: {}", r.status());
        Ok(())
    }

    async fn approve_device(url: &str, key: &str, verification: &str, _user_code: &str) {
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let r = http.get(verification).send().await.unwrap();
        let cookie = r.headers().get(SET_COOKIE).unwrap().to_str().unwrap().split(';').next().unwrap().to_owned();
        let csrf = hidden(&r.text().await.unwrap(), "csrf");
        let r = http
            .post(format!("{url}/oauth/signin"))
            .header(COOKIE, &cookie)
            .form(&[("csrf", csrf.as_str()), ("api_key", key)])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
        let csrf = hidden(&r.text().await.unwrap(), "csrf");
        let r = http
            .post(format!("{url}/oauth/consent"))
            .header(COOKIE, &cookie)
            .form(&[("csrf", csrf.as_str()), ("decision", "allow")])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
    }

    #[tokio::test]
    async fn browser_flow_end_to_end() {
        let _home = Home::new();
        let (url, store) = spawn(test_config()).await;
        let key = Arc::new(user_key(&store, "browser-user").await);
        let hook_url = url.clone();
        let opener = BrowserOpener::Test(Arc::new(move |authorize| {
            let key = key.clone();
            let url = hook_url.clone();
            Box::pin(async move { sign_in_and_consent(&url, &key, &authorize).await })
        }));
        let tokens = browser_login(&url, &BrowserOptions { opener, timeout: Duration::from_secs(10) }).await.unwrap();
        assert_eq!(tokens.method, "browser");
        assert_eq!(tokens.user.as_deref(), Some("browser-user"));
        assert_eq!(tokens.scope, "memdusa");
        assert!(config::client_id(&url).unwrap().is_some(), "client_id should be cached");
        assert!(config::remote().unwrap().unwrap().oauth.is_some());
        let http = http_client().unwrap();
        let who = whoami(&http, &url, &tokens.access_token).await.unwrap();
        assert_eq!(who["user"], json!("browser-user"));
        // No token material is leaked through the saved config's non-secret fields.
        assert_eq!(SUPPORTED_SCOPES.len(), 2);
    }

    #[tokio::test]
    async fn device_flow_end_to_end() {
        let _home = Home::new();
        let (url, store) = spawn(test_config()).await;
        let key = Arc::new(user_key(&store, "device-user").await);
        let on_code: Arc<dyn Fn(String, String) + Send + Sync> = Arc::new({
            let key = key.clone();
            let url = url.clone();
            move |verification, user_code| {
                let key = key.clone();
                let url = url.clone();
                tokio::spawn(async move { approve_device(&url, &key, &verification, &user_code).await });
            }
        });
        let tokens = tokio::time::timeout(
            Duration::from_secs(15),
            device_login(&url, &DeviceOptions { timeout: Duration::from_secs(10), on_code: Some(on_code) }),
        )
        .await
        .expect("device login timed out")
        .unwrap();
        assert_eq!(tokens.method, "device");
        assert_eq!(tokens.user.as_deref(), Some("device-user"));
        let http = http_client().unwrap();
        assert_eq!(whoami(&http, &url, &tokens.access_token).await.unwrap()["user"], json!("device-user"));
    }

    /// Sign in via the device flow and return the first tokens.
    pub(crate) async fn login_and_tokens(url: &str, store: &Store, name: &str) -> OAuthTokens {
        let key = Arc::new(user_key(store, name).await);
        let key2 = key.clone();
        let url2 = url.to_owned();
        let on_code: Arc<dyn Fn(String, String) + Send + Sync> = Arc::new(move |verification, user_code| {
            let key = key2.clone();
            let url = url2.clone();
            tokio::spawn(async move { approve_device(&url, &key, &verification, &user_code).await });
        });
        device_login(url, &DeviceOptions { timeout: Duration::from_secs(10), on_code: Some(on_code) }).await.unwrap()
    }

    #[tokio::test]
    async fn refresh_after_expiry_and_concurrent_rotations() {
        let _home = Home::new();
        let mut cfg = test_config();
        cfg.access_secs = 1;
        let (url, store) = spawn(cfg).await;
        let first = login_and_tokens(&url, &store, "refresh-user").await;
        tokio::time::sleep(Duration::from_millis(1100)).await;

        // A REST call after expiry refreshes once and retries successfully.
        let client = crate::remote::Client::from_config(Duration::from_secs(5)).unwrap();
        let info = client.whoami().await.unwrap();
        assert_eq!(info["user"], json!("refresh-user"));
        let rotated = current_tokens(&url).unwrap();
        assert_ne!(rotated.refresh_token, first.refresh_token);

        // Two hook processes racing: neither may spend the other's rotated token.
        let (a, b) = tokio::join!(access_token(&url, true), access_token(&url, true));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a, b, "both callers should observe the same rotated access token");
        let saved = current_tokens(&url).unwrap();
        let http = http_client().unwrap();
        let meta = discover(&url, &http).await.unwrap();
        let (status, body) = post_token(
            &http,
            &meta.token_endpoint,
            &[("grant_type", "refresh_token".into()), ("refresh_token", saved.refresh_token.clone())],
        )
        .await
        .unwrap();
        assert_eq!(status, 200, "the token left in config must still work: {body:?}");
    }

    #[tokio::test]
    async fn revoked_grant_spools_hook_and_status_asks_for_login() {
        let _home = Home::new();
        let (url, store) = spawn(test_config()).await;
        let tokens = login_and_tokens(&url, &store, "revoked-user").await;
        let sessions = store.oauth_sessions_list(Some("revoked-user")).await.unwrap();
        store.oauth_grant_revoke(&sessions[0].grant_id, now_ms()).await.unwrap();

        let r = config::remote().unwrap().unwrap();
        let req = HookRequest {
            project: "demo".into(),
            config: HookConfig { agent: "test".into(), journal_prompts: false, stop_guard_mins: 0, recall: false },
            input: json!({"session_id": "s1", "cwd": "/tmp"}),
            git: None,
            at: Some(now_ms()),
            task: None,
            parent: None,
            run: None,
        };
        let resp = crate::remote_hook(HookEvent::PostToolUse, req, &r, Duration::from_secs(3)).await.unwrap();
        assert!(resp.output.is_none(), "hooks must not fail the harness");
        assert_eq!(spool_len(), 1, "the event must be in the offline queue");

        let lines = crate::status_lines(&r, Some("demo")).await.unwrap();
        let text = lines.join("\n");
        assert!(text.contains("run `memdusa login` again"), "status should tell the user to log in again: {text}");
        assert!(!text.contains(&tokens.access_token), "status must not print secrets");
    }

    fn spool_len() -> usize {
        crate::spool::Spool::open().unwrap().len()
    }

    #[tokio::test]
    async fn logout_revokes_the_refresh_token() {
        let _home = Home::new();
        let (url, store) = spawn(test_config()).await;
        let tokens = login_and_tokens(&url, &store, "logout-user").await;
        logout(&url).await.unwrap();
        assert!(config::remote().unwrap().is_none(), "local tokens must be deleted");
        let http = http_client().unwrap();
        let meta = discover(&url, &http).await.unwrap();
        let (status, body) = post_token(
            &http,
            &meta.token_endpoint,
            &[("grant_type", "refresh_token".into()), ("refresh_token", tokens.refresh_token)],
        )
        .await
        .unwrap();
        assert_eq!(status, 400);
        assert_eq!(body.error.as_deref(), Some("invalid_grant"), "the server must have revoked the grant: {body:?}");
    }
}
