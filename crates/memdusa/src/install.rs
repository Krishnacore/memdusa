//! `memdusa install <harness>`: registers the MCP server and merges memdusa's
//! hooks into the harness's own config files, idempotently and without
//! touching anything else in them. `memdusa uninstall <harness>` reverses it.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};

use crate::adapters::Harness;
use crate::config::home_dir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Scope {
    /// Your user config: every project on this machine.
    User,
    /// This project's config, committed and shared with the team.
    Project,
    /// This project, just you (Claude Code only: .claude/settings.local.json).
    Local,
}

pub struct InstallOptions {
    pub harness: Harness,
    pub scope: Scope,
    /// Override the hooks/settings file (mostly for testing).
    pub settings: Option<PathBuf>,
    /// Command used to invoke memdusa. Default: absolute path of this binary.
    pub bin: Option<String>,
    pub mcp: bool,
    pub dry_run: bool,
    pub uninstall: bool,
}

/// Where a harness keeps its hooks (and, for some, MCP servers).
fn hooks_path(h: Harness, scope: Scope) -> anyhow::Result<PathBuf> {
    let home = || home_dir();
    Ok(match (h, scope) {
        (Harness::ClaudeCode, Scope::User) => home()?.join(".claude/settings.json"),
        (Harness::ClaudeCode, Scope::Project) => ".claude/settings.json".into(),
        (Harness::ClaudeCode, Scope::Local) => ".claude/settings.local.json".into(),
        (Harness::Codex, Scope::User) => home()?.join(".codex/hooks.json"),
        (Harness::Codex, _) => ".codex/hooks.json".into(),
        (Harness::Cursor, Scope::User) => home()?.join(".cursor/hooks.json"),
        (Harness::Cursor, _) => ".cursor/hooks.json".into(),
        (Harness::Gemini, Scope::User) => home()?.join(".gemini/settings.json"),
        (Harness::Gemini, _) => ".gemini/settings.json".into(),
        (Harness::Antigravity, Scope::User) => home()?.join(".gemini/config/hooks.json"),
        (Harness::Antigravity, _) => ".agents/hooks.json".into(),
    })
}

/// JSON file holding `mcpServers` (Claude Code and Codex are handled by their CLIs / TOML).
fn mcp_json_path(h: Harness, scope: Scope) -> anyhow::Result<Option<PathBuf>> {
    let user = scope == Scope::User;
    Ok(match h {
        Harness::Cursor => Some(if user { home_dir()?.join(".cursor/mcp.json") } else { ".cursor/mcp.json".into() }),
        Harness::Gemini => Some(hooks_path(h, scope)?), // same settings.json
        Harness::Antigravity => Some(if user {
            home_dir()?.join(".gemini/config/mcp_config.json")
        } else {
            ".agents/mcp_config.json".into()
        }),
        Harness::ClaudeCode | Harness::Codex => None,
    })
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

fn hook_command(bin: &str, h: Harness, event: crate::hooks::HookEvent) -> String {
    match h {
        Harness::ClaudeCode => format!("{} hook {}", shell_quote(bin), event.arg()),
        _ => format!("{} hook {} --harness {}", shell_quote(bin), event.arg(), h.agent()),
    }
}

fn is_ours(v: &Value) -> bool {
    v.get("command").and_then(Value::as_str).is_some_and(is_memdusa_hook)
}

/// Remove our previous hook entries from `settings`, then (unless
/// uninstalling) add the current set. Everything else is preserved.
pub fn merge_hooks(mut settings: Value, h: Harness, bin: &str, uninstall: bool) -> anyhow::Result<Value> {
    let root = settings.as_object_mut().context("config file is not a JSON object")?;

    // Antigravity: one named group per tool, which we own entirely.
    if h == Harness::Antigravity {
        root.remove("memdusa");
        if !uninstall {
            let mut group = Map::new();
            group.insert("enabled".into(), json!(true));
            for nh in h.events() {
                let handler =
                    json!({ "type": "command", "command": hook_command(bin, h, nh.event), "timeout": nh.timeout_secs });
                let entry = match nh.matcher {
                    Some(m) => json!([{ "matcher": m, "hooks": [handler] }]),
                    None => json!([handler]),
                };
                group.insert(nh.native.into(), entry);
            }
            root.insert("memdusa".into(), group.into());
        }
        return Ok(settings);
    }

    if h == Harness::Cursor {
        root.entry("version").or_insert(json!(1));
    }
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().context("`hooks` is not a JSON object")?;

    // strip existing memdusa hooks everywhere
    for entries in hooks.values_mut() {
        let Some(entries) = entries.as_array_mut() else { continue };
        if h == Harness::Cursor {
            entries.retain(|e| !is_ours(e)); // Cursor: handlers sit directly in the event list
        } else {
            for g in entries.iter_mut() {
                if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                    hs.retain(|h| !is_ours(h));
                }
            }
            entries.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|hs| !hs.is_empty()));
        }
    }
    hooks.retain(|_, entries| entries.as_array().is_none_or(|e| !e.is_empty()));

    if !uninstall {
        for nh in h.events() {
            // Gemini CLI timeouts are in milliseconds, everyone else's in seconds
            let timeout = if h == Harness::Gemini { nh.timeout_secs * 1000 } else { nh.timeout_secs };
            let handler = json!({ "type": "command", "command": hook_command(bin, h, nh.event), "timeout": timeout });
            let entry = if h == Harness::Cursor {
                json!({ "command": hook_command(bin, h, nh.event), "timeout": nh.timeout_secs })
            } else {
                let mut group = Map::new();
                if let Some(m) = nh.matcher {
                    group.insert("matcher".into(), json!(m));
                }
                group.insert("hooks".into(), json!([handler]));
                group.into()
            };
            hooks
                .entry(nh.native)
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .with_context(|| format!("hooks.{} is not an array", nh.native))?
                .push(entry);
        }
    }
    if hooks.is_empty() {
        root.remove("hooks");
        if h == Harness::Cursor {
            root.remove("version");
        }
    }
    Ok(settings)
}

