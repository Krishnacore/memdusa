//! `memdusa install claude-code`: registers the MCP server and merges the
//! memdusa hooks into a Claude Code settings file, idempotently and without
//! touching anything else in it.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};

use crate::hooks::HookEvent;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Scope {
    /// ~/.claude/settings.json: every project on this machine.
    User,
    /// .claude/settings.json: this project, committed and shared with the team.
    Project,
    /// .claude/settings.local.json: this project, just you (gitignored).
    Local,
}

impl Scope {
    fn settings_path(self) -> anyhow::Result<PathBuf> {
        Ok(match self {
            Self::User => {
                PathBuf::from(std::env::var_os("HOME").context("HOME not set")?).join(".claude/settings.json")
            }
            Self::Project => PathBuf::from(".claude/settings.json"),
            Self::Local => PathBuf::from(".claude/settings.local.json"),
        })
    }
    fn mcp_scope(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }
}

pub struct InstallOptions {
    pub scope: Scope,
    /// Override the settings file (mostly for testing).
    pub settings: Option<PathBuf>,
    /// Command used to invoke memdusa. Default: absolute path of this binary.
    pub bin: Option<String>,
    pub mcp: bool,
    pub dry_run: bool,
    pub uninstall: bool,
}

/// Is this hook command one of ours? Matches regardless of binary location.
fn is_memdusa_hook(cmd: &str) -> bool {
    let cmd = cmd.trim_start();
    let (bin, rest) = match cmd.strip_prefix('\'') {
        Some(q) => match q.split_once('\'') {
            Some((bin, rest)) => (bin, rest),
            None => return false,
        },
        None => cmd.split_once(char::is_whitespace).unwrap_or((cmd, "")),
    };
    bin.ends_with("memdusa") && rest.split_whitespace().next() == Some("hook")
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+:@".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Remove our previous hook entries from `settings`, then (unless
/// uninstalling) add the current set. Everything else is preserved.
pub fn merge_hooks(mut settings: Value, bin: &str, uninstall: bool) -> anyhow::Result<Value> {
    let root = settings.as_object_mut().context("settings file is not a JSON object")?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().context("`hooks` is not a JSON object")?;

    // strip existing memdusa hooks everywhere
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else { continue };
        for g in groups.iter_mut() {
            if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                hs.retain(|h| !h.get("command").and_then(Value::as_str).is_some_and(is_memdusa_hook));
            }
        }
        groups.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|hs| !hs.is_empty()));
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|g| !g.is_empty()));

    if !uninstall {
        for ev in HookEvent::ALL {
            let mut group = Map::new();
            if let Some(m) = ev.matcher() {
                group.insert("matcher".into(), json!(m));
            }
            group.insert(
                "hooks".into(),
                json!([{
                    "type": "command",
                    "command": format!("{} hook {}", shell_quote(bin), ev.arg()),
                    "timeout": ev.timeout_secs(),
                }]),
            );
            let list = hooks.entry(ev.claude_name()).or_insert_with(|| json!([]));
            list.as_array_mut()
                .with_context(|| format!("hooks.{} is not an array", ev.claude_name()))?
                .push(group.into());
        }
    }
    if hooks.is_empty() {
        root.remove("hooks");
    }
    Ok(settings)
}

pub fn run(opts: InstallOptions) -> anyhow::Result<()> {
    let bin = match opts.bin {
        Some(b) => b,
        None => std::env::current_exe()?.canonicalize()?.display().to_string(),
    };
    let path = match opts.settings {
        Some(p) => p,
        None => opts.scope.settings_path()?,
    };

    let current: Value = match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).with_context(|| format!("{} is not valid JSON; fix it first", path.display()))?
        }
        Ok(_) => json!({}),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let updated = merge_hooks(current.clone(), &bin, opts.uninstall)?;
    let text = serde_json::to_string_pretty(&updated)? + "\n";

    if opts.dry_run {
        println!("# {} (dry run)\n{text}", path.display());
    } else if updated == current {
        println!("hooks already up to date in {}", path.display());
    } else {
        write_with_backup(&path, &text)?;
        let verb = if opts.uninstall { "removed memdusa hooks from" } else { "installed memdusa hooks into" };
        println!("{verb} {}", path.display());
    }

    if opts.mcp {
        mcp_register(&bin, opts.scope, opts.dry_run, opts.uninstall)?;
    }
    if !opts.uninstall && !opts.dry_run {
        println!("\nDone. Start a new Claude Code session: it will load the resume brief automatically.");
    }
    Ok(())
}

