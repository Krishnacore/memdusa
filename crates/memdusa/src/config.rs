//! Client configuration: which memdusa server this machine talks to.
//!
//! Stored in `~/.memdusa/config.json` (mode 0600) by `memdusa login`, and
//! overridable with `MEMDUSA_URL` / `MEMDUSA_TOKEN`. When no remote is
//! configured, or `--database-url` is given explicitly, commands use a local
//! database instead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use memdusa_core::model::Millis;
use serde::{Deserialize, Serialize};

/// An OAuth sign-in saved in `~/.memdusa/config.json`. Tokens are secrets:
/// only printed as prefixes (or not at all), never in full.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix millis at which the access token stops being usable.
    pub expires_at: Millis,
    pub scope: String,
    /// Who the server says we are (from `/v1/whoami`), for `memdusa status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// `browser` or `device`.
    #[serde(default = "default_method")]
    pub method: String,
}

fn default_method() -> String {
    "oauth".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Remote {
    /// Base URL of a `memdusa serve` instance, e.g. `https://memdusa.example.com`.
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthTokens>,
}

impl Remote {
    pub fn new(url: &str, token: Option<String>) -> anyhow::Result<Self> {
        let url = url.trim().trim_end_matches('/');
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            anyhow::bail!("server URL must start with http:// or https://");
        }
        let url = url.strip_suffix("/mcp").unwrap_or(url);
        Ok(Self { url: url.to_owned(), token: token.filter(|t| !t.is_empty()), oauth: None })
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote: Option<Remote>,
}

/// The user's home directory (`HOME`, or `USERPROFILE` on Windows).
pub fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("cannot find the home directory (HOME / USERPROFILE not set)")
}

pub fn dir() -> anyhow::Result<PathBuf> {
    if let Some(d) = std::env::var_os("MEMDUSA_HOME") {
        return Ok(PathBuf::from(d));
    }
    Ok(home_dir()?.join(".memdusa"))
}

fn path() -> anyhow::Result<PathBuf> {
    Ok(dir()?.join("config.json"))
}

fn load_file() -> anyhow::Result<ConfigFile> {
    let p = path()?;
    match std::fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ConfigFile::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

/// The configured remote server, if any. `MEMDUSA_URL` beats the config file.
pub fn remote() -> anyhow::Result<Option<Remote>> {
    let env_token = std::env::var("MEMDUSA_TOKEN").ok();
    if let Ok(url) = std::env::var("MEMDUSA_URL")
        && !url.trim().is_empty()
    {
        return Remote::new(&url, env_token).map(Some);
    }
    Ok(load_file()?.remote.map(|mut r| {
        if env_token.is_some() {
            r.token = env_token;
        }
        r
    }))
}

/// Write a secret-bearing file atomically, readable only by its owner.
fn write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().expect("file path has a parent"))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn save_remote(remote: Option<Remote>) -> anyhow::Result<PathBuf> {
    let p = path()?;
    let mut cfg = load_file()?;
    cfg.remote = remote;
    write_private(&p, &(serde_json::to_string_pretty(&cfg)? + "\n"))?;
    Ok(p)
}

/// Save an OAuth sign-in for `url`, replacing any stored tokens (and any API
/// key) for that server. Called after login and after every token rotation.
pub fn save_oauth(url: &str, tokens: OAuthTokens) -> anyhow::Result<PathBuf> {
    let p = path()?;
    let mut cfg = load_file()?;
    let remote = cfg.remote.get_or_insert(Remote::new(url, None)?);
    anyhow::ensure!(remote.url == url.trim_end_matches('/'), "config points at a different server");
    remote.token = None;
    remote.oauth = Some(tokens);
    write_private(&p, &(serde_json::to_string_pretty(&cfg)? + "\n"))?;
    Ok(p)
}

/// Dynamically registered `client_id`s, cached per server (RFC 7591 clients
/// aren't meant to be re-registered on every login).
fn clients_path() -> anyhow::Result<PathBuf> {
    Ok(dir()?.join("oauth-clients.json"))
}

pub fn client_id(url: &str) -> anyhow::Result<Option<String>> {
    let p = clients_path()?;
    match std::fs::read_to_string(&p) {
        Ok(text) => Ok(serde_json::from_str::<BTreeMap<String, String>>(&text).ok().and_then(|m| m.get(url).cloned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

pub fn save_client_id(url: &str, id: &str) -> anyhow::Result<()> {
    let p = clients_path()?;
    let mut clients = match std::fs::read_to_string(&p) {
        Ok(text) => serde_json::from_str::<BTreeMap<String, String>>(&text).unwrap_or_default(),
        Err(_) => BTreeMap::new(),
    };
    clients.insert(url.to_owned(), id.to_owned());
    write_private(&p, &serde_json::to_string_pretty(&clients)?)
}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::{Remote, save_oauth};

    #[test]
    fn normalizes_server_url() {
        let r = Remote::new(" https://d.example.com/mcp/ ", Some(String::new())).unwrap();
        assert_eq!(r, Remote { url: "https://d.example.com".into(), token: None, oauth: None });
        assert!(Remote::new("d.example.com", None).is_err());
    }

    #[test]
    fn oauth_roundtrip() {
        let _env = super::TEST_ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("memdusa-config-{}", memdusa_core::model::new_id()));
        let old = std::env::var_os("MEMDUSA_HOME");
        unsafe {
            std::env::set_var("MEMDUSA_HOME", &dir);
            std::env::remove_var("MEMDUSA_URL");
            std::env::remove_var("MEMDUSA_TOKEN");
        }
        let tokens = super::OAuthTokens {
            access_token: "mdo_a".into(),
            refresh_token: "mdr_r".into(),
            expires_at: 1234,
            scope: "memdusa".into(),
            user: Some("alice".into()),
            method: "device".into(),
        };
        save_oauth("https://d.example.com", tokens.clone()).unwrap();
        assert_eq!(super::remote().unwrap().unwrap().oauth, Some(tokens));
        super::save_client_id("https://d.example.com", "mdoc_1").unwrap();
        assert_eq!(super::client_id("https://d.example.com").unwrap().as_deref(), Some("mdoc_1"));
        unsafe {
            match old {
                Some(v) => std::env::set_var("MEMDUSA_HOME", v),
                None => std::env::remove_var("MEMDUSA_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