/// Add (or remove) `mcpServers.memdusa` in a JSON config.
pub fn merge_mcp_json(mut config: Value, h: Harness, bin: &str, uninstall: bool) -> anyhow::Result<Value> {
    let root = config.as_object_mut().context("config file is not a JSON object")?;
    let servers = root.entry("mcpServers").or_insert_with(|| json!({}));
    let servers = servers.as_object_mut().context("`mcpServers` is not a JSON object")?;
    servers.remove("memdusa");
    if !uninstall {
        let mut entry = json!({ "command": bin, "args": ["mcp", "--agent", h.agent()] });
        if h == Harness::Cursor {
            entry["type"] = json!("stdio");
        }
        servers.insert("memdusa".into(), entry);
    }
    if servers.is_empty() {
        root.remove("mcpServers");
    }
    Ok(config)
}

/// Add (or remove) the `[mcp_servers.memdusa]` table in Codex's config.toml.
pub fn merge_codex_toml(text: &str, bin: &str, uninstall: bool) -> String {
    const HEADER: &str = "[mcp_servers.memdusa]";
    // drop an existing table: from its header to the next table header
    let mut out: Vec<&str> = vec![];
    let mut skipping = false;
    for line in text.lines() {
        let t = line.trim();
        if t == HEADER || t.starts_with("[mcp_servers.memdusa.") {
            skipping = true;
            continue;
        }
        if skipping && t.starts_with('[') {
            skipping = false;
        }
        if !skipping {
            out.push(line);
        }
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    let mut s = out.join("\n");
    if !uninstall {
        let quote = |v: &str| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""));
        if !s.is_empty() {
            s.push_str("\n\n");
        }
        s.push_str(&format!("{HEADER}\ncommand = {}\nargs = [\"mcp\", \"--agent\", \"codex\"]", quote(bin)));
    }
    if !s.is_empty() {
        s.push('\n');
    }
    s
}

fn read_json(path: &Path) -> anyhow::Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).with_context(|| format!("{} is not valid JSON; fix it first", path.display()))
        }
        Ok(_) => Ok(json!({})),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write `text` to `path` (with a backup of the old file), or show it in a dry run.
fn apply(path: &Path, old: &str, text: &str, what: &str, opts: &InstallOptions) -> anyhow::Result<()> {
    if opts.dry_run {
        println!("# {} (dry run)\n{text}", path.display());
    } else if text == old {
        println!("{what} already up to date in {}", path.display());
    } else {
        write_with_backup(path, text)?;
        let verb = if opts.uninstall { "removed" } else { "installed" };
        println!("{verb} memdusa {what} in {}", path.display());
    }
    Ok(())
}

fn json_text(v: &Value) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(v)? + "\n")
}

