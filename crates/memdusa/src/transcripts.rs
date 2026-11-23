//! Import saved agent session transcripts (`memdusa import-transcript`).
//!
//! Work done before memdusa was installed, or in a tool without hooks, still
//! reaches memdusa: a harness's transcript is turned into the events its hooks
//! would have recorded (prompts, file changes, shell commands, then a session
//! end that triggers the fallback checkpoint) and sent through the normal hook
//! pipeline, so redaction, remote mode, timestamps and project detection all
//! apply. Parsers are defensive: lines they don't understand are skipped.

use std::path::{Path, PathBuf};

use anyhow::Context;
use memdusa_core::Store;
use memdusa_core::model::{Identity, Millis, now_ms};
use serde_json::{Value, json};

use crate::hooks::{self, HookBatchItem, HookConfig, HookEvent, HookRequest};
use crate::remote::Client;
use crate::{config, project};

/// Harnesses whose saved sessions can be imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TranscriptHarness {
    ClaudeCode,
    Codex,
    Pi,
}

impl TranscriptHarness {
    /// Agent name recorded on the imported events.
    pub fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Pi => "pi",
        }
    }

    fn parse(self, text: &str, fallback_at: Millis, fallback_id: &str) -> Option<ImportedSession> {
        match self {
            Self::ClaudeCode => parse_claude(text, fallback_at, fallback_id),
            Self::Codex => parse_codex(text, fallback_at, fallback_id),
            Self::Pi => parse_pi(text, fallback_at, fallback_id),
        }
    }
}

/// One hook the transcript implies: event, canonical input, time in ms.
pub type Item = (HookEvent, Value, Millis);

/// A session in harness-independent form, in timestamp order, ending with `session-end`.
#[derive(Debug, Clone)]
pub struct ImportedSession {
    pub id: String,
    pub cwd: Option<String>,
    pub items: Vec<Item>,
}

impl ImportedSession {
    /// (prompts, file changes, commands)
    pub fn counts(&self) -> (usize, usize, usize) {
        let (mut prompts, mut files, mut commands) = (0, 0, 0);
        for (event, input, _) in &self.items {
            match event {
                HookEvent::UserPromptSubmit => prompts += 1,
                HookEvent::PostToolUse => match input["tool_name"].as_str() {
                    Some("Bash") => commands += 1,
                    Some("Edit") => files += edit_paths(&input["tool_input"]).len(),
                    _ => {}
                },
                _ => {}
            }
        }
        (prompts, files, commands)
    }

    pub fn range(&self) -> (Millis, Millis) {
        let at = self.items.iter().map(|i| i.2);
        (at.clone().min().unwrap_or(0), at.max().unwrap_or(0))
    }
}

fn edit_paths(tool_input: &Value) -> Vec<&str> {
    match tool_input.get("file_paths").and_then(Value::as_array) {
        Some(list) => list.iter().filter_map(Value::as_str).collect(),
        None => tool_input.get("file_path").and_then(Value::as_str).into_iter().collect(),
    }
}

// ------------------------------------------------------------------ builder

struct Builder {
    id: Option<String>,
    cwd: Option<String>,
    items: Vec<Item>,
    last_at: Millis,
    fallback_id: String,
}

impl Builder {
    fn new(fallback_at: Millis, fallback_id: &str) -> Self {
        Self { id: None, cwd: None, items: vec![], last_at: fallback_at, fallback_id: fallback_id.to_owned() }
    }

    fn note_id(&mut self, v: &Value) {
        if self.id.is_none() {
            self.id = v.as_str().filter(|s| !s.is_empty()).map(String::from);
        }
    }

    fn note_cwd(&mut self, v: &Value) {
        if self.cwd.is_none() {
            self.cwd = v.as_str().filter(|s| !s.is_empty()).map(String::from);
        }
    }

    /// Time of an entry; entries without a usable timestamp inherit the previous one.
    fn at(&mut self, v: &Value) -> Millis {
        if let Some(t) = timestamp_ms(v) {
            self.last_at = t;
        }
        self.last_at
    }

    fn prompt(&mut self, text: &str, at: Millis) {
        let text = text.trim();
        if !is_noise(text) {
            self.items.push((HookEvent::UserPromptSubmit, json!({ "prompt": text }), at));
        }
    }