fn write_with_backup(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        let backup = path.with_extension("json.memdusa-backup");
        std::fs::copy(path, &backup)?;
        println!("backup: {}", backup.display());
    }
    // write-then-rename so a crash never leaves a half-written settings file
    let tmp = path.with_extension("json.memdusa-tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn mcp_register(bin: &str, scope: Scope, dry_run: bool, uninstall: bool) -> anyhow::Result<()> {
    let args: Vec<String> = if uninstall {
        ["mcp", "remove", "memdusa", "--scope", scope.mcp_scope()].map(String::from).to_vec()
    } else {
        ["mcp", "add", "memdusa", "--scope", scope.mcp_scope(), "--", bin, "mcp", "--agent", "claude-code"]
            .map(String::from)
            .to_vec()
    };
    let shown = format!("claude {}", args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" "));
    if dry_run {
        println!("# would run: {shown}");
        return Ok(());
    }
    let exists = Command::new("claude")
        .args(["mcp", "get", "memdusa"])
        .output()
        .map(|o| o.status.success())
        .map_err(|_| anyhow::anyhow!("`claude` CLI not found; register the MCP server manually:\n  {shown}"));
    match (exists, uninstall) {
        (Err(e), _) => {
            println!("{e}");
            Ok(())
        }
        (Ok(true), false) => {
            println!("MCP server `memdusa` already registered (check with `claude mcp get memdusa`)");
            Ok(())
        }
        (Ok(false), true) => Ok(()),
        _ => {
            let status = Command::new("claude").args(&args).status()?;
            if !status.success() {
                bail!("`{shown}` failed");
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_is_idempotent_and_preserves_other_settings() {
        let existing = json!({
            "model": "opus",
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "say done"}]}],
                "SessionStart": [{"hooks": [{"type": "command", "command": "/old/path/memdusa hook session-start"}]}]
            }
        });
        let once = merge_hooks(existing, "/usr/local/bin/memdusa", false).unwrap();
        let twice = merge_hooks(once.clone(), "/usr/local/bin/memdusa", false).unwrap();
        assert_eq!(once, twice, "re-installing must not duplicate hooks");

        assert_eq!(once["model"], "opus");
        let stop = once["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "user's own Stop hook is kept");
        assert_eq!(stop[0]["hooks"][0]["command"], "say done");
        let ss = once["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(ss.len(), 1, "old memdusa hook replaced, not duplicated");
        assert_eq!(ss[0]["hooks"][0]["command"], "/usr/local/bin/memdusa hook session-start");
        assert_eq!(once["hooks"]["PostToolUse"][0]["matcher"], crate::hooks::JOURNALED_TOOLS);
        assert_eq!(once["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 10);

        let removed = merge_hooks(once, "/usr/local/bin/memdusa", true).unwrap();
        assert_eq!(
            removed,
            json!({"model": "opus", "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}})
        );
    }

    #[test]
    fn quotes_paths_with_spaces() {
        let s = merge_hooks(json!({}), "/Users/me/my tools/memdusa", false).unwrap();
        let cmd = s["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(cmd, "'/Users/me/my tools/memdusa' hook stop");
        // ...and quoted commands are still recognized as ours
        assert!(merge_hooks(s, "/x/memdusa", true).unwrap().get("hooks").is_none());
    }
}