pub fn run(opts: InstallOptions) -> anyhow::Result<()> {
    let h = opts.harness;
    if opts.scope == Scope::Local && h != Harness::ClaudeCode {
        bail!("--scope local is only for Claude Code; use --scope project");
    }
    let bin = match &opts.bin {
        Some(b) => b.clone(),
        None => std::env::current_exe()?.canonicalize()?.display().to_string(),
    };

    // hooks
    let path = match &opts.settings {
        Some(p) => p.clone(),
        None => hooks_path(h, opts.scope)?,
    };
    let current = read_json(&path)?;
    let old = if path.exists() { std::fs::read_to_string(&path)? } else { String::new() };
    let mut updated = merge_hooks(current, h, &bin, opts.uninstall)?;
    let mcp_same_file = opts.mcp && h == Harness::Gemini && opts.settings.is_none();
    if mcp_same_file {
        updated = merge_mcp_json(updated, h, &bin, opts.uninstall)?;
    }
    apply(&path, &old, &json_text(&updated)?, if mcp_same_file { "hooks and MCP server" } else { "hooks" }, &opts)?;

    // MCP server
    if opts.mcp && !mcp_same_file {
        match h {
            Harness::ClaudeCode => mcp_register_claude(&bin, opts.scope, opts.dry_run, opts.uninstall)?,
            Harness::Codex => {
                let p = if opts.scope == Scope::User {
                    home_dir()?.join(".codex/config.toml")
                } else {
                    ".codex/config.toml".into()
                };
                let old = std::fs::read_to_string(&p).unwrap_or_default();
                apply(&p, &old, &merge_codex_toml(&old, &bin, opts.uninstall), "MCP server", &opts)?;
            }
            _ => {
                if let Some(p) = mcp_json_path(h, opts.scope)? {
                    let old = if p.exists() { std::fs::read_to_string(&p)? } else { String::new() };
                    let merged = merge_mcp_json(read_json(&p)?, h, &bin, opts.uninstall)?;
                    apply(&p, &old, &json_text(&merged)?, "MCP server", &opts)?;
                }
            }
        }
    }

    if !opts.uninstall && !opts.dry_run {
        for note in notes(h)? {
            println!("\nnote: {note}");
        }
        println!("\nDone. Start a new session: it will load the resume brief automatically.");
    }
    Ok(())
}

/// Things the user must know or do after installing.
fn notes(h: Harness) -> anyhow::Result<Vec<String>> {
    let mut n = vec![];
    match h {
        Harness::Codex => n.push(
            "Codex runs new or changed hooks only after you trust them: open Codex and approve them in /hooks \
             (automation: --dangerously-bypass-hook-trust)."
                .into(),
        ),
        Harness::Cursor => {
            // Cursor also runs Claude Code hooks ("Third-Party Imports"), so they could fire twice.
            let claude = [
                home_dir()?.join(".claude/settings.json"),
                ".claude/settings.json".into(),
                ".claude/settings.local.json".into(),
            ];
            if claude.iter().any(|p| std::fs::read_to_string(p).is_ok_and(|s| s.contains(" hook session-start"))) {
                n.push(
                    "memdusa's Claude Code hooks are installed too, and Cursor runs Claude Code hooks by default \
                     (Settings > Third-Party Imports). Turn that off, or uninstall one set, so events aren't recorded twice."
                        .into(),
                );
            }
        }
        Harness::Antigravity => n.push(
            "Antigravity has no session-end or compaction hooks, so memdusa can't write fallback checkpoints there; \
             the stop hook asks the agent to checkpoint instead."
                .into(),
        ),
        Harness::Gemini => n.push(
            "Gemini CLI was replaced by Antigravity CLI for consumer accounts in June 2026; if you're on Antigravity, \
             run `memdusa install antigravity`."
                .into(),
        ),
        Harness::ClaudeCode => {}
    }
    Ok(n)
}

