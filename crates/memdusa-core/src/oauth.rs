//! OAuth 2.1 storage (H2a): dynamic clients, sign-in transactions,
//! authorization codes, grants and rotated access/refresh tokens.
//!
//! Portable SQL on top of `sqlx::Any` (SQLite + Postgres), following the same
//! conventions as the rest of the store: TEXT ids, BIGINT unix millis, JSON as
//! TEXT, and every value bound as a `$N` placeholder.

use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::any::AnyRow;

use crate::auth::hash_secret;
use crate::model::*;
use crate::store::{Q, Store};
use crate::{Error, Result};

pub const ACCESS_PREFIX: &str = "mdo_";
pub const REFRESH_PREFIX: &str = "mdr_";
pub const CODE_PREFIX: &str = "mdc_";
pub const DEVICE_PREFIX: &str = "mdd_";

/// Lifecycle of a device authorization request. Only `approved` mints tokens.
pub mod device_status {
    pub const PENDING: &str = "pending";
    pub const AUTHENTICATED: &str = "authenticated";
    pub const APPROVED: &str = "approved";
    pub const DENIED: &str = "denied";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthClient {
    pub id: String,
    pub client_id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub created_at: Millis,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTransaction {
    pub csrf: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub scope: String,
    pub resource: Option<String>,
    pub state: String,
    pub user_name: Option<String>,
    pub created_at: Millis,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthGrant {
    pub id: String,
    pub user_name: String,
    pub client_id: String,
    pub scope: String,
    pub resource: Option<String>,
    pub method: String,
    pub created_at: Millis,
    pub last_interactive_at: Millis,
    pub revoked_at: Option<Millis>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCode {
    pub id: String,
    pub secret: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub scope: String,
    pub resource: Option<String>,
    pub grant_id: String,
    pub expires_at: Millis,
    pub used_at: Option<Millis>,
    pub created_at: Millis,
}

/// A pending RFC 8628 device authorization. `id` doubles as the CSRF token
/// for the sign-in/consent pages; both the device code and the short user code
/// are stored only as hashes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthDeviceCode {
    pub id: String,
    /// The code as shown to the user (e.g. `ABCD-EFGH`); never stored raw.
    pub user_code: String,
    pub client_id: String,
    pub scope: String,
    pub resource: Option<String>,
    pub expires_at: Millis,
    /// Minimum seconds between token polls.
    pub interval: i64,
    pub status: String,
    pub user_name: Option<String>,
    pub last_polled_at: Option<Millis>,
    pub created_at: Millis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenKind {
    Access,
    Refresh,
}

impl TokenKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Access => "access",
            Self::Refresh => "refresh",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthToken {
    pub id: String,
    pub secret: String,
    pub grant_id: String,
    pub kind: TokenKind,
    pub prefix: String,
    pub scope: String,
    pub resource: Option<String>,
    pub expires_at: Millis,
    pub rotated_from: Option<String>,
    pub revoked_at: Option<Millis>,
    pub last_used_at: Option<Millis>,
    pub created_at: Millis,
}

/// Result of resolving an `mdo_` bearer token in the HTTP auth middleware.
#[derive(Debug, Clone)]
pub struct AccessAuth {
    pub identity: Identity,
    /// `true` for tokens scoped to `memdusa:read` only.
    pub read_only: bool,
}

/// One entry of `memdusa sessions` / the sessions API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthSession {
    pub id: String,
    pub grant_id: String,
    pub user: String,
    pub client_id: String,
    pub client_name: String,
    pub kind: String,
    pub prefix: String,
    pub scope: String,
    pub resource: Option<String>,
    pub created_at: Millis,
    pub expires_at: Millis,
    pub last_used_at: Option<Millis>,
    pub revoked_at: Option<Millis>,
}

fn strings(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

/// User codes are case-insensitive and may be typed with or without the dash.
fn normalize_user_code(s: &str) -> String {
    s.trim().to_ascii_uppercase().replace([' ', '-'], "")
}

fn row_to_client(r: &AnyRow) -> Result<OAuthClient> {
    Ok(OAuthClient {
        id: r.try_get("id")?,
        client_id: r.try_get("client_id")?,
        name: r.try_get("name")?,
        redirect_uris: strings(&r.try_get::<String, _>("redirect_uris")?),
        grant_types: strings(&r.try_get::<String, _>("grant_types")?),
        created_at: r.try_get("created_at")?,
    })
}

fn row_to_txn(r: &AnyRow) -> Result<OAuthTransaction> {
    Ok(OAuthTransaction {
        csrf: r.try_get("csrf")?,
        client_id: r.try_get("client_id")?,
        redirect_uri: r.try_get("redirect_uri")?,
        code_challenge: r.try_get("code_challenge")?,
        scope: r.try_get("scope")?,
        resource: r.try_get("resource")?,
        state: r.try_get("state")?,
        user_name: r.try_get("user_name")?,
        created_at: r.try_get("created_at")?,
    })
}

fn row_to_grant(r: &AnyRow) -> Result<OAuthGrant> {
    Ok(OAuthGrant {
        id: r.try_get("id")?,
        user_name: r.try_get("user_name")?,
        client_id: r.try_get("client_id")?,
        scope: r.try_get("scope")?,
        resource: r.try_get("resource")?,
        method: r.try_get("method")?,
        created_at: r.try_get("created_at")?,
        last_interactive_at: r.try_get("last_interactive_at")?,
        revoked_at: r.try_get("revoked_at")?,
    })
}

fn row_to_code(r: &AnyRow) -> Result<OAuthCode> {
    Ok(OAuthCode {
        id: r.try_get("id")?,
        // Never returned to callers; kept on the struct only for insertion.
        secret: String::new(),
        client_id: r.try_get("client_id")?,
        redirect_uri: r.try_get("redirect_uri")?,
        code_challenge: r.try_get("code_challenge")?,
        scope: r.try_get("scope")?,
        resource: r.try_get("resource")?,
        grant_id: r.try_get("grant_id")?,
        expires_at: r.try_get("expires_at")?,
        used_at: r.try_get("used_at")?,
        created_at: r.try_get("created_at")?,
    })
}

fn row_to_device(r: &AnyRow) -> Result<OAuthDeviceCode> {
    Ok(OAuthDeviceCode {
        id: r.try_get("id")?,
        // Never returned to callers; only the hash lives in the database.
        user_code: String::new(),
        client_id: r.try_get("client_id")?,
        scope: r.try_get("scope")?,
        resource: r.try_get("resource")?,
        expires_at: r.try_get("expires_at")?,
        interval: r.try_get("interval")?,
        status: r.try_get("status")?,
        user_name: r.try_get("user_name")?,
        last_polled_at: r.try_get("last_polled_at")?,
        created_at: r.try_get("created_at")?,
    })
}

fn row_to_token(r: &AnyRow) -> Result<OAuthToken> {
    Ok(OAuthToken {
        id: r.try_get("id")?,
        secret: String::new(),
        grant_id: r.try_get("grant_id")?,
        kind: if r.try_get::<String, _>("kind")? == "refresh" { TokenKind::Refresh } else { TokenKind::Access },
        prefix: r.try_get("prefix")?,
        scope: r.try_get("scope")?,
        resource: r.try_get("resource")?,
        expires_at: r.try_get("expires_at")?,
        rotated_from: r.try_get("rotated_from")?,
        revoked_at: r.try_get("revoked_at")?,
        last_used_at: r.try_get("last_used_at")?,
        created_at: r.try_get("created_at")?,
    })
}

impl Store {
    // ---------------------------------------------------------------- clients

    pub async fn oauth_client_create(&self, c: &OAuthClient) -> Result<()> {
        let mut q =
            Q::new("INSERT INTO oauth_clients (id, client_id, name, redirect_uris, grant_types, created_at) VALUES (");
        let vals = [
            q.text(&c.id),
            q.text(&c.client_id),
            q.text(&c.name),
            q.text(serde_json::to_string(&c.redirect_uris).unwrap_or_else(|_| "[]".into())),
            q.text(serde_json::to_string(&c.grant_types).unwrap_or_else(|_| "[]".into())),
            q.int(c.created_at),
        ];
        q.push(&vals.join(", ")).push(")");
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    pub async fn oauth_client_get(&self, client_id: &str) -> Result<Option<OAuthClient>> {
        let mut q = Q::new("SELECT * FROM oauth_clients WHERE client_id = ");
        let id = q.text(client_id);
        q.push(&id);
        q.build().fetch_optional(&self.pool).await?.map(|r| row_to_client(&r)).transpose()
    }

    // ------------------------------------------------------------ transactions

    pub async fn oauth_txn_create(&self, t: &OAuthTransaction) -> Result<()> {
        let mut q = Q::new(
            "INSERT INTO oauth_transactions (csrf, client_id, redirect_uri, code_challenge, scope, resource, state, user_name, created_at) VALUES (",
        );
        let vals = [
            q.text(&t.csrf),
            q.text(&t.client_id),
            q.text(&t.redirect_uri),
            q.text(&t.code_challenge),
            q.text(&t.scope),
            q.opt_text(t.resource.clone()),
            q.text(&t.state),
            q.opt_text(t.user_name.clone()),
            q.int(t.created_at),
        ];
        q.push(&vals.join(", ")).push(")");
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    pub async fn oauth_txn_get(&self, csrf: &str) -> Result<Option<OAuthTransaction>> {
        let mut q = Q::new("SELECT * FROM oauth_transactions WHERE csrf = ");
        let c = q.text(csrf);
        q.push(&c);
        q.build().fetch_optional(&self.pool).await?.map(|r| row_to_txn(&r)).transpose()
    }

    pub async fn oauth_txn_set_user(&self, csrf: &str, user: &str) -> Result<()> {
        let mut q = Q::new("UPDATE oauth_transactions SET user_name = ");
        let (u, c) = (q.text(user), q.text(csrf));
        q.push(&format!("{u} WHERE csrf = {c}"));
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    pub async fn oauth_txn_delete(&self, csrf: &str) -> Result<()> {
        let mut q = Q::new("DELETE FROM oauth_transactions WHERE csrf = ");
        let c = q.text(csrf);
        q.push(&c);
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    // ------------------------------------------------------------------ codes

    pub async fn oauth_code_create(&self, c: &OAuthCode) -> Result<()> {
        let mut q = Q::new(
            "INSERT INTO oauth_codes (id, hash, client_id, redirect_uri, code_challenge, scope, resource, grant_id, expires_at, used_at, created_at) VALUES (",
        );
        let vals = [
            q.text(&c.id),
            q.text(hash_secret(&c.secret)),
            q.text(&c.client_id),
            q.text(&c.redirect_uri),
            q.text(&c.code_challenge),
            q.text(&c.scope),
            q.opt_text(c.resource.clone()),
            q.text(&c.grant_id),
            q.int(c.expires_at),
            q.opt_int(c.used_at),
            q.int(c.created_at),
        ];
        q.push(&vals.join(", ")).push(")");
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    pub async fn oauth_code_get(&self, secret: &str) -> Result<Option<OAuthCode>> {
        let mut q = Q::new("SELECT * FROM oauth_codes WHERE hash = ");
        let h = q.text(hash_secret(secret));
        q.push(&h);
        q.build().fetch_optional(&self.pool).await?.map(|r| row_to_code(&r)).transpose()
    }

    /// Atomically claim a code. `false` means it was already used (replay).
    pub async fn oauth_code_claim(&self, secret: &str, at: Millis) -> Result<bool> {
        let mut q = Q::new("UPDATE oauth_codes SET used_at = ");
        let (t, h) = (q.int(at), q.text(hash_secret(secret)));
        q.push(&format!("{t} WHERE hash = {h} AND used_at IS NULL"));
        Ok(q.build().execute(&self.pool).await?.rows_affected() > 0)
    }

    // ---------------------------------------------------------- device codes

    /// Store a device authorization. Both secrets are hashed; `user_code` is
    /// kept on the struct for display and for the hash written here.
    pub async fn oauth_device_create(&self, d: &OAuthDeviceCode, device_secret: &str) -> Result<()> {
        let mut q = Q::new(
            "INSERT INTO oauth_device_codes (id, hash, user_code, client_id, scope, resource, expires_at, \
             interval, status, user_name, last_polled_at, created_at) VALUES (",
        );
        let vals = [
            q.text(&d.id),
            q.text(hash_secret(device_secret)),
            q.text(hash_secret(&normalize_user_code(&d.user_code))),
            q.text(&d.client_id),
            q.text(&d.scope),
            q.opt_text(d.resource.clone()),
            q.int(d.expires_at),
            q.int(d.interval),
            q.text(&d.status),
            q.opt_text(d.user_name.clone()),
            q.opt_int(d.last_polled_at),
            q.int(d.created_at),
        ];
        q.push(&vals.join(", ")).push(")");
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    /// Look up by the long device code (token endpoint).
    pub async fn oauth_device_get(&self, device_secret: &str) -> Result<Option<OAuthDeviceCode>> {
        let mut q = Q::new("SELECT * FROM oauth_device_codes WHERE hash = ");
        let h = q.text(hash_secret(device_secret));
        q.push(&h);
        q.build().fetch_optional(&self.pool).await?.map(|r| row_to_device(&r)).transpose()
    }

    /// Look up by the short user code (the human enters it on the device page).
    pub async fn oauth_device_get_by_user_code(&self, user_code: &str) -> Result<Option<OAuthDeviceCode>> {
        let mut q = Q::new("SELECT * FROM oauth_device_codes WHERE user_code = ");
        let h = q.text(hash_secret(&normalize_user_code(user_code)));
        q.push(&h);
        q.build().fetch_optional(&self.pool).await?.map(|r| row_to_device(&r)).transpose()
    }

    /// The device row's id is its sign-in CSRF token.
    pub async fn oauth_device_get_by_csrf(&self, csrf: &str) -> Result<Option<OAuthDeviceCode>> {
        let mut q = Q::new("SELECT * FROM oauth_device_codes WHERE id = ");
        let i = q.text(csrf);
        q.push(&i);
        q.build().fetch_optional(&self.pool).await?.map(|r| row_to_device(&r)).transpose()
    }

    pub async fn oauth_device_set_user(&self, id: &str, user: &str) -> Result<()> {
        let mut q = Q::new("UPDATE oauth_device_codes SET user_name = ");
        let u = q.text(user);
        let s = q.text(device_status::AUTHENTICATED);
        q.push(&format!("{u}, status = {s} WHERE id = "));
        let i = q.text(id);
        q.push(&i);
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    pub async fn oauth_device_set_status(&self, id: &str, status: &str) -> Result<()> {
        let mut q = Q::new("UPDATE oauth_device_codes SET status = ");
        let s = q.text(status);
        q.push(&format!("{s} WHERE id = "));
        let i = q.text(id);
        q.push(&i);
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    /// Mark a poll, returning whether this poll was within the interval and
    /// must be answered with `slow_down`.
    pub async fn oauth_device_poll(&self, id: &str, at: Millis, interval_ms: i64) -> Result<bool> {
        let mut q = Q::new("SELECT last_polled_at FROM oauth_device_codes WHERE id = ");
        let i = q.text(id);
        q.push(&i);
        let last: Option<i64> = q
            .build()
            .fetch_optional(&self.pool)
            .await?
            .map(|r| r.try_get::<Option<i64>, _>("last_polled_at"))
            .transpose()?
            .flatten();
        let too_fast = last.is_some_and(|t| at.saturating_sub(t) < interval_ms);
        let mut q = Q::new("UPDATE oauth_device_codes SET last_polled_at = ");
        let t = q.int(at);
        q.push(&format!("{t} WHERE id = "));
        let i = q.text(id);
        q.push(&i);
        q.build().execute(&self.pool).await?;
        Ok(too_fast)
    }

    // ----------------------------------------------------------------- grants

    /// Find the active grant for this user/client/scope/resource, refreshing
    /// its interactive timestamp, or start a new grant chain on first sign-in.
    pub async fn oauth_grant_find_or_create(
        &self,
        user: &str,
        client_id: &str,
        scope: &str,
        resource: Option<&str>,
        method: &str,
    ) -> Result<OAuthGrant> {
        let mut q = Q::new("SELECT * FROM oauth_grants WHERE user_name = ");
        let u = q.text(user);
        q.push(&u);
        let c = q.text(client_id);
        q.push(&format!(" AND client_id = {c}"));
        let s = q.text(scope);
        q.push(&format!(" AND scope = {s}"));
        match resource {
            Some(r) => {
                let r = q.text(r);
                q.push(&format!(" AND resource = {r}"));
            }
            None => {
                q.push(" AND resource IS NULL");
            }
        }
        q.push(" AND revoked_at IS NULL ORDER BY created_at DESC, id DESC LIMIT 1");
        if let Some(row) = q.build().fetch_optional(&self.pool).await? {
            let grant = row_to_grant(&row)?;
            let mut q = Q::new("UPDATE oauth_grants SET last_interactive_at = ");
            let (t, id) = (q.int(now_ms()), q.text(&grant.id));
            q.push(&format!("{t} WHERE id = {id}"));
            q.build().execute(&self.pool).await?;
            return Ok(OAuthGrant { last_interactive_at: now_ms(), ..grant });
        }
        let grant = OAuthGrant {
            id: new_id(),
            user_name: user.into(),
            client_id: client_id.into(),
            scope: scope.into(),
            resource: resource.map(String::from),
            method: method.into(),
            created_at: now_ms(),
            last_interactive_at: now_ms(),
            revoked_at: None,
        };
        let mut q = Q::new(
            "INSERT INTO oauth_grants (id, user_name, client_id, scope, resource, method, created_at, last_interactive_at, revoked_at) VALUES (",
        );
        let vals = [
            q.text(&grant.id),
            q.text(&grant.user_name),
            q.text(&grant.client_id),
            q.text(&grant.scope),
            q.opt_text(grant.resource.clone()),
            q.text(&grant.method),
            q.int(grant.created_at),
            q.int(grant.last_interactive_at),
            q.opt_int(None),
        ];
        q.push(&vals.join(", ")).push(")");
        q.build().execute(&self.pool).await?;
        Ok(grant)
    }

    pub async fn oauth_grant_get(&self, id: &str) -> Result<Option<OAuthGrant>> {
        let mut q = Q::new("SELECT * FROM oauth_grants WHERE id = ");
        let g = q.text(id);
        q.push(&g);
        q.build().fetch_optional(&self.pool).await?.map(|r| row_to_grant(&r)).transpose()
    }

    /// Revoke a grant and every token in its chain (used on refresh-token theft).
    pub async fn oauth_grant_revoke(&self, id: &str, at: Millis) -> Result<()> {
        let mut q = Q::new("UPDATE oauth_grants SET revoked_at = COALESCE(revoked_at, ");
        let (t, g) = (q.int(at), q.text(id));
        q.push(&format!("{t}) WHERE id = {g}"));
        q.build().execute(&self.pool).await?;
        let mut q = Q::new("UPDATE oauth_tokens SET revoked_at = COALESCE(revoked_at, ");
        let (t, g) = (q.int(at), q.text(id));
        q.push(&format!("{t}) WHERE grant_id = {g}"));
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    // ----------------------------------------------------------------- tokens

    pub async fn oauth_token_create(&self, t: &OAuthToken) -> Result<()> {
        let mut q = Q::new(
            "INSERT INTO oauth_tokens (id, hash, grant_id, kind, prefix, scope, resource, expires_at, rotated_from, revoked_at, last_used_at, created_at) VALUES (",
        );
        let vals = [
            q.text(&t.id),
            q.text(hash_secret(&t.secret)),
            q.text(&t.grant_id),
            q.text(t.kind.as_str()),
            q.text(&t.prefix),
            q.text(&t.scope),
            q.opt_text(t.resource.clone()),
            q.int(t.expires_at),
            q.opt_text(t.rotated_from.clone()),
            q.opt_int(t.revoked_at),
            q.opt_int(t.last_used_at),
            q.int(t.created_at),
        ];
        q.push(&vals.join(", ")).push(")");
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    pub async fn oauth_token_get(&self, secret: &str) -> Result<Option<OAuthToken>> {
        let mut q = Q::new("SELECT * FROM oauth_tokens WHERE hash = ");
        let h = q.text(hash_secret(secret));
        q.push(&h);
        q.build().fetch_optional(&self.pool).await?.map(|r| row_to_token(&r)).transpose()
    }

    pub async fn oauth_token_revoke(&self, id: &str, at: Millis) -> Result<()> {
        let mut q = Q::new("UPDATE oauth_tokens SET revoked_at = COALESCE(revoked_at, ");
        let (t, i) = (q.int(at), q.text(id));
        q.push(&format!("{t}) WHERE id = {i}"));
        q.build().execute(&self.pool).await?;
        Ok(())
    }

    pub async fn oauth_tokens_for_grant(&self, grant_id: &str) -> Result<Vec<OAuthToken>> {
        let mut q = Q::new("SELECT * FROM oauth_tokens WHERE grant_id = ");
        let g = q.text(grant_id);
        q.push(&g).push(" ORDER BY created_at, id");
        q.build().fetch_all(&self.pool).await?.iter().map(row_to_token).collect()
    }

    /// Resolve an `mdo_` access token to the same `Identity` API keys produce,
    /// plus its read-only flag. Returns `None` for unknown/expired/revoked
    /// tokens, disabled users or revoked grants.
    pub async fn authenticate_access(&self, secret: &str, now: Millis) -> Result<Option<AccessAuth>> {
        if !secret.starts_with(ACCESS_PREFIX) {
            return Ok(None);
        }
        let mut q = Q::new(
            "SELECT t.id AS token_id, t.scope, t.last_used_at, u.name AS user_name, u.admin, \
             c.name AS client_name FROM oauth_tokens t \
             JOIN oauth_grants g ON g.id = t.grant_id \
             JOIN users u ON u.name = g.user_name \
             LEFT JOIN oauth_clients c ON c.client_id = g.client_id \
             WHERE t.kind = 'access' AND t.hash = ",
        );
        let h = q.text(hash_secret(secret));
        q.push(&h);
        q.push(" AND t.revoked_at IS NULL AND t.expires_at > ");
        let n = q.int(now);
        q.push(&n);
        q.push(" AND g.revoked_at IS NULL AND u.disabled_at IS NULL");
        let Some(row) = q.build().fetch_optional(&self.pool).await? else { return Ok(None) };
        let scope: String = row.try_get("scope")?;
        let read_only = !scope.split_whitespace().any(|s| s == "memdusa");
        // Touch at most once a minute, like API keys.
        let last: Option<i64> = row.try_get("last_used_at")?;
        if last.is_none_or(|t| now - t > 60_000) {
            let mut q = Q::new("UPDATE oauth_tokens SET last_used_at = ");
            let (t, id) = (q.int(now), q.text(row.try_get::<String, _>("token_id")?));
            q.push(&format!("{t} WHERE id = {id}"));
            q.build().execute(&self.pool).await?;
        }
        Ok(Some(AccessAuth {
            identity: Identity { user: Some(row.try_get("user_name")?), admin: row.try_get::<i64, _>("admin")? != 0 },
            read_only,
        }))
    }

    // --------------------------------------------------------------- sessions

    pub async fn oauth_sessions_list(&self, user: Option<&str>) -> Result<Vec<OAuthSession>> {
        let mut q = Q::new(
            "SELECT t.*, g.user_name, g.client_id, c.name AS client_name FROM oauth_tokens t \
             JOIN oauth_grants g ON g.id = t.grant_id \
             LEFT JOIN oauth_clients c ON c.client_id = g.client_id \
             WHERE t.kind = 'access' AND t.revoked_at IS NULL",
        );
        if let Some(u) = user {
            let u = q.text(u);
            q.push(&format!(" AND g.user_name = {u}"));
        }
        q.push(" ORDER BY t.created_at DESC, t.id DESC");
        q.build().fetch_all(&self.pool).await?.iter().map(row_to_session).collect()
    }

    /// Revoke the whole grant chain behind a session id (a token id or a
    /// grant id), optionally restricted to one user. Returns how many session
    /// tokens were revoked.
    pub async fn oauth_session_revoke(&self, id: &str, user: Option<&str>, now: Millis) -> Result<usize> {
        let mut q =
            Q::new("SELECT t.grant_id FROM oauth_tokens t JOIN oauth_grants g ON g.id = t.grant_id WHERE t.id = ");
        let i = q.text(id);
        q.push(&i);
        let grant_id: Option<String> =
            q.build().fetch_optional(&self.pool).await?.map(|r| r.try_get::<String, _>("grant_id")).transpose()?;
        let grant_id = match grant_id {
            Some(g) => g,
            None => {
                // Also accept a grant id directly.
                let mut q = Q::new("SELECT id FROM oauth_grants WHERE id = ");
                let i = q.text(id);
                q.push(&i);
                match q.build().fetch_optional(&self.pool).await? {
                    Some(_) => id.to_owned(),
                    None => return Err(Error::NotFound(format!("session '{id}'"))),
                }
            }
        };
        if let Some(u) = user {
            let grant = self.oauth_grant_get(&grant_id).await?.ok_or_else(|| Error::NotFound("session".into()))?;
            if grant.user_name != u {
                return Err(Error::NotFound(format!("session '{id}'")));
            }
        }
        let before = self.oauth_sessions_list(user).await?.len();
        self.oauth_grant_revoke(&grant_id, now).await?;
        let after = self.oauth_sessions_list(user).await?.len();
        Ok(before.saturating_sub(after))
    }
}

fn row_to_session(r: &AnyRow) -> Result<OAuthSession> {
    Ok(OAuthSession {
        id: r.try_get("id")?,
        grant_id: r.try_get("grant_id")?,
        user: r.try_get("user_name")?,
        client_id: r.try_get("client_id")?,
        client_name: r.try_get::<Option<String>, _>("client_name")?.unwrap_or_default(),
        kind: r.try_get("kind")?,
        prefix: r.try_get("prefix")?,
        scope: r.try_get("scope")?,
        resource: r.try_get("resource")?,
        created_at: r.try_get("created_at")?,
        expires_at: r.try_get("expires_at")?,
        last_used_at: r.try_get("last_used_at")?,
        revoked_at: r.try_get("revoked_at")?,
    })
}