    fn edit(&mut self, paths: Vec<String>, at: Millis) {
        let paths: Vec<String> = paths.into_iter().filter(|p| !p.is_empty()).collect();
        let input = match paths.as_slice() {
            [] => return,
            [one] => json!({ "file_path": one }),
            _ => json!({ "file_paths": paths }),
        };
        self.items.push((HookEvent::PostToolUse, json!({ "tool_name": "Edit", "tool_input": input }), at));
    }

    fn bash(&mut self, command: &str, at: Millis) {
        if command.trim().is_empty() {
            return;
        }
        let input = json!({ "tool_name": "Bash", "tool_input": { "command": command } });
        self.items.push((HookEvent::PostToolUse, input, at));
    }

    /// Sort, append the session end and stamp every request with session and cwd.
    fn finish(mut self) -> Option<ImportedSession> {
        if self.items.is_empty() {
            return None;
        }
        self.items.sort_by_key(|i| i.2); // stable: same-time entries keep file order
        let end = self.items.last().map_or(0, |i| i.2);
        self.items.push((HookEvent::SessionEnd, json!({ "reason": "other" }), end));
        let id = self.id.unwrap_or(self.fallback_id);
        for (_, input, _) in &mut self.items {
            if let Some(o) = input.as_object_mut() {
                o.insert("session_id".into(), json!(id));
                if let Some(cwd) = &self.cwd {
                    o.insert("cwd".into(), json!(cwd));
                }
            }
        }
        Some(ImportedSession { id, cwd: self.cwd, items: self.items })
    }
}

/// "Prompts" that the harness injected rather than the user typing them.
fn is_noise(text: &str) -> bool {
    const PREFIXES: [&str; 11] = [
        "<command-name>",
        "<command-message>",
        "<local-command-",
        "<system-reminder>",
        "<environment_context>",
        "<user_instructions>",
        "<user_shell_command>",
        "<turn_aborted>",
        "# AGENTS.md instructions",
        "[Request interrupted",
        "Caveat: The messages below",
    ];
    text.is_empty() || PREFIXES.iter().any(|p| text.starts_with(p))
}

/// Text of a message: a plain string, or the `text` blocks of a block array.
fn message_text(content: &Value, block_types: &[&str]) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"].as_str().is_some_and(|t| block_types.contains(&t)))
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn lines(text: &str) -> impl Iterator<Item = Value> + '_ {
    text.lines().filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
}

// -------------------------------------------------------------- claude code

fn parse_claude(text: &str, fallback_at: Millis, fallback_id: &str) -> Option<ImportedSession> {
    let mut b = Builder::new(fallback_at, fallback_id);
    for v in lines(text) {
        let kind = v["type"].as_str().unwrap_or_default();
        if !matches!(kind, "user" | "assistant") || v["isMeta"] == true || v["isSidechain"] == true {
            continue;
        }
        b.note_id(&v["sessionId"]);
        b.note_cwd(&v["cwd"]);
        let at = b.at(&v["timestamp"]);
        let content = &v["message"]["content"];
        if kind == "user" {
            b.prompt(&message_text(content, &["text"]), at);
            continue;
        }
        for block in content.as_array().into_iter().flatten().filter(|b| b["type"] == "tool_use") {
            let input = &block["input"];
            let field = |k: &str| input[k].as_str().map(String::from);
            match block["name"].as_str().unwrap_or_default() {
                "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
                    b.edit(field("file_path").or_else(|| field("notebook_path")).into_iter().collect(), at)
                }
                "Bash" => b.bash(&field("command").unwrap_or_default(), at),
                _ => {}
            }
        }
    }
    b.finish()
}

// -------------------------------------------------------------------- codex

fn parse_codex(text: &str, fallback_at: Millis, fallback_id: &str) -> Option<ImportedSession> {
    let mut b = Builder::new(fallback_at, fallback_id);
    for v in lines(text) {
        let at = b.at(&v["timestamp"]);
        let p = &v["payload"];
        match v["type"].as_str().unwrap_or_default() {
            "session_meta" => {
                b.note_id(&p["id"]);
                b.note_cwd(&p["cwd"]);
            }
            "response_item" => match p["type"].as_str().unwrap_or_default() {
                "message" if p["role"] == "user" => b.prompt(&message_text(&p["content"], &["input_text", "text"]), at),
                "function_call" | "custom_tool_call" | "local_shell_call" => codex_call(&mut b, p, at),
                _ => {}
            },
            _ => {}
        }
    }
    b.finish()
}

