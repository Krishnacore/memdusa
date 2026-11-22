//! Users and API keys.
//!
//! Keys look like `mdk_<64 hex chars>` (256 random bits). Only a SHA-256 hash
//! is stored: for high-entropy secrets a fast hash is the right tool (a slow
//! password hash protects guessable inputs; these aren't guessable), and it
//! makes lookup a single indexed equality query.

use sha2::{Digest, Sha256};
use sqlx::Row;
use sqlx::any::AnyRow;

use crate::model::*;
use crate::store::{Q, Store};
use crate::{Error, Result};

const KEY_PREFIX: &str = "mdk_";
/// Characters of the key kept in clear for listings (`mdk_` + 8).
const DISPLAY_PREFIX_LEN: usize = 12;
/// Don't write `last_used_at` more than once per minute per key.
const TOUCH_INTERVAL_MS: i64 = 60_000;

/// SHA-256 of a high-entropy secret, as lowercase hex. Shared with OAuth
/// tokens/codes: all credentials are stored as hashes, never in the clear.
pub fn hash_secret(secret: &str) -> String {
    Sha256::digest(secret.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn hash_key(secret: &str) -> String {
    hash_secret(secret)
}

/// `prefix` + 32 random bytes as hex. Used for API keys and OAuth credentials.
pub fn generate_secret(prefix: &str) -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| Error::Invalid(format!("no system randomness available: {e}")))?;
    Ok(format!("{prefix}{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

fn generate_key() -> Result<String> {
    generate_secret(KEY_PREFIX)
}

/// User names end up in identities like `alice/claude-code`, so keep them simple.
fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c));
    if ok { Ok(()) } else { Err(Error::Invalid(format!("invalid user name '{name}': use 1-40 of [a-z0-9._-]"))) }
}

impl Store {
    fn row_to_user(r: &AnyRow) -> Result<User> {
        Ok(User {
            id: r.try_get("id")?,
            name: r.try_get("name")?,
            admin: r.try_get::<i64, _>("admin")? != 0,
            created_at: r.try_get("created_at")?,
            disabled_at: r.try_get("disabled_at")?,
        })
    }

    fn row_to_key(r: &AnyRow) -> Result<ApiKey> {
        Ok(ApiKey {
            id: r.try_get("id")?,
            user: r.try_get("user_name")?,
            name: r.try_get("name")?,
            prefix: r.try_get("prefix")?,
            created_at: r.try_get("created_at")?,
            last_used_at: r.try_get("last_used_at")?,
            revoked_at: r.try_get("revoked_at")?,
        })
    }

    pub async fn user_count(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM users").fetch_one(&self.pool).await?;
        Ok(row.try_get("n")?)
    }