fn write_with_backup(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("txt");
    if path.exists() {
        let backup = path.with_extension(format!("{ext}.memdusa-backup"));
        std::fs::copy(path, &backup)?;
        println!("backup: {}", backup.display());
    }
    // write-then-rename so a crash never leaves a half-written config file
    let tmp = path.with_extension(format!("{ext}.memdusa-tmp"));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn mcp_register_claude(bin: &str, scope: Scope, dry_run: bool, uninstall: bool) -> anyhow::Result<()> {
    let scope = match scope {
        Scope::User => "user",
        Scope::Project => "project",
        Scope::Local => "local",
    };
    let args: Vec<String> = if uninstall {
        ["mcp", "remove", "memdusa", "--scope", scope].map(String::from).to_vec()
    } else {
        ["mcp", "add", "memdusa", "--scope", scope, "--", bin, "mcp", "--agent", "claude-code"]
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

    const BIN: &str = "/usr/local/bin/memdusa";

    #[test]
    fn claude_merge_is_idempotent_and_preserves_other_settings() {
        let existing = json!({
            "model": "opus",
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "say done"}]}],
                "SessionStart": [{"hooks": [{"type": "command", "command": "/old/path/memdusa hook session-start"}]}]
            }
        });
        let once = merge_hooks(existing, Harness::ClaudeCode, BIN, false).unwrap();
        let twice = merge_hooks(once.clone(), Harness::ClaudeCode, BIN, false).unwrap();
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

        let removed = merge_hooks(once, Harness::ClaudeCode, BIN, true).unwrap();
        assert_eq!(
            removed,
            json!({"model": "opus", "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}})
        );
    }

    #[test]
    fn quotes_paths_with_spaces() {
        let s = merge_hooks(json!({}), Harness::ClaudeCode, "/Users/me/my tools/memdusa", false).unwrap();
        let cmd = s["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(cmd, "'/Users/me/my tools/memdusa' hook stop");
        assert!(merge_hooks(s, Harness::ClaudeCode, "/x/memdusa", true).unwrap().get("hooks").is_none());
    }

    #[test]
    fn harness_specific_shapes() {
        // Codex: Claude-like groups, its own matcher and the 3s SessionEnd cap
        let codex = merge_hooks(json!({}), Harness::Codex, BIN, false).unwrap();
        assert_eq!(codex["hooks"]["PostToolUse"][0]["matcher"], "Bash|apply_patch");
        assert_eq!(codex["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 3);
        assert_eq!(codex["hooks"]["Stop"][0]["hooks"][0]["command"], format!("{BIN} hook stop --harness codex"));

        // Gemini: native event names, millisecond timeouts
        let gem = merge_hooks(json!({"theme": "dark"}), Harness::Gemini, BIN, false).unwrap();
        assert_eq!(
            gem["hooks"]["BeforeAgent"][0]["hooks"][0]["command"],
            format!("{BIN} hook user-prompt-submit --harness gemini")
        );
        assert_eq!(gem["hooks"]["SessionStart"][0]["hooks"][0]["timeout"], 10_000);
        assert_eq!(gem["theme"], "dark");

        // Cursor: version 1, handlers directly in the event list, two native events for tool use
        let mine = json!({"command": "./audit.sh"});
        let cur =
            merge_hooks(json!({"hooks": {"afterFileEdit": [mine.clone()]}}), Harness::Cursor, BIN, false).unwrap();
        assert_eq!(cur["version"], 1);
        assert_eq!(cur["hooks"]["afterFileEdit"].as_array().unwrap().len(), 2);
        assert_eq!(
            cur["hooks"]["afterShellExecution"][0]["command"],
            format!("{BIN} hook post-tool-use --harness cursor")
        );
        let back = merge_hooks(cur, Harness::Cursor, BIN, true).unwrap();
        assert_eq!(back, json!({"version": 1, "hooks": {"afterFileEdit": [mine]}}));

        // Antigravity: one named group we own
        let agy = merge_hooks(json!({"other": {"enabled": true}}), Harness::Antigravity, BIN, false).unwrap();
        assert_eq!(agy["memdusa"]["enabled"], true);
        assert_eq!(
            agy["memdusa"]["SessionStart"][0]["command"],
            format!("{BIN} hook session-start --harness antigravity")
        );
        assert!(agy["memdusa"]["PostToolUse"][0]["matcher"].as_str().unwrap().contains("run_command"));
        assert_eq!(merge_hooks(agy, Harness::Antigravity, BIN, true).unwrap(), json!({"other": {"enabled": true}}));
    }

    #[test]
    fn mcp_configs() {
        let cur = merge_mcp_json(json!({"mcpServers": {"x": {}}}), Harness::Cursor, BIN, false).unwrap();
        assert_eq!(
            cur["mcpServers"]["memdusa"],
            json!({"type": "stdio", "command": BIN, "args": ["mcp", "--agent", "cursor"]})
        );
        assert_eq!(merge_mcp_json(cur, Harness::Cursor, BIN, true).unwrap(), json!({"mcpServers": {"x": {}}}));

        let toml = "model = \"o5\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
        let added = merge_codex_toml(toml, "/opt/my \"bin\"/memdusa", false);
        assert!(added.ends_with("[mcp_servers.memdusa]\ncommand = \"/opt/my \\\"bin\\\"/memdusa\"\nargs = [\"mcp\", \"--agent\", \"codex\"]\n"), "{added}");
        assert_eq!(merge_codex_toml(&added, "/opt/my \"bin\"/memdusa", false), added, "idempotent");
        assert_eq!(merge_codex_toml(&added, BIN, true), toml, "uninstall restores the original");
        let between =
            "[mcp_servers.memdusa]\ncommand = \"old\"\n[mcp_servers.memdusa.env]\nA = \"1\"\n[profile]\nx = 1\n";
        assert_eq!(
            merge_codex_toml(between, BIN, true),
            "[profile]\nx = 1\n",
            "removes nested tables, keeps later ones"
        );
    }
}
