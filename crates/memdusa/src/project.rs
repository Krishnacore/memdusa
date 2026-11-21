//! Project key detection. The key should be identical on every machine and in
//! every harness, so the normalized git remote (`host/org/repo`) is preferred.

use std::path::Path;
use std::process::Command;

use memdusa_core::model::GitState;

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

/// Code state of the repo containing `dir`, from a single
/// `git status --porcelain=v2 --branch`. `None` outside a git repo.
pub fn git_state(dir: &Path) -> Option<GitState> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain=v2", "--branch", "--untracked-files=normal"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut state = parse_status(&String::from_utf8_lossy(&out.stdout));
    state.host = gethostname::gethostname().into_string().ok().filter(|h| !h.is_empty());
    Some(state)
}

const MAX_DIRTY_FILES: usize = 20;

fn parse_status(text: &str) -> GitState {
    let mut g = GitState::default();
    for line in text.lines() {
        if let Some(oid) = line.strip_prefix("# branch.oid ") {
            g.head = (oid != "(initial)").then(|| oid.to_owned());
        } else if let Some(head) = line.strip_prefix("# branch.head ") {
            g.branch = (head != "(detached)").then(|| head.to_owned());
        } else if line.starts_with('#') || line.starts_with('!') || line.is_empty() {
            continue;
        } else {
            // 1 <xy> <sub> <mH> <mI> <mW> <hH> <hI> <path>
            // 2 <xy> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\t<orig>
            // u <xy> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
            // ? <path>
            let fields = match line.as_bytes()[0] {
                b'1' => 9,
                b'2' => 10,
                b'u' => 11,
                b'?' => 2,
                _ => continue,
            };
            g.dirty_count += 1;
            if g.dirty_files.len() < MAX_DIRTY_FILES
                && let Some(path) = line.splitn(fields, ' ').nth(fields - 1)
            {
                g.dirty_files.push(path.split('\t').next().unwrap_or(path).to_owned());
            }
        }
    }
    g
}

/// Code state of the current directory, but only if its repo *is* `project`.
/// Comparing against some unrelated repo would produce bogus drift warnings.
pub fn git_state_for(project: &str) -> Option<GitState> {
    let dir = std::env::current_dir().ok()?;
    (detect(&dir) == project).then(|| git_state(&dir)).flatten()
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
    use super::{normalize_remote, parse_status};

    #[test]
    fn parses_porcelain_v2() {
        let out = "# branch.oid a1b2c3d4e5f6\n# branch.head feat/refunds\n# branch.upstream origin/feat/refunds\n\
                   1 .M N... 100644 100644 100644 abc abc src/refund.ts\n\
                   2 R. N... 100644 100644 100644 abc abc R100 docs/new name.md\tdocs/old.md\n\
                   ? notes with spaces.txt\n! target/ignored\n";
        let g = parse_status(out);
        assert_eq!(g.branch.as_deref(), Some("feat/refunds"));
        assert_eq!(g.head.as_deref(), Some("a1b2c3d4e5f6"));
        assert_eq!(g.dirty_count, 3);
        assert_eq!(g.dirty_files, vec!["src/refund.ts", "docs/new name.md", "notes with spaces.txt"]);

        let fresh = parse_status("# branch.oid (initial)\n# branch.head (detached)\n");
        assert_eq!((fresh.head, fresh.branch, fresh.dirty_count), (None, None, 0));
    }

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
