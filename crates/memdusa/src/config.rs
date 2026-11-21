//! Client configuration: which memdusa server this machine talks to.
//!
//! Stored in `~/.memdusa/config.json` (mode 0600) by `memdusa login`, and
//! overridable with `MEMDUSA_URL` / `MEMDUSA_TOKEN`. When no remote is
//! configured, or `--database-url` is given explicitly, commands use a local
//! database instead.

use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Remote {
    /// Base URL of a `memdusa serve` instance, e.g. `https://memdusa.example.com`.
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl Remote {
    pub fn new(url: &str, token: Option<String>) -> anyhow::Result<Self> {
        let url = url.trim().trim_end_matches('/');
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            anyhow::bail!("server URL must start with http:// or https://");
        }
        let url = url.strip_suffix("/mcp").unwrap_or(url);
        Ok(Self { url: url.to_owned(), token: token.filter(|t| !t.is_empty()) })
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

pub fn save_remote(remote: Option<Remote>) -> anyhow::Result<PathBuf> {
    let p = path()?;
    let mut cfg = load_file()?;
    cfg.remote = remote;
    std::fs::create_dir_all(p.parent().expect("config path has a parent"))?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&cfg)? + "\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // the file holds a bearer token
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &p)?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::Remote;

    #[test]
    fn normalizes_server_url() {
        let r = Remote::new(" https://d.example.com/mcp/ ", Some(String::new())).unwrap();
        assert_eq!(r, Remote { url: "https://d.example.com".into(), token: None });
        assert!(Remote::new("d.example.com", None).is_err());
    }
}
