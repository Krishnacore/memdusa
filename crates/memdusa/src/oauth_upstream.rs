//! Upstream identity providers for OAuth sign-in (H2b).
//!
//! GitHub uses its OAuth web flow (state + PKCE S256); Google uses OpenID
//! Connect (state + PKCE S256 + nonce) and verifies the ID token against
//! Google's JWKS. Upstream access tokens are used exactly once to read the
//! identity and are never stored.
//!
//! The `*_url` fields are test-only overrides: they let the test suite point
//! at an in-process mock upstream without touching production defaults.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::Value;

pub const GITHUB: &str = "github";
pub const GOOGLE: &str = "google";

const GH_AUTH: &str = "https://github.com/login/oauth/authorize";
const GH_TOKEN: &str = "https://github.com/login/oauth/access_token";
const GH_API: &str = "https://api.github.com";
const GOOGLE_DISCOVERY: &str = "https://accounts.google.com/.well-known/openid-configuration";

const CACHE_TTL: Duration = Duration::from_secs(3600);

// ------------------------------------------------------------------- config

#[derive(Clone, Debug, Default)]
pub struct GitHubConfig {
    pub client_id: String,
    pub client_secret: String,
    /// Test-only override of the authorize / token / API base URLs.
    pub auth_url: String,
    pub token_url: String,
    pub api_url: String,
    /// Only members of one of these orgs may sign in (empty = no restriction).
    pub allowed_orgs: Vec<String>,
}