    pub async fn user_create(&self, name: &str, admin: bool) -> Result<User> {
        validate_name(name)?;
        let user = User { id: new_id(), name: name.into(), admin, created_at: now_ms(), disabled_at: None };
        let mut q = Q::new("INSERT INTO users (id, name, admin, created_at) VALUES (");
        let vals = [q.text(&user.id), q.text(&user.name), q.int(admin as i64), q.int(user.created_at)];
        q.push(&vals.join(", ")).push(")");
        match q.build().execute(&self.pool).await {
            Ok(_) => Ok(user),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                Err(Error::Conflict(format!("user '{name}' already exists")))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn user_list(&self) -> Result<Vec<User>> {
        sqlx::query("SELECT * FROM users ORDER BY name")
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(Self::row_to_user)
            .collect()
    }

    async fn user_get(&self, name: &str) -> Result<User> {
        let mut q = Q::new("SELECT * FROM users WHERE name = ");
        let n = q.text(name);
        q.push(&n);
        let row =
            q.build().fetch_optional(&self.pool).await?.ok_or_else(|| Error::NotFound(format!("user '{name}'")))?;
        Self::row_to_user(&row)
    }

    /// Disabling a user immediately invalidates all of their keys.
    pub async fn user_set_disabled(&self, name: &str, disabled: bool) -> Result<User> {
        let mut q = Q::new("UPDATE users SET disabled_at = ");
        q.push(if disabled { "COALESCE(disabled_at, " } else { "NULL" });
        if disabled {
            let t = q.int(now_ms());
            q.push(&t).push(")");
        }
        let n = q.text(name);
        q.push(&format!(" WHERE name = {n}"));
        if q.build().execute(&self.pool).await?.rows_affected() == 0 {
            return Err(Error::NotFound(format!("user '{name}'")));
        }
        self.user_get(name).await
    }

    pub async fn key_create(&self, user: &str, label: &str) -> Result<CreatedKey> {
        let u = self.user_get(user).await?;
        let secret = generate_key()?;
        let key = ApiKey {
            id: new_id(),
            user: u.name,
            name: if label.trim().is_empty() { "default".into() } else { label.trim().into() },
            prefix: secret[..DISPLAY_PREFIX_LEN].into(),
            created_at: now_ms(),
            last_used_at: None,
            revoked_at: None,
        };
        let mut q = Q::new("INSERT INTO api_keys (id, user_id, name, prefix, hash, created_at) VALUES (");
        let vals = [
            q.text(&key.id),
            q.text(&u.id),
            q.text(&key.name),
            q.text(&key.prefix),
            q.text(hash_key(&secret)),
            q.int(key.created_at),
        ];
        q.push(&vals.join(", ")).push(")");
        q.build().execute(&self.pool).await?;
        Ok(CreatedKey { key, secret })
    }

    /// Keys of one user, or of everyone.
    pub async fn key_list(&self, user: Option<&str>) -> Result<Vec<ApiKey>> {
        let mut q = Q::new("SELECT k.*, u.name AS user_name FROM api_keys k JOIN users u ON u.id = k.user_id");
        if let Some(u) = user {
            let u = q.text(u);
            q.push(&format!(" WHERE u.name = {u}"));
        }
        q.push(" ORDER BY u.name, k.created_at");
        q.build().fetch_all(&self.pool).await?.iter().map(Self::row_to_key).collect()
    }

    /// Revoke by key id or display prefix (`mdk_1a2b3c4d`). With `owner`, only
    /// that user's keys can be revoked.
    pub async fn key_revoke(&self, id_or_prefix: &str, owner: Option<&str>) -> Result<ApiKey> {
        let candidates: Vec<ApiKey> = self
            .key_list(owner)
            .await?
            .into_iter()
            .filter(|k| k.id == id_or_prefix || k.prefix == id_or_prefix)
            .collect();
        let key = match candidates.as_slice() {
            [k] => k.clone(),
            [] => return Err(Error::NotFound(format!("key '{id_or_prefix}'"))),
            _ => return Err(Error::Conflict(format!("'{id_or_prefix}' matches several keys; use the key id"))),
        };
        let mut q = Q::new("UPDATE api_keys SET revoked_at = COALESCE(revoked_at, ");
        let (t, id) = (q.int(now_ms()), q.text(&key.id));
        q.push(&format!("{t}) WHERE id = {id}"));
        q.build().execute(&self.pool).await?;
        Ok(self.key_list(Some(&key.user)).await?.into_iter().find(|k| k.id == key.id).expect("key exists"))
    }

    /// Resolve an API key to its user. `None` for unknown, revoked or
    /// disabled-user keys.
    pub async fn authenticate(&self, secret: &str) -> Result<Option<Identity>> {
        if !secret.starts_with(KEY_PREFIX) {
            return Ok(None);
        }
        let mut q = Q::new(
            "SELECT k.id, k.last_used_at, u.name, u.admin FROM api_keys k JOIN users u ON u.id = k.user_id \
             WHERE k.revoked_at IS NULL AND u.disabled_at IS NULL AND k.hash = ",
        );
        let h = q.text(hash_key(secret));
        q.push(&h);
        let Some(row) = q.build().fetch_optional(&self.pool).await? else { return Ok(None) };

        let now = now_ms();
        let last: Option<i64> = row.try_get("last_used_at")?;
        if last.is_none_or(|t| now - t > TOUCH_INTERVAL_MS) {
            let mut q = Q::new("UPDATE api_keys SET last_used_at = ");
            let (t, id) = (q.int(now), q.text(row.try_get::<String, _>("id")?));
            q.push(&format!("{t} WHERE id = {id}"));
            q.build().execute(&self.pool).await?;
        }
        Ok(Some(Identity { user: Some(row.try_get("name")?), admin: row.try_get::<i64, _>("admin")? != 0 }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_long_random_and_hash_stably() {
        let (a, b) = (generate_key().unwrap(), generate_key().unwrap());
        assert_ne!(a, b);
        assert_eq!(a.len(), 4 + 64);
        assert_eq!(hash_key(&a), hash_key(&a));
        assert_eq!(hash_key("x").len(), 64);
        assert!(validate_name("alice.b-1_x").is_ok());
        assert!(validate_name("Alice").is_err() && validate_name("a/b").is_err() && validate_name("").is_err());
    }
}
