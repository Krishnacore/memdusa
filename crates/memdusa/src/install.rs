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
    /// Database URL given explicitly at install time (flag or `MEMDUSA_DATABASE_URL`);
    /// baked into every generated hook and MCP command.
    pub database_url: Option<String>,
    pub mcp: bool,
    pub dry_run: bool,
    pub uninstall: bool,
}

/// Git's native hook names, installed by `memdusa install git`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum GitHookKind {
    #[value(name = "post-commit")]
    Commit,
    #[value(name = "post-checkout")]
    Checkout,
    #[value(name = "post-merge")]
    Merge,
}

impl GitHookKind {
    pub const ALL: [Self; 3] = [Self::Commit, Self::Checkout, Self::Merge];

    /// Native hook file name, also the `memdusa git-hook` argument.
    pub fn name(self) -> &'static str {
        match self {
            Self::Commit => "post-commit",
            Self::Checkout => "post-checkout",
            Self::Merge => "post-merge",
        }
    }

    /// `git_event` value recorded in the journal.
    pub fn event(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Checkout => "checkout",
            Self::Merge => "merge",
        }
    }
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
        (Harness::OpenCode | Harness::Pi | Harness::Hermes | Harness::Git, _) => {
            bail!("{} has no JSON hooks file", h.agent())
        }
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
        Harness::Pi => Some(if user { home_dir()?.join(".pi/agent/mcp.json") } else { ".pi/mcp.json".into() }),
        Harness::ClaudeCode | Harness::Codex | Harness::OpenCode | Harness::Hermes | Harness::Git => None,
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

/// ` --database-url <url>` (shell-quoted) when the URL was given explicitly at install time.
fn db_flag(db: Option<&str>) -> String {
    db.map(|u| format!(" --database-url {}", shell_quote(u))).unwrap_or_default()
}

/// `["--database-url", url]` for argument-list configs (MCP entries), else nothing.
fn db_args(db: Option<&str>) -> Vec<String> {
    db.map(|u| vec!["--database-url".to_owned(), u.to_owned()]).unwrap_or_default()
}

/// Cursor silently ignores its whole `.cursor/hooks.json` when any command contains `://` (verified with
/// cursor-agent, November 2026), so SQLite URLs are written in the equivalent `sqlite:<path>` form there.
/// Other URLs can't avoid `://`: the installer warns instead (see [`cursor_db_note`]).
fn cursor_safe_db(db: &str) -> String {
    db.strip_prefix("sqlite://").map_or_else(|| db.to_owned(), |path| format!("sqlite:{path}"))
}

/// The warning for a Cursor install whose database URL still contains `://` after [`cursor_safe_db`].
fn cursor_db_note(db: Option<&str>) -> Option<String> {
    db.map(cursor_safe_db).filter(|u| u.contains("://")).map(|_| {
        "Cursor ignores every hook in .cursor/hooks.json when a command contains `://`, so the database URL can't \
         be written there: use `memdusa login <server>` (stored in ~/.memdusa) or a SQLite database instead."
            .to_owned()
    })
}

pub(crate) fn hook_command(bin: &str, h: Harness, event: crate::hooks::HookEvent, db: Option<&str>) -> String {
    let cursor_db = (h == Harness::Cursor).then(|| db.map(cursor_safe_db)).flatten();
    let flag = db_flag(cursor_db.as_deref().or(db));
    match h {
        Harness::ClaudeCode => format!("{} hook {}{flag}", shell_quote(bin), event.arg()),
        _ => format!("{} hook {} --harness {}{flag}", shell_quote(bin), event.arg(), h.agent()),
    }
}

fn is_ours(v: &Value) -> bool {
    v.get("command").and_then(Value::as_str).is_some_and(is_memdusa_hook)
}