impl GitHubConfig {
    pub fn enabled(&self) -> bool {
        !self.client_id.is_empty() && !self.client_secret.is_empty()
    }
    pub fn auth_url(&self) -> &str {
        if self.auth_url.is_empty() { GH_AUTH } else { &self.auth_url }
    }
    pub fn token_url(&self) -> &str {
        if self.token_url.is_empty() { GH_TOKEN } else { &self.token_url }
    }
    pub fn api_url(&self) -> &str {
        if self.api_url.is_empty() { GH_API } else { &self.api_url }
    }
    pub fn has_overrides(&self) -> bool {
        !self.auth_url.is_empty() || !self.token_url.is_empty() || !self.api_url.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct GoogleConfig {
    pub client_id: String,
    pub client_secret: String,
    /// Test-only overrides of the discovery / JWKS / userinfo URLs.
    pub discovery_url: String,
    pub jwks_url: String,
    pub userinfo_url: String,
    /// Only these verified email domains (or `hd` claims) may sign in.
    pub allowed_domains: Vec<String>,
}

impl GoogleConfig {
    pub fn enabled(&self) -> bool {
        !self.client_id.is_empty() && !self.client_secret.is_empty()
    }
    pub fn discovery_url(&self) -> &str {
        if self.discovery_url.is_empty() { GOOGLE_DISCOVERY } else { &self.discovery_url }
    }
    pub fn has_overrides(&self) -> bool {
        !self.discovery_url.is_empty() || !self.jwks_url.is_empty() || !self.userinfo_url.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct UpstreamConfig {
    pub github: GitHubConfig,
    pub google: GoogleConfig,
}

// ------------------------------------------------------------------ runtime

/// A provider-independent identity, resolved from an upstream token.
#[derive(Debug, Clone)]
pub struct Identity {
    pub provider: String,
    /// Stable upstream id: GitHub numeric user id, Google `sub`.
    pub subject: String,
    /// Human handle used for pending links (GitHub login).
    pub login: Option<String>,
    /// Verified email (Google).
    pub email: Option<String>,
    /// Display name for audit/listing.
    pub display: String,
    /// Hosted domain (Google Workspace).
    pub hd: Option<String>,
}

#[derive(Clone)]
pub struct Upstream {
    pub config: UpstreamConfig,
    client: Client,
    jwks: Arc<Mutex<HashMap<String, (Instant, Jwk)>>>,
    discovery: Arc<Mutex<Option<(Instant, Discovery)>>>,
}

impl Upstream {
    pub fn new(config: UpstreamConfig) -> Self {
        Self {
            config,
            client: Client::builder().user_agent("memdusa-oauth").build().expect("build HTTP client"),
            jwks: Arc::new(Mutex::new(HashMap::new())),
            discovery: Arc::new(Mutex::new(None)),
        }
    }

    fn test_mode(&self) -> bool {
        self.config.github.has_overrides() || self.config.google.has_overrides()
    }

    pub fn enabled(&self, provider: &str) -> bool {
        match provider {
            GITHUB => self.config.github.enabled(),
            GOOGLE => self.config.google.enabled(),
            _ => false,
        }
    }

    /// The callback URL an admin must register with the provider.
    pub fn callback_url(&self, issuer: &str, provider: &str) -> String {
        format!("{issuer}/oauth/callback/{provider}")
    }

    /// Build the upstream authorize redirect for a sign-in transaction.
    pub async fn authorize_url(
        &self,
        provider: &str,
        callback: &str,
        state: &str,
        verifier: &str,
        nonce: Option<&str>,
    ) -> anyhow::Result<String> {
        let challenge = sha256_b64(verifier);
        match provider {
            GITHUB => {
                let cfg = &self.config.github;
                let mut scopes = vec!["read:user"];
                if !cfg.allowed_orgs.is_empty() {
                    scopes.push("read:org");
                }
                Ok(Url::parse_with_params(
                    cfg.auth_url(),
                    [
                        ("client_id", cfg.client_id.as_str()),
                        ("redirect_uri", callback),
                        ("scope", &scopes.join(" ")),
                        ("state", state),
                        ("code_challenge", &challenge),
                        ("code_challenge_method", "S256"),
                    ],
                )?
                .to_string())
            }
            GOOGLE => {
                let cfg = &self.config.google;
                let discovery = self.discovery().await?;
                Ok(Url::parse_with_params(
                    &discovery.authorization_endpoint,
                    [
                        ("client_id", cfg.client_id.as_str()),
                        ("redirect_uri", callback),
                        ("response_type", "code"),
                        ("scope", "openid email"),
                        ("state", state),
                        ("code_challenge", &challenge),
                        ("code_challenge_method", "S256"),
                        ("nonce", nonce.unwrap_or("")),
                        ("access_type", "online"),
                    ],
                )?
                .to_string())
            }
            _ => bail!("unknown upstream provider"),
        }
    }

    /// Exchange the upstream code and resolve the identity. The returned
    /// access token is consumed here and never leaked to the caller.
    pub async fn exchange(
        &self,
        provider: &str,
        code: &str,
        verifier: &str,
        callback: &str,
        nonce: Option<&str>,
    ) -> anyhow::Result<Identity> {
        match provider {
            GITHUB => self.github_exchange(code, verifier, callback).await,
            GOOGLE => self.google_exchange(code, verifier, callback, nonce).await,
            _ => bail!("unknown upstream provider"),
        }
    }

    // --------------------------------------------------------------- GitHub

    async fn github_exchange(&self, code: &str, verifier: &str, callback: &str) -> anyhow::Result<Identity> {
        let cfg = &self.config.github;
        let resp = self
            .client
            .post(cfg.token_url())
            .header("accept", "application/json")
            .form(&[
                ("client_id", cfg.client_id.as_str()),
                ("client_secret", cfg.client_secret.as_str()),
                ("code", code),
                ("redirect_uri", callback),
                ("code_verifier", verifier),
            ])
            .send()
            .await
            .context("GitHub token request failed")?;
        if !resp.status().is_success() {
            bail!("GitHub token endpoint returned {}", resp.status());
        }
        let body: Value = resp.json().await.context("GitHub token response was not JSON")?;
        if let Some(e) = body["error"].as_str() {
            bail!("GitHub token error: {e}");
        }
        let token = body["access_token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("GitHub token response had no access_token"))?;

        let user: Value = self
            .client
            .get(format!("{}/user", cfg.api_url()))
            .bearer_auth(token)
            .header("accept", "application/vnd.github+json")
            .send()
            .await
            .context("GitHub user request failed")?
            .error_for_status()
            .context("GitHub user endpoint returned an error")?
            .json()
            .await
            .context("GitHub user response was not JSON")?;
        let id = user["id"].as_i64().ok_or_else(|| anyhow::anyhow!("GitHub user had no numeric id"))?;
        let login = user["login"].as_str().ok_or_else(|| anyhow::anyhow!("GitHub user had no login"))?.to_owned();

        if !cfg.allowed_orgs.is_empty() {
            let orgs: Value = self
                .client
                .get(format!("{}/user/memberships/orgs", cfg.api_url()))
                .query(&[("per_page", "100"), ("state", "active")])
                .bearer_auth(token)
                .header("accept", "application/vnd.github+json")
                .send()
                .await
                .context("GitHub org request failed")?
                .error_for_status()
                .context("GitHub org endpoint returned an error")?
                .json()
                .await
                .context("GitHub org response was not JSON")?;
            let member = orgs.as_array().is_some_and(|rows| {
                rows.iter().any(|row| {
                    let name = row["organization"]["login"].as_str().unwrap_or_default();
                    cfg.allowed_orgs.iter().any(|o| o.eq_ignore_ascii_case(name))
                })
            });
            if !member {
                bail!("GitHub organization is not allowlisted");
            }
        }

        Ok(Identity {
            provider: GITHUB.into(),
            subject: id.to_string(),
            email: user["email"].as_str().map(String::from),
            display: login.clone(),
            login: Some(login),
            hd: None,
        })
    }

    // ---------------------------------------------------------------- Google

    async fn discovery(&self) -> anyhow::Result<Discovery> {
        {
            let cache = self.discovery.lock().expect("discovery cache poisoned");
            if let Some((at, d)) = cache.as_ref()
                && at.elapsed() < CACHE_TTL
            {
                return Ok(d.clone());
            }
        }
        let url = self.config.google.discovery_url();
        let d: Discovery = self
            .client
            .get(url)
            .send()
            .await
            .context("Google discovery request failed")?
            .error_for_status()
            .context("Google discovery endpoint returned an error")?
            .json()
            .await
            .context("Google discovery response was not JSON")?;
        *self.discovery.lock().expect("discovery cache poisoned") = Some((Instant::now(), d.clone()));
        Ok(d)
    }

    async fn google_exchange(
        &self,
        code: &str,
        verifier: &str,
        callback: &str,
        nonce: Option<&str>,
    ) -> anyhow::Result<Identity> {
        let cfg = &self.config.google;
        let d = self.discovery().await?;
        let resp = self
            .client
            .post(&d.token_endpoint)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", callback),
                ("client_id", cfg.client_id.as_str()),
                ("client_secret", cfg.client_secret.as_str()),
                ("code_verifier", verifier),
            ])
            .send()
            .await
            .context("Google token request failed")?;
        if !resp.status().is_success() {
            bail!("Google token endpoint returned {}", resp.status());
        }
        let body: Value = resp.json().await.context("Google token response was not JSON")?;
        if let Some(e) = body["error"].as_str() {
            bail!("Google token error: {e}");
        }
        let id_token =
            body["id_token"].as_str().ok_or_else(|| anyhow::anyhow!("Google token response had no id_token"))?;
        let access_token = body["access_token"].as_str();
        let claims = self.verify_id_token(id_token, &d).await?;

        if claims.nonce.as_deref() != nonce {
            bail!("Google ID token nonce mismatch");
        }
        if !claims.email_verified.unwrap_or(false) {
            bail!("Google email is not verified");
        }

        let mut email = claims.email.clone();
        let mut hd = claims.hd.clone();
        if email.is_none()
            && let Some(url) = d.userinfo_endpoint.as_deref().or(if cfg.userinfo_url.is_empty() {
                None
            } else {
                Some(cfg.userinfo_url.as_str())
            })
            && let Some(token) = access_token
        {
            let info: Value = self
                .client
                .get(url)
                .bearer_auth(token)
                .send()
                .await
                .context("Google userinfo request failed")?
                .error_for_status()
                .context("Google userinfo endpoint returned an error")?
                .json()
                .await
                .context("Google userinfo response was not JSON")?;
            email = info["email"].as_str().map(String::from);
            hd = info["hd"].as_str().map(String::from).or(hd);
            if !info["email_verified"].as_bool().unwrap_or(false) {
                bail!("Google email is not verified");
            }
        }
        let email = email.ok_or_else(|| anyhow::anyhow!("Google identity had no email"))?;

        if !cfg.allowed_domains.is_empty() {
            let domain = email.split('@').nth(1).unwrap_or_default();
            let domain_ok = cfg.allowed_domains.iter().any(|d| d.eq_ignore_ascii_case(domain));
            let hd_ok = hd.as_ref().is_some_and(|h| cfg.allowed_domains.iter().any(|d| d.eq_ignore_ascii_case(h)));
            if !domain_ok && !hd_ok {
                bail!("Google email domain is not allowlisted");
            }
        }

        Ok(Identity {
            provider: GOOGLE.into(),
            subject: claims.sub,
            display: email.clone(),
            email: Some(email),
            login: None,
            hd,
        })
    }

    async fn verify_id_token(&self, token: &str, d: &Discovery) -> anyhow::Result<GoogleClaims> {
        let header = decode_header(token).context("could not parse Google ID token header")?;
        let jwk = self.jwk(&d.jwks_uri, header.kid.as_deref()).await?;
        let (key, alg) = match jwk.kty.as_str() {
            "RSA" => {
                let n = jwk.n.as_deref().ok_or_else(|| anyhow::anyhow!("JWK has no modulus"))?;
                let e = jwk.e.as_deref().ok_or_else(|| anyhow::anyhow!("JWK has no exponent"))?;
                (DecodingKey::from_rsa_components(n, e).context("invalid JWK RSA components")?, Algorithm::RS256)
            }
            // Only the in-process mock upstream uses symmetric keys; real
            // Google JWKS always publishes RSA keys.
            "oct" if self.test_mode() => {
                let k = jwk.k.as_deref().ok_or_else(|| anyhow::anyhow!("JWK has no key"))?;
                let bytes = URL_SAFE_NO_PAD.decode(k).context("invalid JWK oct key")?;
                (DecodingKey::from_secret(&bytes), Algorithm::HS256)
            }
            other => bail!("unsupported JWK type '{other}'"),
        };
        // Pin the algorithm to the key type: never let the token header pick
        // RS256 vs HS256 (classic algorithm-confusion attack).
        if header.alg != alg {
            bail!("ID token algorithm does not match the JWK");
        }
        let mut validation = Validation::new(alg);
        validation.set_audience(&[&self.config.google.client_id]);
        validation.set_issuer(&[&d.issuer]);
        validation.leeway = 60;
        decode::<GoogleClaims>(token, &key, &validation)
            .map(|data| data.claims)
            .context("Google ID token verification failed")
    }

    async fn jwk(&self, jwks_uri: &str, kid: Option<&str>) -> anyhow::Result<Jwk> {
        let key = kid.unwrap_or("");
        {
            let cache = self.jwks.lock().expect("JWKS cache poisoned");
            if let Some((at, jwk)) = cache.get(key)
                && at.elapsed() < CACHE_TTL
            {
                return Ok(jwk.clone());
            }
        }
        let set: JwkSet = self
            .client
            .get(if self.config.google.jwks_url.is_empty() { jwks_uri } else { &self.config.google.jwks_url })
            .send()
            .await
            .context("JWKS request failed")?
            .error_for_status()
            .context("JWKS endpoint returned an error")?
            .json()
            .await
            .context("JWKS response was not JSON")?;
        let mut cache = self.jwks.lock().expect("JWKS cache poisoned");
        for jwk in set.keys {
            cache.insert(jwk.kid.clone().unwrap_or_default(), (Instant::now(), jwk));
        }
        cache
            .get(key)
            .or_else(|| {
                // A single published key can be used when the token omits kid.
                if cache.len() == 1 { cache.values().next() } else { None }
            })
            .map(|(_, j)| j.clone())
            .ok_or_else(|| anyhow::anyhow!("no JWK matching kid '{key}'"))
    }
}

#[derive(Clone, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    userinfo_endpoint: Option<String>,
}

#[derive(Clone, Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

#[derive(Clone, Deserialize)]
struct Jwk {
    #[serde(default)]
    kid: Option<String>,
    kty: String,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    #[serde(default)]
    k: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GoogleClaims {
    sub: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Option<bool>,
    #[serde(default)]
    hd: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
}

fn sha256_b64(input: &str) -> String {
    use sha2::{Digest, Sha256};
    URL_SAFE_NO_PAD.encode(Sha256::digest(input.as_bytes()))
}

/// Resolve the upstream identity to a memdusa user.
///
/// Order: existing subject → pending admin link (binds on match) → self-service
/// link from the consent page (when the caller is already signed in) →
/// allowlist auto-provision. GitHub pending links bind by login only; Google
/// pending links bind by its verified email.
pub async fn resolve_identity(
    store: &memdusa_core::Store,
    identity: &Identity,
    upstream: &Upstream,
    signed_in_user: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let provider = identity.provider.as_str();
    if let Some(existing) = store.identity_by_subject(provider, &identity.subject).await? {
        if let Some(user) = signed_in_user
            && existing.user_name != user
        {
            bail!("that {provider} identity is already linked to another user");
        }
        return Ok(Some(existing.user_name));
    }

    // Pending admin links bind by handle on first successful sign-in. GitHub's
    // email from `/user` is not verified, so GitHub links are bound by login
    // only. Google's email is verified and may bind its pending links.
    let mut handles: Vec<&str> = Vec::new();
    if let Some(login) = identity.login.as_deref() {
        handles.push(login);
    }
    if provider != GITHUB
        && let Some(email) = identity.email.as_deref()
    {
        handles.push(email);
    }
    for handle in &handles {
        if let Some(pending) = store.identity_pending(provider, handle).await? {
            if let Some(user) = signed_in_user
                && pending.user_name != user
            {
                bail!("that {provider} identity is already pending for another user");
            }
            store.identity_bind_pending(&pending, &identity.subject).await?;
            return Ok(Some(pending.user_name));
        }
    }

    // Self-service linking from the consent page: the user is already signed
    // in with an API key and is explicitly linking this upstream identity.
    // This takes precedence over auto-provisioning: a link-only round trip
    // must never mint a different user, even with an allowlist configured.
    if let Some(user) = signed_in_user {
        store
            .identity_insert(user, provider, &identity.subject, identity.email.as_deref(), identity.login.as_deref())
            .await?;
        return Ok(Some(user.to_owned()));
    }

    // Allowlist auto-provisioning creates a non-admin user on first sign-in.
    let allowed = match provider {
        GOOGLE => {
            let cfg = &upstream.config.google;
            !cfg.allowed_domains.is_empty()
                && identity.email.as_ref().is_some_and(|e| {
                    let domain = e.split('@').nth(1).unwrap_or_default();
                    cfg.allowed_domains.iter().any(|d| d.eq_ignore_ascii_case(domain))
                        || identity
                            .hd
                            .as_ref()
                            .is_some_and(|h| cfg.allowed_domains.iter().any(|d| d.eq_ignore_ascii_case(h)))
                })
        }
        GITHUB => {
            // The org membership was already verified in `github_exchange`;
            // a configured allowlist means this identity passed it.
            !upstream.config.github.allowed_orgs.is_empty()
        }
        _ => false,
    };
    if allowed {
        let base = match provider {
            GITHUB => identity.login.as_deref().unwrap_or("user"),
            GOOGLE => identity.email.as_deref().and_then(|e| e.split('@').next()).unwrap_or("user"),
            _ => "user",
        };
        let user = store.user_create_unique(base).await?;
        store
            .identity_insert(
                &user.name,
                provider,
                &identity.subject,
                identity.email.as_deref(),
                identity.login.as_deref(),
            )
            .await?;
        return Ok(Some(user.name));
    }

    Ok(None)
}

/// Serialize an admin/self-service link target from `provider:value`.
pub fn parse_link(value: &str) -> anyhow::Result<(String, String)> {
    let (provider, handle) =
        value.split_once(':').ok_or_else(|| anyhow::anyhow!("expected github:<login-or-id> or google:<email>"))?;
    let provider = provider.trim().to_lowercase();
    let handle = handle.trim();
    if !matches!(provider.as_str(), GITHUB | GOOGLE) {
        bail!("unknown provider '{provider}' (use github or google)");
    }
    if handle.is_empty() {
        bail!("identity handle must not be empty");
    }
    if provider == GITHUB && handle.contains('@') {
        bail!("link GitHub by login or numeric id, not email");
    }
    Ok((provider, handle.to_owned()))
}

/// A numeric handle is a stable subject and can be linked directly; anything
/// else is stored as a pending handle to be bound on first sign-in.
pub fn is_stable_subject(handle: &str) -> bool {
    !handle.is_empty() && handle.bytes().all(|b| b.is_ascii_digit())
}

pub use memdusa_core::oauth::PENDING_PREFIX;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_link_accepts_github_login_or_numeric_id() {
        assert_eq!(parse_link("github:octocat").unwrap(), (GITHUB.into(), "octocat".into()));
        assert_eq!(parse_link("GitHub: 424242 ").unwrap(), (GITHUB.into(), "424242".into()));
    }

    #[test]
    fn parse_link_rejects_github_email() {
        let e = parse_link("github:alice@acme.com").unwrap_err().to_string();
        assert_eq!(e, "link GitHub by login or numeric id, not email");
        // Google is linked by its verified email, which stays valid.
        assert_eq!(parse_link("google:alice@acme.com").unwrap(), (GOOGLE.into(), "alice@acme.com".into()));
    }
}
