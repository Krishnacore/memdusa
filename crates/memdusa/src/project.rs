//! Project key detection. The key should be identical on every machine and in
//! every harness, so the normalized git remote (`host/org/repo`) is preferred.

use std::path::Path;
use std::process::Command;

use memdusa_core::model::GitState;

/// A `git` command, or `None` when git can't run here. On a Mac without the developer tools, `/usr/bin/git` is a
/// placeholder: running it opens the "Install Command Line Developer Tools" dialog instead of running git, so a
/// hook or `memdusa resume` would pop a system dialog. `xcode-select -p` tells without that side effect.
pub fn git_command() -> Option<Command> {
    static USABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    USABLE.get_or_init(git_usable).then(|| Command::new("git"))
}

fn git_usable() -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    let exe = format!("git{}", std::env::consts::EXE_SUFFIX); // git.exe on Windows
    let Some(git) = std::env::split_paths(&path).map(|d| d.join(&exe)).find(|p| p.is_file()) else { return false };
    if cfg!(target_os = "macos") && git == Path::new("/usr/bin/git") {
        return Command::new("/usr/bin/xcode-select").arg("-p").output().is_ok_and(|o| o.status.success());
    }
    true
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git_command()?.arg("-C").arg(dir).args(args).output().ok()?;
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
    let out = git_command()?
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
    // Same repo, different worktrees: record which one this state belongs to.
    state.worktree = git(dir, &["rev-parse", "--show-toplevel"])
        .map(|p| std::fs::canonicalize(&p).map(|c| c.to_string_lossy().into_owned()).unwrap_or(p));
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
    use std::process::Command;

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
    fn git_state_records_the_worktree_root() {
        let dir = std::env::temp_dir().join(format!("memdusa-wt-{}", memdusa_core::model::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Command::new("git").arg("init").arg("-q").arg(&dir).status().unwrap().success());
        let state = super::git_state(&dir).unwrap();
        assert!(state.worktree.is_some(), "worktree root should be recorded");
        let root = std::fs::canonicalize(&dir).unwrap();
        let recorded = std::fs::canonicalize(state.worktree.unwrap()).unwrap();
        assert_eq!(recorded, root);
        let _ = std::fs::remove_dir_all(&dir);
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