fn codex_call(b: &mut Builder, p: &Value, at: Millis) {
    let name = p["name"].as_str().unwrap_or("local_shell");
    // `arguments` is a JSON string; custom tools carry a raw `input` string instead
    let mut args = match &p["arguments"] {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        other => other.clone(),
    };
    if args.is_null() && p["action"].is_object() {
        args = p["action"].clone();
    }
    let raw = p["input"].as_str();
    let command = ["command", "cmd"].iter().find_map(|k| (!args[*k].is_null()).then(|| command_text(&args[*k])));
    let patch_text = || {
        raw.map(String::from)
            .or_else(|| ["input", "patch"].iter().find_map(|k| args[*k].as_str().map(String::from)))
            .or_else(|| command.clone())
            .unwrap_or_default()
    };
    if name == "apply_patch" {
        b.edit(crate::adapters::patch_files(&patch_text()), at);
    } else if matches!(name, "shell" | "exec_command" | "container.exec" | "shell_command" | "local_shell") {
        let cmd = command.unwrap_or_default();
        // `apply_patch <<EOF …` run through the shell is an edit, not a command
        let files = if cmd.contains("*** Begin Patch") { crate::adapters::patch_files(&cmd) } else { vec![] };
        if files.is_empty() {
            b.bash(&cmd, at);
        } else {
            b.edit(files, at);
        }
    }
}