/// Remove our previous hook entries from `settings`, then (unless
/// uninstalling) add the current set. Everything else is preserved.
pub fn merge_hooks(
    mut settings: Value,
    h: Harness,
    bin: &str,
    db: Option<&str>,
    uninstall: bool,
) -> anyhow::Result<Value> {
    let root = settings.as_object_mut().context("config file is not a JSON object")?;

    // Antigravity: one named group per tool, which we own entirely.
    if h == Harness::Antigravity {
        root.remove("memdusa");
        if !uninstall {
            let mut group = Map::new();
            group.insert("enabled".into(), json!(true));
            for nh in h.events() {
                let handler = json!({ "type": "command", "command": hook_command(bin, h, nh.event, db), "timeout": nh.timeout_secs });
                let entry = match nh.matcher {
                    Some(m) => json!([{ "matcher": m, "hooks": [handler] }]),
                    None => json!([handler]),
                };
                group.insert(nh.native.into(), entry);
            }
            // `agy -p` soft-denies MCP calls it cannot ask approval for. A PreToolUse hook can
            // auto-approve just memdusa's server; the handler checks ServerName before doing so.
            let approve = hook_command(bin, h, crate::hooks::HookEvent::McpApproval, db);
            group.insert(
                "PreToolUse".into(),
                json!([{
                    "matcher": "call_mcp_tool|mcp_memdusa_.*",
                    "hooks": [{ "type": "command", "command": approve, "timeout": 5 }]
                }]),
            );
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
            let command = hook_command(bin, h, nh.event, db);
            let handler = json!({ "type": "command", "command": command, "timeout": timeout });
            let entry = if h == Harness::Cursor {
                json!({ "command": command, "timeout": nh.timeout_secs })
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

/// Cursor CLI reads permissions from `<project>/.cursor/cli.json` (project scope) or
/// `~/.cursor/cli-config.json` (user scope); only permissions are valid in the project file. In print mode an MCP
/// tool that isn't allowed is rejected, because there is no interactive approver (`--approve-mcps` only approves the
/// *server*). `Mcp(server:tool)` gates tools by server, so this one narrow entry auto-approves memdusa's own tools
/// without widening shell, file, web or any other server's MCP permissions.
const CURSOR_MCP_PERMISSION: &str = "Mcp(memdusa:*)";

/// Cursor CLI's permission file for the given scope (the global one holds all settings, the project one only
/// permissions).
fn cursor_permissions_path(scope: Scope) -> anyhow::Result<PathBuf> {
    Ok(match scope {
        Scope::User => home_dir()?.join(".cursor/cli-config.json"),
        _ => ".cursor/cli.json".into(),
    })
}

/// Add (or remove) memdusa's narrow MCP-tool permission in Cursor's CLI config. Every other key — including the
/// user's other `permissions.allow` / `deny` entries — is preserved verbatim, and re-installing doesn't duplicate the
/// entry. Uninstall removes exactly this entry (and the containers only if they'd otherwise be empty).
pub fn merge_cursor_permissions(mut config: Value, uninstall: bool) -> anyhow::Result<Value> {
    let root = config.as_object_mut().context("config file is not a JSON object")?;
    let permissions = root.entry("permissions").or_insert_with(|| json!({}));
    let perms = permissions.as_object_mut().context("`permissions` is not a JSON object")?;
    // Cursor validates both arrays even in the project-scoped file (`permissions.deny` is required), so create
    // them together on install and take them back together on uninstall.
    perms.entry("deny").or_insert_with(|| json!([]));
    let allow = perms.entry("allow").or_insert_with(|| json!([]));
    let allow = allow.as_array_mut().context("`permissions.allow` is not an array")?;
    // Keep the first copy in place (so a user's ordering never changes), drop any stale duplicates, drop all copies
    // on uninstall, and only add ours when installing and it isn't there yet.
    let mut seen = false;
    allow.retain(|v| {
        if v.as_str() == Some(CURSOR_MCP_PERMISSION) {
            if uninstall || seen {
                false
            } else {
                seen = true;
                true
            }
        } else {
            true
        }
    });
    if !uninstall && !seen {
        allow.push(json!(CURSOR_MCP_PERMISSION));
    }
    let allow_removed = allow.is_empty();
    if allow_removed {
        perms.remove("allow");
    }
    // Only take back the empty `deny` we created alongside an empty `allow`; never touch a user's own entries.
    if uninstall && allow_removed && perms.get("deny").and_then(Value::as_array).is_some_and(|d| d.is_empty()) {
        perms.remove("deny");
    }
    if perms.is_empty() {
        root.remove("permissions");
    }
    Ok(config)
}

/// Add (or remove) `mcpServers.memdusa` in a JSON config.
pub fn merge_mcp_json(
    mut config: Value,
    h: Harness,
    bin: &str,
    db: Option<&str>,
    uninstall: bool,
) -> anyhow::Result<Value> {
    let root = config.as_object_mut().context("config file is not a JSON object")?;
    let servers = root.entry("mcpServers").or_insert_with(|| json!({}));
    let servers = servers.as_object_mut().context("`mcpServers` is not a JSON object")?;
    servers.remove("memdusa");
    if !uninstall {
        let mut args = vec!["mcp".to_owned(), "--agent".to_owned(), h.agent().to_owned()];
        args.extend(db_args(db));
        let mut entry = json!({ "command": bin, "args": args });
        if h == Harness::Cursor {
            entry["type"] = json!("stdio");
        }
        if h == Harness::Pi {
            entry["exposure"] = json!("direct"); // pi's default "codemode" hides tools from the model
        }
        servers.insert("memdusa".into(), entry);
    }
    if servers.is_empty() {
        root.remove("mcpServers");
    }
    Ok(config)
}

/// Env vars forwarded to memdusa's Codex MCP server. Unlike hooks, Codex does not pass the parent environment to
/// stdio MCP servers, so the coordination variables have to be named explicitly. Deliberately only these four:
/// `MEMDUSA_DATABASE_URL` is already baked into `args` when given, and `MEMDUSA_HOME` is not needed.
const CODEX_MCP_ENV_VARS: [&str; 4] = ["MEMDUSA_RUN", "MEMDUSA_TASK", "MEMDUSA_PARENT", "MEMDUSA_STEP_LEASE_SECS"];

/// Keys in `[mcp_servers.memdusa]` that the installer owns. Any other key the user put in that table is left
/// exactly as written.
const CODEX_OWNED_KEYS: [&str; 4] = ["command", "args", "default_tools_approval_mode", "env_vars"];

fn codex_quote(v: &str) -> String {
    format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Canonical TOML for a single memdusa-owned key.
///
/// `default_tools_approval_mode = "approve"` is needed because `codex exec` denies MCP tool calls that require
/// approval (there is no interactive user to ask). Auto-approval is reasonable here: memdusa's tools only write to
/// memdusa's own store, so approving them grants no access to arbitrary files or commands.
fn codex_owned_lines(key: &str, bin: &str, db: Option<&str>) -> Vec<String> {
    match key {
        "command" => vec![format!("command = {}", codex_quote(bin))],
        "args" => {
            let extra: String = db_args(db).iter().map(|a| format!(", {}", codex_quote(a))).collect();
            vec![format!("args = [\"mcp\", \"--agent\", \"codex\"{extra}]")]
        }
        "default_tools_approval_mode" => vec!["default_tools_approval_mode = \"approve\"".to_owned()],
        "env_vars" => vec![format!(
            "env_vars = [{}]",
            CODEX_MCP_ENV_VARS.iter().map(|v| codex_quote(v)).collect::<Vec<_>>().join(", ")
        )],
        _ => unreachable!("unknown memdusa-owned Codex key: {key}"),
    }
}

/// Split a table body into statements, preserving each statement's original lines verbatim. A statement can span
/// several lines (a user's multi-line array), so re-installing does not fold or rewrite it.
fn toml_statements(lines: &[&str]) -> Vec<Vec<String>> {
    let delta = |line: &str| {
        line.chars().fold(0, |d, c| match c {
            '[' => d + 1,
            ']' => d - 1,
            _ => d,
        })
    };
    let mut out: Vec<Vec<String>> = vec![];
    let mut cur: Vec<String> = vec![];
    let mut depth = 0;
    for &line in lines {
        cur.push(line.to_owned());
        let t = line.trim();
        if t.starts_with('#') || t.is_empty() {
            if depth == 0 {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        depth += delta(t);
        if depth <= 0 {
            depth = 0;
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn toml_statement_key(stmt: &[String]) -> Option<&str> {
    let t = stmt.first()?.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    t.split_once('=').map(|(k, _)| k.trim())
}

/// Rebuild the memdusa table body: refresh memdusa's keys in place (appending any that were missing), keep every
/// other line — user keys, comments, blank lines — as written.
fn codex_table_body(body: &[&str], bin: &str, db: Option<&str>) -> Vec<String> {
    let mut out = vec![];
    let mut written = [false; CODEX_OWNED_KEYS.len()];
    for stmt in toml_statements(body) {
        let owned = toml_statement_key(&stmt).and_then(|key| CODEX_OWNED_KEYS.iter().position(|k| *k == key));
        if let Some(i) = owned {
            out.extend(codex_owned_lines(CODEX_OWNED_KEYS[i], bin, db));
            written[i] = true;
        } else {
            out.extend(stmt);
        }
    }
    for (i, &key) in CODEX_OWNED_KEYS.iter().enumerate() {
        if !written[i] {
            out.extend(codex_owned_lines(key, bin, db));
        }
    }
    out
}

/// Add (or remove) the `[mcp_servers.memdusa]` table in Codex's config.toml.
pub fn merge_codex_toml(text: &str, bin: &str, db: Option<&str>, uninstall: bool) -> String {
    const HEADER: &str = "[mcp_servers.memdusa]";
    const NESTED: &str = "[mcp_servers.memdusa.";

    if uninstall {
        // Drop the table and any of its sub-tables; keep every other line, including later tables.
        let mut out: Vec<&str> = vec![];
        let mut skipping = false;
        for line in text.lines() {
            let t = line.trim();
            if t == HEADER || t.starts_with(NESTED) {
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
        let mut s = out.join("\n");
        while s.ends_with('\n') {
            s.pop();
        }
        if !s.is_empty() {
            s.push('\n');
        }
        return s;
    }

    // Install: replace the table in place, preserving the user's own keys and sub-tables around it.
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = vec![];
    let mut i = 0;
    let mut found = false;
    while i < lines.len() {
        if lines[i].trim() == HEADER {
            if found {
                // A duplicate table: drop it, its keys were folded into the first one.
                i += 1;
                while i < lines.len() && !lines[i].trim_start().starts_with('[') {
                    i += 1;
                }
                continue;
            }
            found = true;
            let start = i + 1;
            let mut end = start;
            while end < lines.len() && !lines[end].trim_start().starts_with('[') {
                end += 1;
            }
            out.push(HEADER.to_owned());
            out.extend(codex_table_body(&lines[start..end], bin, db));
            i = end;
            continue;
        }
        out.push(lines[i].to_owned());
        i += 1;
    }
    if !found {
        while out.last().is_some_and(|l| l.trim().is_empty()) {
            out.pop();
        }
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push(HEADER.to_owned());
        out.extend(codex_table_body(&[], bin, db));
    }
    let mut s = out.join("\n");
    if !s.is_empty() {
        s.push('\n');
    }
    s
}

/// Add or remove memdusa's line in one git hook script. Returns the new file
/// content, or `None` when the file should be deleted (uninstall left only a
/// shebang and blank lines). The user's other lines are never changed.
pub fn merge_git_hook(existing: &str, line: &str, uninstall: bool) -> Option<String> {
    let is_ours = |l: &str| l.trim_end().ends_with("# memdusa");
    let lines: Vec<&str> = existing.lines().collect();

    if uninstall {
        if lines.iter().all(|l| l.trim().is_empty()) {
            return None;
        }
        let kept: Vec<&str> = lines.into_iter().filter(|l| !is_ours(l)).collect();
        // Nothing left but a shebang (and blank lines): our line was the whole hook.
        if kept.iter().all(|l| l.trim().is_empty() || l.trim_start().starts_with("#!")) {
            return None;
        }
        return Some(join_hook_lines(&kept, existing));
    }

    if lines.iter().all(|l| l.trim().is_empty()) {
        return Some(format!("#!/bin/sh\n{line}\n"));
    }

    // Replace the first memdusa line (an older bin path) in place; drop any duplicates.
    if lines.iter().any(|l| is_ours(l)) {
        let mut replaced = false;
        let mut out: Vec<&str> = Vec::with_capacity(lines.len());
        for l in lines {
            if is_ours(l) {
                if !replaced {
                    out.push(line);
                    replaced = true;
                }
            } else {
                out.push(l);
            }
        }
        return Some(join_hook_lines(&out, existing));
    }

    // A trailing `exit` would skip anything after it, so insert our line before
    // the last meaningful line when that line is an exit statement. Otherwise
    // append as usual.
    let last_meaningful = lines.iter().rposition(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with('#')
    });
    let insert_at = match last_meaningful {
        Some(i) if lines[i].trim_start().starts_with("exit") => i,
        _ => lines.len(),
    };
    let mut out = lines;
    out.insert(insert_at, line);
    Some(join_hook_lines(&out, existing))
}

fn join_hook_lines(lines: &[&str], existing: &str) -> String {
    let mut text = lines.join("\n");
    if existing.ends_with('\n') || !text.is_empty() {
        text.push('\n');
    }
    text
}

/// The line `memdusa install git` adds to each hook. It can never fail the git
/// operation (`|| true`) and never prints (`>/dev/null 2>&1`).
fn git_hook_line(bin: &str, hook: &str) -> String {
    format!("{} git-hook {hook} \"$@\" >/dev/null 2>&1 || true  # memdusa", shell_quote(bin))
}

/// `git rev-parse --git-path hooks`: respects `core.hooksPath` (e.g. Husky).
fn git_hooks_dir() -> anyhow::Result<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-path", "hooks"])
        .output()
        .context("running git (is git installed?)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("not in a git repository: {}", err.trim());
    }
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if path.as_os_str().is_empty() {
        bail!("git did not report a hooks directory");
    }
    Ok(if path.is_absolute() { path } else { std::env::current_dir()?.join(path) })
}

fn make_executable(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Write a hook atomically (write-then-rename), without leaving a backup in
/// the hooks directory. Our merge only touches memdusa's own line.
fn write_hook(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("memdusa-tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
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
    if opts.scope == Scope::Local && !matches!(h, Harness::ClaudeCode | Harness::Git) {
        bail!("--scope local is only for Claude Code; use --scope project");
    }
    let bin = match &opts.bin {
        Some(b) => b.clone(),
        None => std::env::current_exe()?.canonicalize()?.display().to_string(),
    };
    let db = opts.database_url.as_deref().filter(|u| !u.is_empty());
    match h {
        Harness::OpenCode | Harness::Pi => return install_plugin(&opts, &bin),
        Harness::Hermes => return install_hermes(&opts, &bin),
        Harness::Git => return install_git_hooks(&opts, &bin),
        _ => {}
    }

    // hooks
    let path = match &opts.settings {
        Some(p) => p.clone(),
        None => hooks_path(h, opts.scope)?,
    };
    let current = read_json(&path)?;
    let old = if path.exists() { std::fs::read_to_string(&path)? } else { String::new() };
    let mut updated = merge_hooks(current, h, &bin, db, opts.uninstall)?;
    let mcp_same_file = opts.mcp && h == Harness::Gemini && opts.settings.is_none();
    if mcp_same_file {
        updated = merge_mcp_json(updated, h, &bin, db, opts.uninstall)?;
    }
    apply(&path, &old, &json_text(&updated)?, if mcp_same_file { "hooks and MCP server" } else { "hooks" }, &opts)?;

    // MCP server
    if opts.mcp && !mcp_same_file {
        match h {
            Harness::ClaudeCode => mcp_register_claude(&bin, db, opts.scope, opts.dry_run, opts.uninstall)?,
            Harness::Codex => {
                let p = if opts.scope == Scope::User {
                    home_dir()?.join(".codex/config.toml")
                } else {
                    ".codex/config.toml".into()
                };
                let old = std::fs::read_to_string(&p).unwrap_or_default();
                apply(&p, &old, &merge_codex_toml(&old, &bin, db, opts.uninstall), "MCP server", &opts)?;
            }
            _ => {
                if let Some(p) = mcp_json_path(h, opts.scope)? {
                    let old = if p.exists() { std::fs::read_to_string(&p)? } else { String::new() };
                    let merged = merge_mcp_json(read_json(&p)?, h, &bin, db, opts.uninstall)?;
                    apply(&p, &old, &json_text(&merged)?, "MCP server", &opts)?;
                }
            }
        }
    }

    // Cursor CLI's print mode rejects MCP tool calls it would have to ask about. Allow only memdusa's tools there;
    // shell/file/web policy and every other MCP server are left to the user's own configuration.
    if h == Harness::Cursor && opts.mcp {
        let p = cursor_permissions_path(opts.scope)?;
        if !p.exists() && opts.uninstall {
            // Nothing was ever added here; don't leave an empty file behind.
        } else {
            let old = if p.exists() { std::fs::read_to_string(&p)? } else { String::new() };
            let merged = merge_cursor_permissions(read_json(&p)?, opts.uninstall)?;
            apply(&p, &old, &json_text(&merged)?, "MCP tool permission", &opts)?;
        }
    }

    if !opts.uninstall && !opts.dry_run {
        let cursor_note = (h == Harness::Cursor).then(|| cursor_db_note(db)).flatten();
        for note in notes(h)?.into_iter().chain(cursor_note) {
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
            n.push(
                "`cursor-agent -p` (headless) only fires the sessionStart, shell/edit and sessionEnd hooks; memdusa \
                 reads the prompt and the agent's last message from Cursor's transcript at session end."
                    .into(),
            );
            n.push(
                "Headless `cursor-agent -p` rejects MCP calls it would have to ask about; memdusa adds \
                 `Mcp(memdusa:*)` to Cursor's CLI permissions so only its own tools run without approval. Shell \
                 commands still follow Cursor's own policy (`--force` or an allowlist for automation)."
                    .into(),
            );
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
        Harness::Antigravity => {
            n.push(
                "Antigravity has no session-end or compaction hooks. memdusa journals the agent's last words from the \
                 session transcript at Stop and writes one fallback checkpoint when the agent goes fully idle after \
                 editing files (at most one per session)."
                    .into(),
            );
            n.push(
                "Headless `agy -p` soft-denies MCP calls it can't ask approval for; memdusa installs a PreToolUse hook \
                 that auto-approves only its own MCP server, so `checkpoint` works without \
                 --dangerously-skip-permissions. MEMDUSA_RUN / MEMDUSA_TASK reach the MCP server from agy's environment."
                    .into(),
            );
        }
        Harness::Gemini => n.push(
            "Gemini CLI was replaced by Antigravity CLI for consumer accounts in June 2026; if you're on Antigravity, \
             run `memdusa install antigravity`."
                .into(),
        ),
        Harness::Pi => n.push(
            "pi loads project extensions (.pi/) only after you trust the project; user scope works everywhere.".into(),
        ),
        Harness::OpenCode => n.push(
            "Restart OpenCode to load the plugin. OpenCode has no session-end hook, so no session-end event or \
             fallback checkpoint is recorded there; the agent's last message is journaled when it goes idle."
                .into(),
        ),
        Harness::Hermes => n.push(
            "Hermes only runs shell hooks that are allowlisted: run Hermes once with `--accept-hooks` (or approve \
             them at its prompt). `hermes hooks doctor` shows their status. Headless runs never ask. Hermes' \
             gateway keeps MCP servers running across sessions: after installing or reinstalling, run \
             `hermes gateway restart` so it starts memdusa's MCP server with the new settings. The gateway also \
             runs every session's hooks, so a delegated Hermes worker can't take MEMDUSA_RUN / MEMDUSA_TASK from \
             its shell: put `delegate`'s `MEMDUSA_RUN=<id> MEMDUSA_TASK=<task>` line in the worker's prompt."
                .into(),
        ),
        Harness::Git | Harness::ClaudeCode => {}
    }
    Ok(n)
}

const OPENCODE_PLUGIN: &str = include_str!("../assets/opencode-plugin.ts");
const PI_EXTENSION: &str = include_str!("../assets/pi-extension.ts");

/// The plugin source with the memdusa binary baked in.
pub fn plugin_source(h: Harness, bin: &str, db: Option<&str>) -> String {
    let template = if h == Harness::OpenCode { OPENCODE_PLUGIN } else { PI_EXTENSION };
    // extra `spawn` args (after the fixed ones): empty unless a database URL was given
    let extra: String =
        db_args(db).iter().map(|a| format!(", {}", serde_json::to_string(a).expect("string serializes"))).collect();
    template
        .replace("__MEMDUSA_BIN__", &serde_json::to_string(bin).expect("string serializes"))
        .replace("__MEMDUSA_EXTRA_ARGS__", &extra)
}

/// OpenCode and pi: write the plugin/extension file and register the MCP server.
fn install_plugin(opts: &InstallOptions, bin: &str) -> anyhow::Result<()> {
    let h = opts.harness;
    let db = opts.database_url.as_deref().filter(|u| !u.is_empty());
    let user = opts.scope == Scope::User;
    let path: PathBuf = match &opts.settings {
        Some(p) => p.clone(),
        None => match (h, user) {
            (Harness::OpenCode, true) => home_dir()?.join(".config/opencode/plugins/memdusa/index.ts"),
            (Harness::OpenCode, false) => ".opencode/plugins/memdusa/index.ts".into(),
            (_, true) => home_dir()?.join(".pi/agent/extensions/memdusa.ts"),
            (_, false) => ".pi/extensions/memdusa.ts".into(),
        },
    };
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    if opts.uninstall {
        if opts.dry_run {
            println!("# would remove {}", path.display());
        } else if path.exists() {
            std::fs::remove_file(&path)?;
            if h == Harness::OpenCode
                && let Some(dir) = path.parent()
            {
                let _ = std::fs::remove_dir(dir); // only if empty
            }
            println!("removed {}", path.display());
        }
    } else {
        let what = if h == Harness::OpenCode { "plugin" } else { "extension" };
        apply(&path, &old, &plugin_source(h, bin, db), what, opts)?;
    }

    if opts.mcp {
        match h {
            Harness::OpenCode => opencode_mcp(bin, db, user, opts.dry_run, opts.uninstall)?,
            _ => {
                if let Some(p) = mcp_json_path(h, opts.scope)? {
                    let old = if p.exists() { std::fs::read_to_string(&p)? } else { String::new() };
                    let merged = merge_mcp_json(read_json(&p)?, h, bin, db, opts.uninstall)?;
                    apply(&p, &old, &json_text(&merged)?, "MCP server", opts)?;
                }
            }
        }
    }
    if !opts.uninstall && !opts.dry_run {
        let cursor_note = (h == Harness::Cursor).then(|| cursor_db_note(db)).flatten();
        for note in notes(h)?.into_iter().chain(cursor_note) {
            println!("\nnote: {note}");
        }
        println!("\nDone. Start a new session: it will load the resume brief automatically.");
    }
    Ok(())
}

fn opencode_mcp(bin: &str, db: Option<&str>, user: bool, dry_run: bool, uninstall: bool) -> anyhow::Result<()> {
    if uninstall {
        println!("OpenCode has no `mcp remove`: delete `memdusa` from `mcp.servers` in your opencode.json(c).");
        return Ok(());
    }
    let mut args: Vec<String> = vec!["mcp".into(), "add".into()];
    if user {
        args.push("--global".into());
    }
    args.extend(["memdusa", "--", bin, "mcp", "--agent", "opencode"].map(String::from));
    args.extend(db_args(db));
    let shown = format!("opencode {}", args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" "));
    if dry_run {
        println!("# would run: {shown}");
        return Ok(());
    }
    let listed = Command::new("opencode").args(["mcp", "list"]).output();
    match listed {
        Err(_) => println!("`opencode` CLI not found; register the MCP server with:\n  {shown}"),
        Ok(o) if String::from_utf8_lossy(&o.stdout).contains("memdusa") => {
            println!("MCP server `memdusa` already registered with OpenCode")
        }
        Ok(_) => {
            if !Command::new("opencode").args(&args).status()?.success() {
                bail!("`{shown}` failed");
            }
        }
    }
    Ok(())
}

/// macOS TCC guards the obvious user folders (Desktop, Documents, Downloads and
/// iCloud Drive). Hermes launches hooks from a long-lived background daemon, and
/// when that daemon `exec`s a binary from one of those folders dyld stalls in
/// `open()` on the executable until Hermes' hook timeout fires. A symlink is no
/// help: dyld resolves it and hits the same protected path. Stage a real copy in
/// memdusa's own (unprotected) home instead and point the hooks at that.
fn tcc_protected(path: &Path, home: &Path) -> bool {
    ["Desktop", "Documents", "Downloads"]
        .iter()
        .map(|name| home.join(name))
        .chain(std::iter::once(home.join("Library/Mobile Documents")))
        .any(|dir| path.starts_with(&dir))
}

/// Hermes runs hooks and MCP servers from a background daemon that macOS may not let into
/// Desktop/Documents/Downloads/iCloud Drive: a SQLite database there makes every hook stall.
fn hermes_db_note(db: Option<&str>, home: &Path) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let path = db?.strip_prefix("sqlite:")?.trim_start_matches("//");
    let path = path.split('?').next().unwrap_or(path);
    tcc_protected(Path::new(path), home).then(|| {
        format!(
            "the database {path} is in a folder macOS protects (Desktop, Documents, Downloads or iCloud Drive). \
             Hermes runs hooks from a background service that may not be allowed to open it, so hooks would \
             time out: keep the database elsewhere (the default ~/.memdusa/memdusa.db is fine)."
        )
    })
}

fn hermes_hook_bin(opts: &InstallOptions, bin: &str) -> anyhow::Result<String> {
    if !cfg!(target_os = "macos") || opts.uninstall {
        return Ok(bin.to_owned());
    }
    let Ok(source) = Path::new(bin).canonicalize() else {
        return Ok(bin.to_owned()); // a bare command (e.g. `memdusa`) has nothing to copy
    };
    if !source.is_file() || !tcc_protected(&source, &home_dir()?) {
        return Ok(bin.to_owned());
    }
    // Stage next to memdusa's own data, unless that is itself in a protected folder (e.g. a
    // MEMDUSA_HOME under ~/Desktop, as the eval uses): then the copy would stall just the same, so use
    // ~/.memdusa/bin, which macOS doesn't guard.
    let home = home_dir()?;
    let data_dir = crate::config::dir()?;
    let staged_dir = if tcc_protected(&data_dir, &home) { home.join(".memdusa") } else { data_dir };
    let staged = staged_dir.join("bin").join("memdusa");
    if !opts.dry_run {
        if let Some(parent) = staged.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let fresh = match (std::fs::metadata(&staged).ok(), std::fs::metadata(&source).ok()) {
            (Some(dst), Some(src)) => {
                dst.len() != src.len()
                    || match (dst.modified().ok(), src.modified().ok()) {
                        (Some(dm), Some(sm)) => dm < sm,
                        _ => true,
                    }
            }
            _ => true,
        };
        if fresh {
            // Copy to a temp file and rename it into place: overwriting a signed binary in place makes
            // macOS kill the next process started from it (SIGKILL, exit 137).
            let tmp = staged.with_extension(format!("tmp.{}", std::process::id()));
            std::fs::copy(&source, &tmp)
                .with_context(|| format!("staging the Hermes hook binary at {}", tmp.display()))?;
            make_executable(&tmp)?;
            std::fs::rename(&tmp, &staged)
                .with_context(|| format!("staging the Hermes hook binary at {}", staged.display()))?;
        }
    }
    println!(
        "note: Hermes hooks use {} — macOS TCC can stall a background daemon that launches a binary from a protected folder (Desktop/Documents/Downloads)",
        staged.display()
    );
    Ok(staged.display().to_string())
}

fn install_hermes(opts: &InstallOptions, bin: &str) -> anyhow::Result<()> {
    if opts.scope != Scope::User {
        bail!("Hermes only has a user config (~/.hermes/config.yaml); use --scope user");
    }
    let bin = hermes_hook_bin(opts, bin)?;
    let path = match &opts.settings {
        Some(p) => p.clone(),
        None => std::env::var_os("HERMES_HOME")
            .map(PathBuf::from)
            .map_or_else(|| Ok(home_dir()?.join(".hermes")), Ok::<_, anyhow::Error>)?
            .join("config.yaml"),
    };
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let db = opts.database_url.as_deref().filter(|u| !u.is_empty());
    let text = crate::hermes_yaml::merge(&old, &bin, db, opts.mcp, opts.uninstall).map_err(|why| {
        anyhow::anyhow!(
            "{}: {why}; memdusa won't edit it. Rewrite it as a block mapping (or remove it) and re-run",
            path.display()
        )
    })?;
    apply(&path, &old, &text, "hooks", opts)?;
    if !opts.uninstall && !opts.dry_run {
        for note in notes(Harness::Hermes)? {
            println!("\nnote: {note}");
        }
        if let Some(note) = hermes_db_note(db, &home_dir()?) {
            println!("\nnote: {note}");
        }
        println!("\nDone. Start a new session: it will load the resume brief automatically.");
    }
    Ok(())
}

/// `memdusa install git` / `uninstall git`: merge our line into the repo's
/// `post-commit`, `post-checkout` and `post-merge` hooks.
fn install_git_hooks(opts: &InstallOptions, bin: &str) -> anyhow::Result<()> {
    let dir = git_hooks_dir()?;
    for hook in GitHookKind::ALL {
        let path = dir.join(hook.name());
        let old = std::fs::read_to_string(&path).unwrap_or_default();
        let line = git_hook_line(bin, hook.name());
        match merge_git_hook(&old, &line, opts.uninstall) {
            None => {
                if opts.dry_run {
                    println!("# would remove {}", path.display());
                } else if path.exists() {
                    std::fs::remove_file(&path)?;
                    println!("removed memdusa git hook {}", path.display());
                }
            }
            Some(text) if text == old => {
                if !opts.dry_run {
                    make_executable(&path)?;
                }
                println!("memdusa {} already up to date in {}", hook.name(), path.display());
            }
            Some(text) => {
                if opts.dry_run {
                    println!("# {} (dry run)\n{text}", path.display());
                } else {
                    write_hook(&path, &text)?;
                    make_executable(&path)?;
                    println!("installed memdusa {} hook in {}", hook.name(), path.display());
                }
            }
        }
    }
    if !opts.uninstall && !opts.dry_run {
        println!("\nDone. Commits, branch switches and merges in this repo will be recorded.");
    }
    Ok(())
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

fn mcp_register_claude(
    bin: &str,
    db: Option<&str>,
    scope: Scope,
    dry_run: bool,
    uninstall: bool,
) -> anyhow::Result<()> {
    let scope = match scope {
        Scope::User => "user",
        Scope::Project => "project",
        Scope::Local => "local",
    };
    let args: Vec<String> = if uninstall {
        ["mcp", "remove", "memdusa", "--scope", scope].map(String::from).to_vec()
    } else {
        let mut a = ["mcp", "add", "memdusa", "--scope", scope, "--", bin, "mcp", "--agent", "claude-code"]
            .map(String::from)
            .to_vec();
        a.extend(db_args(db));
        a
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
    fn hermes_db_note_flags_only_protected_sqlite_paths() {
        let home = Path::new("/Users/me");
        let protected = hermes_db_note(Some("sqlite:/Users/me/Desktop/proj/m.db"), home);
        let fine = hermes_db_note(Some("sqlite:/Users/me/.memdusa/memdusa.db"), home);
        let pg = hermes_db_note(Some("postgres://u@h/db"), home);
        if cfg!(target_os = "macos") {
            assert!(protected.is_some_and(|n| n.contains("protects")));
        } else {
            assert!(protected.is_none());
        }
        assert!(fine.is_none() && pg.is_none() && hermes_db_note(None, home).is_none());
    }

    #[test]
    fn tcc_protected_folders_are_only_the_usual_suspects() {
        let home = Path::new("/Users/someone");
        for dir in ["Desktop", "Documents", "Downloads", "Library/Mobile Documents"] {
            assert!(tcc_protected(&home.join(dir).join("memdusa"), home), "{dir}");
        }
        assert!(tcc_protected(home.join("Desktop").as_path(), home));
        for dir in [".memdusa", ".local/bin", "src/project", "hermes/hermes-agent"] {
            assert!(!tcc_protected(&home.join(dir).join("memdusa"), home), "{dir}");
        }
        // a sibling with a similar name is not inside a protected folder
        assert!(!tcc_protected(Path::new("/Users/someone/Desktopish/memdusa"), home));
    }

    #[test]
    fn claude_merge_is_idempotent_and_preserves_other_settings() {
        let existing = json!({
            "model": "opus",
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "say done"}]}],
                "SessionStart": [{"hooks": [{"type": "command", "command": "/old/path/memdusa hook session-start"}]}]
            }
        });
        let once = merge_hooks(existing, Harness::ClaudeCode, BIN, None, false).unwrap();
        let twice = merge_hooks(once.clone(), Harness::ClaudeCode, BIN, None, false).unwrap();
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

        let removed = merge_hooks(once, Harness::ClaudeCode, BIN, None, true).unwrap();
        assert_eq!(
            removed,
            json!({"model": "opus", "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}})
        );
    }

    #[test]
    fn quotes_paths_with_spaces() {
        let s = merge_hooks(json!({}), Harness::ClaudeCode, "/Users/me/my tools/memdusa", None, false).unwrap();
        let cmd = s["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(cmd, "'/Users/me/my tools/memdusa' hook stop");
        assert!(merge_hooks(s, Harness::ClaudeCode, "/x/memdusa", None, true).unwrap().get("hooks").is_none());
    }

    #[test]
    fn harness_specific_shapes() {
        // Codex: Claude-like groups, its own matcher and the 3s SessionEnd cap
        let codex = merge_hooks(json!({}), Harness::Codex, BIN, None, false).unwrap();
        assert_eq!(codex["hooks"]["PostToolUse"][0]["matcher"], "Bash|apply_patch");
        assert_eq!(codex["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 3);
        assert_eq!(codex["hooks"]["Stop"][0]["hooks"][0]["command"], format!("{BIN} hook stop --harness codex"));

        // Gemini: native event names, millisecond timeouts
        let gem = merge_hooks(json!({"theme": "dark"}), Harness::Gemini, BIN, None, false).unwrap();
        assert_eq!(
            gem["hooks"]["BeforeAgent"][0]["hooks"][0]["command"],
            format!("{BIN} hook user-prompt-submit --harness gemini")
        );
        assert_eq!(gem["hooks"]["SessionStart"][0]["hooks"][0]["timeout"], 10_000);
        assert_eq!(gem["theme"], "dark");

        // Cursor: version 1, handlers directly in the event list, two native events for tool use
        let mine = json!({"command": "./audit.sh"});
        let cur = merge_hooks(json!({"hooks": {"afterFileEdit": [mine.clone()]}}), Harness::Cursor, BIN, None, false)
            .unwrap();
        assert_eq!(cur["version"], 1);
        assert_eq!(cur["hooks"]["afterFileEdit"].as_array().unwrap().len(), 2);
        assert_eq!(
            cur["hooks"]["afterShellExecution"][0]["command"],
            format!("{BIN} hook post-tool-use --harness cursor")
        );
        let back = merge_hooks(cur, Harness::Cursor, BIN, None, true).unwrap();
        assert_eq!(back, json!({"version": 1, "hooks": {"afterFileEdit": [mine]}}));

        // Antigravity: one named group we own
        let agy = merge_hooks(json!({"other": {"enabled": true}}), Harness::Antigravity, BIN, None, false).unwrap();
        assert_eq!(agy["memdusa"]["enabled"], true);
        assert_eq!(
            agy["memdusa"]["SessionStart"][0]["command"],
            format!("{BIN} hook session-start --harness antigravity")
        );
        assert!(agy["memdusa"]["PostToolUse"][0]["matcher"].as_str().unwrap().contains("write_file"));
        assert!(
            agy["memdusa"]["PreToolUse"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with("hook mcp-approval --harness antigravity")
        );
        assert_eq!(
            merge_hooks(agy, Harness::Antigravity, BIN, None, true).unwrap(),
            json!({"other": {"enabled": true}})
        );
    }

    #[test]
    fn mcp_configs() {
        let cur = merge_mcp_json(json!({"mcpServers": {"x": {}}}), Harness::Cursor, BIN, None, false).unwrap();
        assert_eq!(
            cur["mcpServers"]["memdusa"],
            json!({"type": "stdio", "command": BIN, "args": ["mcp", "--agent", "cursor"]})
        );
        assert_eq!(merge_mcp_json(cur, Harness::Cursor, BIN, None, true).unwrap(), json!({"mcpServers": {"x": {}}}));

        let toml = "model = \"o5\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
        let added = merge_codex_toml(toml, "/opt/my \"bin\"/memdusa", None, false);
        assert!(
            added.ends_with(concat!(
                "[mcp_servers.memdusa]\n",
                "command = \"/opt/my \\\"bin\\\"/memdusa\"\n",
                "args = [\"mcp\", \"--agent\", \"codex\"]\n",
                "default_tools_approval_mode = \"approve\"\n",
                "env_vars = [\"MEMDUSA_RUN\", \"MEMDUSA_TASK\", \"MEMDUSA_PARENT\", \"MEMDUSA_STEP_LEASE_SECS\"]\n"
            )),
            "{added}"
        );
        assert_eq!(merge_codex_toml(&added, "/opt/my \"bin\"/memdusa", None, false), added, "idempotent");
        assert_eq!(merge_codex_toml(&added, BIN, None, true), toml, "uninstall restores the original");
        let between =
            "[mcp_servers.memdusa]\ncommand = \"old\"\n[mcp_servers.memdusa.env]\nA = \"1\"\n[profile]\nx = 1\n";
        assert_eq!(
            merge_codex_toml(between, BIN, None, true),
            "[profile]\nx = 1\n",
            "removes nested tables, keeps later ones"
        );
    }

    #[test]
    fn cursor_cli_permissions_are_narrow_idempotent_and_reversible() {
        // A fresh project file gets exactly one narrow entry.
        let fresh = merge_cursor_permissions(json!({}), false).unwrap();
        // Cursor rejects a project cli.json without `permissions.deny`, so both arrays are created together.
        assert_eq!(fresh, json!({"permissions": {"allow": [CURSOR_MCP_PERMISSION], "deny": []}}));
        assert_eq!(merge_cursor_permissions(fresh.clone(), false).unwrap(), fresh, "idempotent");
        assert_eq!(
            merge_cursor_permissions(fresh, true).unwrap(),
            json!({}),
            "uninstall removes exactly what we added"
        );

        // Existing user config: only our entry is added; every other key and array element survives verbatim.
        let existing = json!({
            "version": 1,
            "editor": {"vimMode": false},
            "permissions": {
                "allow": ["Shell(ls)", "WebFetch(docs.github.com)", "Mcp(other:*)", "Mcp(memdusa:*)", "Mcp(memdusa:*)"],
                "deny": ["Shell(rm)"]
            }
        });
        let once = merge_cursor_permissions(existing.clone(), false).unwrap();
        assert_eq!(
            once["permissions"]["allow"],
            json!(["Shell(ls)", "WebFetch(docs.github.com)", "Mcp(other:*)", CURSOR_MCP_PERMISSION]),
            "duplicates collapsed in place, other servers untouched"
        );
        assert_eq!(once["permissions"]["deny"], json!(["Shell(rm)"]));
        assert_eq!(once["version"], 1);
        assert_eq!(once["editor"], json!({"vimMode": false}));
        assert_eq!(merge_cursor_permissions(once.clone(), false).unwrap(), once, "idempotent with user keys");
        let removed = merge_cursor_permissions(once, true).unwrap();
        assert_eq!(removed["permissions"]["allow"], json!(["Shell(ls)", "WebFetch(docs.github.com)", "Mcp(other:*)"]));
        assert_eq!(removed["permissions"]["deny"], json!(["Shell(rm)"]));
        assert_eq!(removed["version"], 1);

        // A config with no permissions at all is restored exactly.
        let plain = json!({"version": 1, "editor": {"vimMode": true}});
        let added = merge_cursor_permissions(plain.clone(), false).unwrap();
        assert_eq!(added["permissions"]["allow"], json!([CURSOR_MCP_PERMISSION]));
        assert_eq!(added["permissions"]["deny"], json!([]));
        assert_eq!(merge_cursor_permissions(added, true).unwrap(), plain);
    }

    #[test]
    fn codex_mcp_config_forwards_coordination_vars_and_keeps_user_keys() {
        let existing = concat!(
            "model = \"o5\"\n\n",
            "[mcp_servers.memdusa]\n",
            "command = \"/old/memdusa\"\n",
            "env = { FOO = \"bar\" }\n",             // user key: must survive
            "startup_timeout_sec = 42\n",            // user key: must survive
            "tags = [\n  \"one\",\n  \"two\",\n]\n", // user's multi-line array: must survive verbatim
            "\n",
            "[mcp_servers.memdusa.env]\n",
            "NESTED = \"1\"\n", // user sub-table: must survive
            "\n",
            "[profile]\n",
            "x = 1\n",
        );
        let once = merge_codex_toml(existing, BIN, Some("sqlite:///x/m.db"), false);
        // memdusa's own keys are refreshed, including the stale command path
        assert!(once.contains("command = \"/usr/local/bin/memdusa\""));
        assert!(once.contains("args = [\"mcp\", \"--agent\", \"codex\", \"--database-url\", \"sqlite:///x/m.db\"]"));
        assert!(once.contains("default_tools_approval_mode = \"approve\""));
        assert!(once.contains(
            "env_vars = [\"MEMDUSA_RUN\", \"MEMDUSA_TASK\", \"MEMDUSA_PARENT\", \"MEMDUSA_STEP_LEASE_SECS\"]"
        ));
        assert!(!once.contains("/old/memdusa"));
        // only the four coordination variables are forwarded
        assert!(!once.contains("MEMDUSA_HOME"));
        assert!(!once.contains("MEMDUSA_DATABASE_URL"));
        // every user key, comment line and sub-table is untouched
        assert!(once.contains("env = { FOO = \"bar\" }"));
        assert!(once.contains("startup_timeout_sec = 42"));
        assert!(once.contains("tags = [\n  \"one\",\n  \"two\",\n]\n"));
        assert!(once.contains("[mcp_servers.memdusa.env]\nNESTED = \"1\""));
        assert!(once.contains("[profile]\nx = 1\n"));
        // re-installing changes nothing
        assert_eq!(merge_codex_toml(&once, BIN, Some("sqlite:///x/m.db"), false), once);
        // uninstall removes the new keys with the whole table
        let removed = merge_codex_toml(&once, BIN, Some("sqlite:///x/m.db"), true);
        assert_eq!(removed, "model = \"o5\"\n\n[profile]\nx = 1\n");
        assert!(!removed.contains("mcp_servers.memdusa"));
        assert!(!removed.contains("env_vars"));
        assert!(!removed.contains("startup_timeout_sec"));
    }

    #[test]
    fn plugins_and_hermes() {
        let src = plugin_source(Harness::OpenCode, "/opt/my \"tools\"/memdusa", None);
        assert!(src.contains(r#"const BIN: string = "/opt/my \"tools\"/memdusa""#), "bin is a JSON string literal");
        assert!(!src.contains("__MEMDUSA_BIN__"));
        assert!(!src.contains("__MEMDUSA_EXTRA_ARGS__"));
        assert!(src.contains(r#"["hook", event, "--agent", AGENT]"#), "no URL: spawn args unchanged");
        assert!(plugin_source(Harness::Pi, BIN, None).contains("agent_before_settle"));
        let pi = merge_mcp_json(json!({}), Harness::Pi, BIN, None, false).unwrap();
        assert_eq!(pi["mcpServers"]["memdusa"]["exposure"], "direct");

        let base = "model: hermes-4\nterminal:\n  backend: local\n";
        let added = crate::hermes_yaml::merge(base, BIN, None, true, false).unwrap();
        assert!(
            added.contains(
                "pre_llm_call:\n    - command: \"/usr/local/bin/memdusa hook session-start --harness hermes\""
            )
        );
        assert_eq!(crate::hermes_yaml::merge(&added, BIN, None, true, true).unwrap(), base);
    }

    #[test]
    fn cursor_hook_commands_never_contain_a_url_scheme() {
        use crate::hooks::HookEvent;
        // Cursor drops its whole hooks.json when a command contains `://`
        let cmd = hook_command("/b/memdusa", Harness::Cursor, HookEvent::SessionStart, Some("sqlite:///x/m.db"));
        assert!(cmd.ends_with(" --database-url sqlite:/x/m.db"), "{cmd}");
        assert!(!cmd.contains("://"), "{cmd}");
        // other harnesses keep the URL as given
        let claude = hook_command("/b/memdusa", Harness::ClaudeCode, HookEvent::SessionStart, Some("sqlite:///x/m.db"));
        assert!(claude.ends_with(" --database-url sqlite:///x/m.db"), "{claude}");
        // a URL that can't avoid `://` gets a warning instead
        assert!(cursor_db_note(Some("sqlite:///x/m.db")).is_none());
        assert!(cursor_db_note(None).is_none());
        assert!(cursor_db_note(Some("postgres://u@h/db")).is_some_and(|n| n.contains("memdusa login")));
    }

    #[test]
    fn database_url_is_written_into_every_harness_format() {
        const URL: &str = "sqlite:///x/memdusa.db";
        let flag = " --database-url sqlite:///x/memdusa.db";

        // JSON hook configs: Claude Code, Codex, Gemini (shared settings), Cursor, Antigravity
        for h in [Harness::ClaudeCode, Harness::Codex, Harness::Gemini] {
            let v = merge_hooks(json!({}), h, BIN, Some(URL), false).unwrap();
            for (_, groups) in v["hooks"].as_object().unwrap() {
                let cmd = groups[0]["hooks"][0]["command"].as_str().unwrap();
                assert!(cmd.ends_with(flag), "{h:?}: {cmd}");
                assert!(is_memdusa_hook(cmd), "still recognised for uninstall: {cmd}");
            }
            assert_eq!(merge_hooks(v, h, BIN, None, true).unwrap(), json!({}), "{h:?} uninstall");
        }
        let cur = merge_hooks(json!({}), Harness::Cursor, BIN, Some(URL), false).unwrap();
        // Cursor gets the `sqlite:<path>` form: it drops hooks.json when a command contains `://`
        let cur_cmd = cur["hooks"]["stop"][0]["command"].as_str().unwrap();
        assert!(cur_cmd.ends_with("--harness cursor --database-url sqlite:/x/memdusa.db"), "{cur_cmd}");
        let agy = merge_hooks(json!({}), Harness::Antigravity, BIN, Some(URL), false).unwrap();
        assert!(agy["memdusa"]["Stop"][0]["command"].as_str().unwrap().ends_with(flag));

        // URLs needing shell quoting are quoted in the command string
        let pg = merge_hooks(json!({}), Harness::ClaudeCode, BIN, Some("postgres://u:p w@h/db?x=1"), false).unwrap();
        assert!(
            pg["hooks"]["Stop"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with(" --database-url 'postgres://u:p w@h/db?x=1'")
        );

        // JSON MCP entries get the args; without a URL they are unchanged
        let mcp = merge_mcp_json(json!({}), Harness::Cursor, BIN, Some(URL), false).unwrap();
        assert_eq!(mcp["mcpServers"]["memdusa"]["args"], json!(["mcp", "--agent", "cursor", "--database-url", URL]));
        let plain = merge_mcp_json(json!({}), Harness::Cursor, BIN, None, false).unwrap();
        assert_eq!(plain["mcpServers"]["memdusa"]["args"], json!(["mcp", "--agent", "cursor"]));

        // Codex TOML
        let toml = merge_codex_toml("", "/opt/my \"bin\"/memdusa", Some("postgres://u:p\"q@h/db"), false);
        assert!(
            toml.contains(
                "args = [\"mcp\", \"--agent\", \"codex\", \"--database-url\", \"postgres://u:p\\\"q@h/db\"]\n"
            ),
            "{toml}"
        );
        assert!(merge_codex_toml(&toml, BIN, None, true).is_empty());

        // OpenCode / pi TS templates: extra spawn args as JSON string literals
        for h in [Harness::OpenCode, Harness::Pi] {
            let src = plugin_source(h, BIN, Some("postgres://u:p\"q@h/db"));
            assert!(src.contains(r#"["hook", event, "--agent", AGENT, "--database-url", "postgres://u:p\"q@h/db"]"#));
            assert!(!src.contains("__MEMDUSA_"));
        }

        // Hermes YAML is covered in hermes_yaml's tests; check the hook shape here too
        let yaml = crate::hermes_yaml::merge("", BIN, Some(URL), true, false).unwrap();
        assert!(yaml.contains(&format!("hook stop --harness hermes{flag}\"")));
        assert!(yaml.contains(&format!("args: [\"mcp\", \"--agent\", \"hermes\", \"--database-url\", \"{URL}\"]")));
    }

    /// Event registrations verified against the real tools (OpenCode 2.2.6, pi 0.73.1; fixtures in tests/fixtures/harness).
    #[test]
    fn plugin_templates_register_the_events_that_really_fire() {
        let oc = plugin_source(Harness::OpenCode, BIN, None);
        for needle in [
            r#"ctx.session.hook("prompt""#,
            r#"ctx.tool.hook("execute.after""#,
            r#"ctx.session.hook("compaction""#,
            r#""experimental.idle""#,
            "ctx.session.context(",
            "last_assistant_message",
            "event.background?.length", // `background: []` is truthy
            "plugin.log",
            "delete env.MEMDUSA_PROJECT", // the shared service's env may carry a stale project
        ] {
            assert!(oc.contains(needle), "OpenCode plugin lacks {needle}");
        }
        assert!(!oc.contains("if (event.background) return"));
        let pi = plugin_source(Harness::Pi, BIN, None);
        for needle in [
            r#"on("agent_end""#,
            r#"on("agent_before_settle""#,
            r#"on("session_shutdown""#,
            r#"on("before_agent_start""#,
            "last_assistant_message",
            "plugin.log",
            "delete env.MEMDUSA_PROJECT",
        ] {
            assert!(pi.contains(needle), "pi extension lacks {needle}");
        }

        // the captured payloads carry what the templates read
        let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/harness");
        let read = |f: &str| {
            serde_json::from_str::<Value>(&std::fs::read_to_string(format!("{fixtures}/{f}")).unwrap()).unwrap()
        };
        assert!(read("opencode-idle.json")["background"].as_array().unwrap().is_empty());
        let ctx = read("opencode-session-context.json");
        let last = ctx.as_array().unwrap().last().unwrap();
        assert_eq!(last["type"], "assistant");
        assert!(last["content"].as_array().unwrap().iter().any(|p| p["type"] == "text" && p["text"] == "ok"));
        let end = read("pi-agent-end.json");
        let last = end["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["role"], "assistant");
        assert!(last["content"].as_array().unwrap().iter().any(|p| p["type"] == "text" && p["text"] == "ok"));
    }

    #[test]
    fn git_hook_creates_appends_and_is_idempotent() {
        let line = git_hook_line(BIN, "post-commit");

        // new file: shebang plus our line
        let fresh = merge_git_hook("", &line, false).unwrap();
        assert_eq!(fresh, format!("#!/bin/sh\n{line}\n"));

        // existing user hook: their lines are untouched, ours is appended
        let user = "#!/bin/sh\nnpm test\n";
        let added = merge_git_hook(user, &line, false).unwrap();
        assert_eq!(added, format!("{user}{line}\n"));
        assert!(added.starts_with(user), "user lines unchanged");

        // re-installing does not duplicate
        assert_eq!(merge_git_hook(&added, &line, false).unwrap(), added);
    }

    #[test]
    fn git_hook_replaces_an_old_bin_path() {
        let old = "/old/bin/memdusa git-hook post-commit \"$@\" >/dev/null 2>&1 || true  # memdusa";
        let existing = format!("#!/bin/sh\necho user hook\n{old}\n");
        let line = git_hook_line(BIN, "post-commit");
        let merged = merge_git_hook(&existing, &line, false).unwrap();
        assert_eq!(merged, format!("#!/bin/sh\necho user hook\n{line}\n"));
        assert!(!merged.contains("/old/bin/memdusa"));
        assert_eq!(merged.matches("# memdusa").count(), 1);
    }

    #[test]
    fn git_hook_goes_before_a_trailing_exit() {
        let line = git_hook_line(BIN, "post-commit");

        let user = "#!/bin/sh\nnpm test\nexit 0\n";
        let merged = merge_git_hook(user, &line, false).unwrap();
        assert_eq!(merged, format!("#!/bin/sh\nnpm test\n{line}\nexit 0\n"));
        assert!(merged.find(&line).unwrap() < merged.find("exit 0").unwrap());

        // trailing blank lines stay after the exit statement
        let user_blank = "#!/bin/sh\nexit $?\n\n";
        let merged_blank = merge_git_hook(user_blank, &line, false).unwrap();
        assert_eq!(merged_blank, format!("#!/bin/sh\n{line}\nexit $?\n\n"));
    }

    #[test]
    fn git_hook_appends_when_exit_is_not_the_last_statement() {
        let line = git_hook_line(BIN, "post-commit");
        let user = "#!/bin/sh\nif [ -n \"$CI\" ]; then\n  exit 0\nfi\necho done\n";
        let merged = merge_git_hook(user, &line, false).unwrap();
        assert_eq!(merged, format!("{user}{line}\n"), "the if-block's exit is not trailing");
        assert!(merged.ends_with(&format!("echo done\n{line}\n")));
    }

    #[test]
    fn git_hook_uninstall_restores_an_exit_hook_exactly() {
        let line = git_hook_line(BIN, "post-merge");
        let user = "#!/bin/sh\nmake check\nexit $?\n";
        let added = merge_git_hook(user, &line, false).unwrap();
        assert_eq!(merge_git_hook(&added, &line, true).unwrap(), user);
    }

    #[test]
    fn git_hook_uninstall_restores_or_deletes() {
        let line = git_hook_line(BIN, "post-merge");
        let user = "#!/bin/sh\nmake check\n";
        let added = merge_git_hook(user, &line, false).unwrap();
        assert_eq!(merge_git_hook(&added, &line, true).unwrap(), user, "user's file restored");

        // a file that only existed for us is deleted
        let ours = merge_git_hook("", &line, false).unwrap();
        assert_eq!(merge_git_hook(&ours, &line, true), None);
        assert_eq!(merge_git_hook("#!/bin/sh\n", &line, true), None);
    }

    #[test]
    fn git_hook_quotes_bin_paths_and_names_the_event() {
        let line = git_hook_line("/Users/me/my tools/memdusa", "post-checkout");
        assert_eq!(
            line,
            "'/Users/me/my tools/memdusa' git-hook post-checkout \"$@\" >/dev/null 2>&1 || true  # memdusa"
        );
        assert_eq!(GitHookKind::Checkout.event(), "checkout");
        assert_eq!(GitHookKind::Merge.name(), "post-merge");
    }
}
