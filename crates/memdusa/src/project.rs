//! Project key detection. The key should be identical on every machine and in
//! every harness, so the normalized git remote (`host/org/repo`) is preferred.

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()).filter(|s| !s.is_empty())
}

/// Remote of `dir`'s repo, else the repo root, else `dir` itself.
pub fn detect(dir: &Path) -> String {
    if let Some(remote) = git(dir, &["remote", "get-url", "origin"]) {
        return normalize_remote(&remote);
    }
    git(dir, &["rev-parse", "--show-toplevel"]).unwrap_or_else(|| dir.display().to_string())
}

pub fn detect_cwd() -> Option<String> {
    std::env::current_dir().ok().map(|d| detect(&d))
}

pub fn normalize_remote(url: &str) -> String {
    let s = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let s = s.split_once("://").map_or(s, |(_, rest)| rest);
    let s = s.rsplit_once('@').map_or(s, |(_, rest)| rest);
    // scp-like `host:org/repo` -> `host/org/repo` (but keep `host:port/...` intact)
    match s.split_once(':') {
        Some((host, rest)) if !rest.starts_with(|c: char| c.is_ascii_digit()) => format!("{host}/{rest}"),
        _ => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_remote;

    #[test]
    fn remotes_normalize_to_same_key() {
        for u in [
            "git@github.com:org/repo.git",
            "https://github.com/org/repo.git",
            "https://user@github.com/org/repo",
            "ssh://git@github.com/org/repo.git",
        ] {
            assert_eq!(normalize_remote(u), "github.com/org/repo", "{u}");
        }
    }
}