/// A command given as an argv array (joined with spaces) or a string.
fn command_text(v: &Value) -> String {
    match v {
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

// ----------------------------------------------------------------------- pi

fn parse_pi(text: &str, fallback_at: Millis, fallback_id: &str) -> Option<ImportedSession> {
    let mut b = Builder::new(fallback_at, fallback_id);
    for v in lines(text) {
        match v["type"].as_str().unwrap_or_default() {
            "session" => {
                b.note_id(&v["id"]);
                b.note_cwd(&v["cwd"]);
                b.at(&v["timestamp"]);
            }
            "message" => {
                let m = &v["message"];
                let stamp = if timestamp_ms(&v["timestamp"]).is_some() { &v["timestamp"] } else { &m["timestamp"] };
                let at = b.at(stamp);
                match m["role"].as_str().unwrap_or_default() {
                    "user" => b.prompt(&message_text(&m["content"], &["text"]), at),
                    "assistant" => {
                        let blocks = m["content"].as_array().into_iter().flatten();
                        for block in blocks.filter(|b| matches!(b["type"].as_str(), Some("toolCall" | "tool_call"))) {
                            let args =
                                if block["arguments"].is_object() { &block["arguments"] } else { &block["input"] };
                            let field = |k: &str| args[k].as_str().map(String::from);
                            match block["name"].as_str().unwrap_or_default().to_ascii_lowercase().as_str() {
                                "write" | "edit" => {
                                    b.edit(field("path").or_else(|| field("file_path")).into_iter().collect(), at)
                                }
                                "bash" => b.bash(&field("command").unwrap_or_default(), at),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    b.finish()
}

// -------------------------------------------------------------------- time

/// ISO 8601 string or a number of seconds / milliseconds, as epoch milliseconds.
fn timestamp_ms(v: &Value) -> Option<Millis> {
    match v {
        Value::String(s) => iso_to_ms(s),
        Value::Number(n) => n.as_i64().map(|n| if n > 100_000_000_000 { n } else { n * 1000 }),
        _ => None,
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]` as epoch milliseconds.
pub fn iso_to_ms(s: &str) -> Option<Millis> {
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut rest = s.get(19..)?;
    let mut ms = 0;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
        ms = format!("{digits:0<3}")[..3].parse::<i64>().ok()?;
        rest = &frac[digits.len()..];
    }
    let offset_secs = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (oh, om) =
                (rest.get(1..3)?.parse::<i64>().ok()?, rest.get(4..6).and_then(|m| m.parse().ok()).unwrap_or(0));
            sign * (oh * 3600 + om * 60)
        }
    };
    let days = days_from_civil(y, mo, d);
    Some((((days * 24 + h) * 60 + mi) * 60 + sec - offset_secs) * 1000 + ms)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// Epoch milliseconds as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn ms_to_iso(ms: Millis) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

// ----------------------------------------------------------------- loading

/// Read and parse one transcript file. `Ok(None)`: nothing importable in it.
pub fn load(harness: TranscriptHarness, path: &Path) -> anyhow::Result<Option<ImportedSession>> {
    if path.extension().is_some_and(|e| e == "zst") {
        anyhow::bail!("compressed (.zst) transcripts are not supported; decompress it first (zstd -d)");
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let fallback_at = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or_else(now_ms, |d| d.as_millis() as Millis);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("session");
    Ok(harness.parse(&String::from_utf8_lossy(&bytes), fallback_at, stem))
}

fn mtime(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH)
}

fn files_in(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == ext)).collect()
}

fn walk_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for p in rd.filter_map(|e| e.ok().map(|e| e.path())) {
        if p.is_dir() {
            walk_jsonl(&p, out);
        } else if p.extension().is_some_and(|e| e == "jsonl") {
            out.push(p);
        }
    }
}

/// The directory itself and its git root: sessions started anywhere in the repo count.
fn repo_roots(cwd: &Path) -> Vec<String> {
    let mut roots = vec![cwd.display().to_string()];
    let top = crate::project::git_command()
        .and_then(|mut git| git.arg("-C").arg(cwd).args(["rev-parse", "--show-toplevel"]).output().ok());
    if let Some(top) = top.filter(|o| o.status.success()) {
        let top = String::from_utf8_lossy(&top.stdout).trim().to_owned();
        if !top.is_empty() && !roots.contains(&top) {
            roots.push(top);
        }
    }
    roots
}

fn within(cwd: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    cwd == root || cwd.strip_prefix(root).is_some_and(|r| r.starts_with('/'))
}

/// `cwd` recorded in a transcript's first lines (codex: session_meta, pi: session).
fn recorded_cwd(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let reader = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    reader
        .lines()
        .take(5)
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .find_map(|v| v["payload"]["cwd"].as_str().or_else(|| v["cwd"].as_str()).map(String::from))
}

/// The most recent session for the repo containing `cwd`.
pub fn latest(harness: TranscriptHarness, cwd: &Path) -> anyhow::Result<Option<PathBuf>> {
    let home = config::home_dir()?;
    let roots = repo_roots(cwd);
    let matches_repo = |p: &Path| recorded_cwd(p).is_some_and(|c| roots.iter().any(|r| within(&c, r)));
    // newest first; the file names of codex and pi start with their timestamp
    let newest_by_name = |mut files: Vec<PathBuf>| {
        files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
        files.into_iter().take(500).find(|p| matches_repo(p))
    };
    Ok(match harness {
        TranscriptHarness::ClaudeCode => {
            let projects = home.join(".claude").join("projects");
            roots
                .iter()
                .flat_map(|r| {
                    let encoded: String =
                        r.chars().map(|c| if matches!(c, '/' | '.' | '\\' | ':') { '-' } else { c }).collect();
                    files_in(&projects.join(encoded), "jsonl")
                })
                .max_by_key(|p| mtime(p))
        }
        TranscriptHarness::Codex => {
            let mut files = vec![];
            walk_jsonl(&home.join(".codex").join("sessions"), &mut files);
            files.retain(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("rollout-")));
            newest_by_name(files)
        }
        TranscriptHarness::Pi => {
            let root = home.join(".pi").join("agent").join("sessions");
            let dirs =
                std::fs::read_dir(root).map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path()))).into_iter().flatten();
            newest_by_name(dirs.flat_map(|d| files_in(&d, "jsonl")).collect())
        }
    })
}

// ------------------------------------------------------------------ importing

pub struct ImportOptions {
    pub harness: TranscriptHarness,
    pub files: Vec<PathBuf>,
    pub latest: bool,
    pub force: bool,
    pub dry_run: bool,
    /// `--project`; else detected from each transcript's cwd.
    pub project: Option<String>,
}

pub enum Backend<'a> {
    Local(&'a Store),
    Remote(&'a Client),
}

impl Backend<'_> {
    async fn has_events(&self, project: &str, session: &str) -> anyhow::Result<bool> {
        Ok(match self {
            Self::Local(s) => !s.events_recent(project, Some(session), 1).await?.is_empty(),
            Self::Remote(c) => !c.events(project, Some(session), 1).await?.is_empty(),
        })
    }

    /// Run the session's hooks through the canonical pipeline, in order.
    async fn send(&self, harness: TranscriptHarness, project: &str, session: &ImportedSession) -> anyhow::Result<()> {
        let config =
            HookConfig { agent: harness.name().into(), journal_prompts: true, stop_guard_mins: 0, recall: false };
        let request = |input: &Value, at: Millis| HookRequest {
            project: project.to_owned(),
            config: config.clone(),
            input: input.clone(),
            git: None,
            at: Some(at),
            task: None,
            parent: None,
            run: None,
            session_coord: false,
        };
        match self {
            Self::Local(store) => {
                for (event, input, at) in &session.items {
                    hooks::handle_request(store, *event, request(input, *at), &Identity::root()).await?;
                }
            }
            Self::Remote(client) => {
                // redact here too, so secrets never leave this machine (the server redacts again)
                let redactor = crate::redactor_from_env()?;
                let items: Vec<HookBatchItem> = session
                    .items
                    .iter()
                    .map(|(event, input, at)| {
                        let mut req = request(input, *at);
                        req.input = redactor.json(req.input);
                        HookBatchItem { event: event.arg().into(), request: req }
                    })
                    .collect();
                for chunk in items.chunks(200) {
                    let resp = client.hook_batch(chunk).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                    for r in resp.rejected {
                        eprintln!("memdusa: server rejected an imported event: {r}");
                    }
                }
            }
        }
        Ok(())
    }
}

pub async fn run(opts: ImportOptions, backend: Backend<'_>) -> anyhow::Result<()> {
    let mut files = opts.files.clone();
    if opts.latest {
        let cwd = std::env::current_dir()?;
        match latest(opts.harness, &cwd)? {
            Some(p) => files.push(p),
            None => anyhow::bail!("no {} session found for {}", opts.harness.name(), cwd.display()),
        }
    }
    if files.is_empty() {
        anyhow::bail!("pass transcript FILES or --latest");
    }
    for path in files {
        let shown = path.display();
        let session = match load(opts.harness, &path) {
            Ok(Some(s)) => s,
            Ok(None) => {
                eprintln!("skipped {shown}: no prompts, file changes or commands in it");
                continue;
            }
            Err(e) => {
                eprintln!("skipped {shown}: {e:#}");
                continue;
            }
        };
        let project = opts
            .project
            .clone()
            .or_else(|| session.cwd.as_deref().map(|d| project::detect(Path::new(d))))
            .or_else(project::detect_cwd)
            .context("could not determine project; pass --project")?;
        let (prompts, edits, commands) = session.counts();
        let summary =
            format!("{} ({prompts} prompts, {edits} file changes, {commands} commands) from {shown}", session.id);
        if !opts.force && backend.has_events(&project, &session.id).await? {
            println!("skipped {}: already imported (use --force to import it again)", session.id);
        } else if opts.dry_run {
            let (from, to) = session.range();
            println!("would import {summary}, {} to {}, project {project}", ms_to_iso(from), ms_to_iso(to));
        } else {
            backend.send(opts.harness, &project, &session).await?;
            println!("imported {summary}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = include_str!("../tests/fixtures/transcripts/claude-code.jsonl");
    const CODEX: &str = include_str!("../tests/fixtures/transcripts/codex.jsonl");
    const PI: &str = include_str!("../tests/fixtures/transcripts/pi.jsonl");

    fn summary(s: &ImportedSession) -> Vec<(String, String, Millis)> {
        s.items
            .iter()
            .map(|(e, input, at)| {
                let what = match e {
                    HookEvent::UserPromptSubmit => input["prompt"].as_str().unwrap().to_owned(),
                    HookEvent::PostToolUse => format!(
                        "{} {}",
                        input["tool_name"].as_str().unwrap(),
                        input["tool_input"]["command"]
                            .as_str()
                            .map(String::from)
                            .unwrap_or_else(|| edit_paths(&input["tool_input"]).join(","))
                    ),
                    other => other.arg().to_owned(),
                };
                (e.arg().to_owned(), what, *at)
            })
            .collect()
    }

    #[test]
    fn iso_times() {
        assert_eq!(iso_to_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_to_ms("2026-03-01T10:00:00.000Z"), Some(1_772_359_200_000));
        assert_eq!(iso_to_ms("2026-03-01T12:00:00.5+02:00"), Some(1_772_359_200_500));
        assert_eq!(iso_to_ms("2024-02-29T23:59:59Z").map(ms_to_iso).as_deref(), Some("2024-02-29T23:59:59Z"));
        assert_eq!(iso_to_ms("garbage"), None);
        assert_eq!(iso_to_ms("2026-13-01T00:00:00Z"), None);
        assert_eq!(ms_to_iso(1_772_359_200_000), "2026-03-01T10:00:00Z");
    }

    #[test]
    fn claude_code_prompts_edits_and_commands_in_order() {
        let s = parse_claude(CLAUDE, 1, "fallback").expect("session");
        assert_eq!(s.id, "sess-claude-1");
        assert_eq!(s.cwd.as_deref(), Some("/work/demo"));
        let t0 = 1_772_359_200_000;
        assert_eq!(
            summary(&s),
            vec![
                ("user-prompt-submit".into(), "Add a refunds endpoint to the billing service".into(), t0),
                ("post-tool-use".into(), "Edit /work/demo/src/refunds.rs".into(), t0 + 5_000),
                ("post-tool-use".into(), "Bash cargo test refunds".into(), t0 + 20_000),
                ("user-prompt-submit".into(), "Also add tests\nfor partial refunds".into(), t0 + 60_000),
                ("post-tool-use".into(), "Edit /work/demo/tests/refunds.rs".into(), t0 + 70_000),
                ("post-tool-use".into(), "Edit /work/demo/src/lib.rs".into(), t0 + 120_000),
                ("session-end".into(), "session-end".into(), t0 + 120_000),
            ]
        );
        assert_eq!(s.counts(), (2, 3, 1));
        assert_eq!(s.items[0].1["session_id"], "sess-claude-1");
        assert_eq!(s.items[0].1["cwd"], "/work/demo");
        assert_eq!(s.items.last().unwrap().1["reason"], "other");
    }

    #[test]
    fn codex_prompts_patches_and_commands() {
        let s = parse_codex(CODEX, 1, "fallback").expect("session");
        assert_eq!(s.id, "sess-codex-1");
        assert_eq!(s.cwd.as_deref(), Some("/work/demo"));
        let t0 = iso_to_ms("2026-03-02T09:00:00Z").unwrap();
        assert_eq!(
            summary(&s),
            vec![
                ("user-prompt-submit".into(), "Fix the flaky login test".into(), t0 + 5_000),
                ("post-tool-use".into(), "Bash bash -lc cargo test login".into(), t0 + 10_000),
                ("post-tool-use".into(), "Edit src/login.rs,tests/login.rs".into(), t0 + 20_000),
                ("post-tool-use".into(), "Edit src/db.rs".into(), t0 + 30_000),
                ("session-end".into(), "session-end".into(), t0 + 30_000),
            ]
        );
        assert_eq!(s.counts(), (1, 3, 1));
    }

    #[test]
    fn pi_prompts_edits_and_commands() {
        let s = parse_pi(PI, 1, "fallback").expect("session");
        assert_eq!(s.id, "sess-pi-1");
        assert_eq!(s.cwd.as_deref(), Some("/work/demo"));
        let t0 = iso_to_ms("2026-03-03T08:00:00Z").unwrap();
        assert_eq!(
            summary(&s),
            vec![
                ("user-prompt-submit".into(), "Rename the helper".into(), t0 + 1_000),
                ("post-tool-use".into(), "Edit src/helper.rs".into(), t0 + 2_000),
                ("post-tool-use".into(), "Bash ls -la".into(), t0 + 3_000),
                ("post-tool-use".into(), "Edit src/main.rs".into(), t0 + 4_000),
                ("session-end".into(), "session-end".into(), t0 + 4_000),
            ]
        );
        assert_eq!(s.counts(), (1, 2, 1));
    }

    #[test]
    fn garbage_and_empty_transcripts_are_skipped_without_panicking() {
        for parse in [parse_claude, parse_codex, parse_pi] {
            assert!(parse("", 1, "x").is_none());
            assert!(parse("not json\n{\"type\":\n[1,2]\n\"str\"\n{}\n", 1, "x").is_none());
        }
    }

    #[test]
    fn zst_transcripts_are_refused_with_a_message() {
        let err = load(TranscriptHarness::Codex, Path::new("rollout-1.jsonl.zst")).unwrap_err();
        assert!(err.to_string().contains(".zst"), "{err}");
    }

    #[test]
    fn repo_membership() {
        assert!(within("/a/b", "/a/b"));
        assert!(within("/a/b/c", "/a/b"));
        assert!(!within("/a/bc", "/a/b"));
    }
}
