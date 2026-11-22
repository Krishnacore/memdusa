//! Harness adapters. Every harness has its own hook dialect; memdusa's hook
//! logic (local or on a server) speaks one canonical, Claude Code–shaped
//! format. An adapter translates on the client:
//!
//! * `normalize`: the harness's stdin payload -> canonical hook input
//! * `render`: canonical hook output -> what the harness expects on stdout
//! * `events`: which native events the installer hooks, and as what
//!
//! Formats verified against each tool's docs/source (Nov 2026): Codex CLI
//! 0.171, Cursor 2026.11, Gemini CLI 0.70, Antigravity CLI (`agy`).

use serde_json::{Map, Value, json};

use crate::hooks::HookEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Harness {
    ClaudeCode,
    Codex,
    Cursor,
    Gemini,
    /// Google Antigravity CLI (`agy`), Gemini CLI's successor for consumer accounts.
    Antigravity,
    /// OpenCode v2: a TypeScript plugin that calls `memdusa hook` with canonical payloads.
    #[value(name = "opencode")]
    OpenCode,
    /// pi coding agent: a TypeScript extension, like OpenCode's plugin.
    Pi,
    /// Nous Research's Hermes Agent: shell hooks in ~/.hermes/config.yaml.
    Hermes,
    /// Git hooks (post-commit, post-checkout, post-merge) in the repo's hooks directory.
    Git,
}

/// One native hook the installer registers.
pub struct NativeHook {
    /// The harness's event name.
    pub native: &'static str,
    /// What memdusa handles it as.
    pub event: HookEvent,
    pub matcher: Option<&'static str>,
    /// Seconds.
    pub timeout_secs: u64,
}

const fn hook(native: &'static str, event: HookEvent, matcher: Option<&'static str>, timeout_secs: u64) -> NativeHook {
    NativeHook { native, event, matcher, timeout_secs }
}

impl Harness {
    /// Name recorded as the agent (`alice/codex`).
    pub fn agent(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Gemini => "gemini",
            Self::Antigravity => "antigravity",
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
            Self::Hermes => "hermes",
            Self::Git => "git",
        }
    }

    pub fn events(self) -> Vec<NativeHook> {
        use HookEvent::*;
        match self {
            Self::ClaudeCode => vec![
                hook("SessionStart", SessionStart, None, 10),
                hook("UserPromptSubmit", UserPromptSubmit, None, 5),
                hook("PostToolUse", PostToolUse, Some(crate::hooks::JOURNALED_TOOLS), 5),
                hook("Stop", Stop, None, 5),
                hook("PreCompact", PreCompact, None, 10),
                hook("PostCompact", PostCompact, None, 10),
                hook("SessionEnd", SessionEnd, None, 10),
            ],
            // SessionStart re-fires with source "compact" after compaction: the brief is re-injected.
            // SessionEnd hooks get 3s at most.
            Self::Codex => vec![
                hook("SessionStart", SessionStart, None, 10),
                hook("UserPromptSubmit", UserPromptSubmit, None, 5),
                hook("PostToolUse", PostToolUse, Some("Bash|apply_patch"), 5),
                hook("Stop", Stop, None, 5),
                hook("PreCompact", PreCompact, None, 10),
                hook("SessionEnd", SessionEnd, None, 3),
            ],
            Self::Cursor => vec![
                hook("sessionStart", SessionStart, None, 10),
                hook("beforeSubmitPrompt", UserPromptSubmit, None, 5),
                hook("afterFileEdit", PostToolUse, None, 5),
                hook("afterShellExecution", PostToolUse, None, 5),
                hook("stop", Stop, None, 5),
                hook("preCompact", PreCompact, None, 10),
                hook("sessionEnd", SessionEnd, None, 10),
            ],
            Self::Gemini => vec![
                hook("SessionStart", SessionStart, None, 10),
                hook("BeforeAgent", UserPromptSubmit, None, 5),
                hook("AfterTool", PostToolUse, Some("write_file|replace|run_shell_command"), 5),
                hook("AfterAgent", Stop, None, 5),
                hook("PreCompress", PreCompact, None, 10),
                hook("SessionEnd", SessionEnd, None, 10),
            ],
            // No prompt-submit, compaction or session-end hooks exist. The first
            // PreInvocation of a turn carries the user's input.
            Self::Antigravity => vec![
                hook("SessionStart", SessionStart, None, 10),
                hook("PreInvocation", UserPromptSubmit, None, 5),
                hook(
                    "PostToolUse",
                    PostToolUse,
                    Some("run_command|write_to_file|replace_file_content|multi_replace_file_content"),
                    5,
                ),
                hook("Stop", Stop, None, 5),
            ],
            // `pre_llm_call` is the only hook that can inject context; it runs on every turn,
            // so session-start only acts on the first. No pre-compaction hook exists, and
            // `pre_verify` (the only "continue" hook) fires only after code edits.
            Self::Hermes => vec![
                hook("pre_llm_call", SessionStart, None, 10),
                hook("pre_llm_call", UserPromptSubmit, None, 5),
                hook("post_tool_call", PostToolUse, Some("write_file|patch|terminal"), 5),
                hook("pre_verify", Stop, None, 5),
                hook("on_session_finalize", SessionEnd, None, 10),
            ],
            // Plugin-based: their plugins call `memdusa hook` directly with canonical payloads.
            Self::OpenCode | Self::Pi => vec![],
            // Installed as git hooks, not harness hooks: `memdusa install git` writes them.
            Self::Git => vec![],
        }
    }

    /// Native payload -> canonical (Claude Code–shaped) hook input, or `None`
    /// when this native event should be ignored.
    pub fn normalize(self, event: HookEvent, input: Value) -> Option<Value> {
        let Value::Object(mut o) = input else { return Some(input) };
        match self {
            Self::ClaudeCode | Self::Git => {}
            Self::Codex => {
                if event == HookEvent::PostToolUse && o.get("tool_name").and_then(Value::as_str) == Some("apply_patch")
                {
                    // edits arrive as raw patch text, not file paths
                    let patch = at(&o, &["tool_input", "command"]).map(command_text).unwrap_or_default();
                    o.insert("tool_name".into(), json!("Edit"));
                    o.insert("tool_input".into(), json!({ "file_paths": patch_files(&patch) }));
                } else if let Some(cmd) = at(&o, &["tool_input", "command"]).filter(|c| c.is_array()).map(command_text)
                {
                    o.insert("tool_input".into(), json!({ "command": cmd }));
                }
            }
            Self::Cursor => {
                rename(&mut o, "conversation_id", "session_id");
                if !o.contains_key("cwd")
                    && let Some(root) = o.get("workspace_roots").and_then(|r| r.get(0)).cloned()
                {
                    o.insert("cwd".into(), root);
                }
                match event {
                    HookEvent::PostToolUse if o.contains_key("file_path") => {
                        let path = o.remove("file_path").unwrap_or_default();
                        o.insert("tool_name".into(), json!("Edit"));
                        o.insert("tool_input".into(), json!({ "file_path": path }));
                    }
                    HookEvent::PostToolUse if o.contains_key("command") => {
                        let cmd = o.remove("command").unwrap_or_default();
                        o.insert("tool_name".into(), json!("Bash"));
                        o.insert("tool_input".into(), json!({ "command": cmd }));
                    }
                    HookEvent::Stop => {
                        let looping = o.get("loop_count").and_then(Value::as_i64).unwrap_or(0) > 0;
                        o.insert("stop_hook_active".into(), json!(looping));
                        cursor_transcript_fields(&mut o, false);
                    }
                    // `cursor-agent -p` fires neither beforeSubmitPrompt, afterAgentResponse nor stop: the
                    // session end is the only place left to pick up the prompt and the agent's last words.
                    HookEvent::SessionEnd => cursor_transcript_fields(&mut o, true),
                    _ => {}
                }
            }
            Self::Antigravity => {
                rename(&mut o, "conversationId", "session_id");
                if let Some(root) = o.get("workspacePaths").and_then(|r| r.get(0)).cloned() {
                    o.entry("cwd").or_insert(root);
                }
                match event {
                    HookEvent::UserPromptSubmit => {
                        // PreInvocation fires before every model call; only the first is the user's prompt
                        if o.get("invocationNum").and_then(Value::as_i64).unwrap_or(0) != 0 {
                            return None;
                        }
                        rename(&mut o, "lastUserInput", "prompt");
                    }
                    HookEvent::PostToolUse => {
                        let name = at(&o, &["toolCall", "name"]).and_then(Value::as_str).unwrap_or_default().to_owned();
                        let args = at(&o, &["toolCall", "args"]).cloned().unwrap_or_default();
                        let (tool, input) = match name.as_str() {
                            "run_command" => ("Bash", json!({ "command": args.get("CommandLine") })),
                            "write_to_file" | "replace_file_content" | "multi_replace_file_content" => {
                                ("Edit", json!({ "file_path": args.get("TargetFile") }))
                            }
                            _ => return None,
                        };
                        o.insert("tool_name".into(), json!(tool));
                        o.insert("tool_input".into(), input);
                    }
                    _ => {}
                }
            }
            Self::Hermes => {
                let extra = o.get("extra").cloned().unwrap_or_default();
                match event {
                    HookEvent::SessionStart => {
                        if extra.get("is_first_turn").and_then(Value::as_bool) != Some(true) {
                            return None;
                        }
                        o.insert("source".into(), json!("startup"));
                    }
                    HookEvent::UserPromptSubmit => {
                        o.insert("prompt".into(), extra.get("user_message").cloned().unwrap_or_default());
                    }
                    HookEvent::PostToolUse => {
                        let input = o.get("tool_input").cloned().unwrap_or_default();
                        let (tool, canonical) = match o.get("tool_name").and_then(Value::as_str) {
                            Some("write_file" | "patch") => ("Edit", json!({ "file_path": input.get("path") })),
                            Some("terminal") => ("Bash", json!({ "command": input.get("command") })),
                            _ => return None,
                        };
                        o.insert("tool_name".into(), json!(tool));
                        o.insert("tool_input".into(), canonical);
                    }
                    HookEvent::SessionEnd => {
                        o.insert("reason".into(), extra.get("reason").cloned().unwrap_or(json!("other")));
                    }
                    HookEvent::Stop => {
                        if let Some(m) = extra.get("assistant_response").filter(|m| m.is_string()) {
                            o.insert("last_assistant_message".into(), m.clone());
                        }
                    }
                    _ => {}
                }
            }
            Self::OpenCode | Self::Pi => {} // their plugins already send canonical payloads
            Self::Gemini => {
                if event == HookEvent::Stop {
                    rename(&mut o, "prompt_response", "last_assistant_message");
                }
                if event == HookEvent::PostToolUse {
                    let canonical = match o.get("tool_name").and_then(Value::as_str) {
                        Some("write_file" | "replace") => Some("Edit"),
                        Some("run_shell_command") => Some("Bash"),
                        _ => None,
                    };
                    if let Some(name) = canonical {
                        o.insert("tool_name".into(), json!(name));
                    }
                }
            }
        }
        Some(Value::Object(o))
    }

    /// Canonical hook output -> the harness's stdout format.
    pub fn render(self, event: HookEvent, output: Option<String>) -> Option<String> {
        let out = output?;
        let Ok(v) = serde_json::from_str::<Value>(&out) else { return Some(out) };
        let context = v.pointer("/hookSpecificOutput/additionalContext").and_then(Value::as_str);
        let reason = v.get("reason").and_then(Value::as_str);
        let rendered = match (self, event) {
            // Claude Code and Codex share the canonical format; OpenCode/pi plugins read it
            (Self::ClaudeCode | Self::Codex | Self::OpenCode | Self::Pi | Self::Git, _) => return Some(out),
            (Self::Gemini, HookEvent::SessionStart) => return Some(out), // same hookSpecificOutput shape
            // BeforeAgent takes the same hookSpecificOutput.additionalContext, named for its event
            (Self::Gemini, HookEvent::UserPromptSubmit) => {
                json!({ "hookSpecificOutput": { "hookEventName": "BeforeAgent", "additionalContext": context? } })
            }
            (Self::Gemini, HookEvent::Stop) => json!({ "decision": "deny", "reason": reason? }),
            // beforeSubmitPrompt can't inject context
            (Self::Cursor, HookEvent::UserPromptSubmit) => return None,
            (Self::Cursor, HookEvent::SessionStart) => json!({ "additional_context": context? }),
            (Self::Antigravity, HookEvent::UserPromptSubmit) => {
                json!({ "injectSteps": [{ "systemMessage": { "systemMessage": context? } }] })
            }
            (Self::Hermes, HookEvent::UserPromptSubmit) => json!({ "context": context? }),
            (Self::Cursor, HookEvent::Stop) => json!({ "followup_message": reason? }),
            (Self::Antigravity, HookEvent::SessionStart) => {
                json!({ "injectSteps": [{ "systemMessage": { "systemMessage": context? } }] })
            }
            (Self::Antigravity, HookEvent::Stop) => json!({ "decision": "continue", "reason": reason? }),
            (Self::Hermes, HookEvent::SessionStart) => json!({ "context": context? }),
            (Self::Hermes, HookEvent::Stop) => json!({ "action": "continue", "message": reason? }),
            _ => return Some(out),
        };
        Some(rendered.to_string())
    }
}

/// Cursor hands hooks the path of the session's transcript (JSONL, one message per line). Fill
/// `last_assistant_message` from it (and `prompt` when `with_prompt`), unless the payload has them.
fn cursor_transcript_fields(o: &mut Map<String, Value>, with_prompt: bool) {
    const MAX_TRANSCRIPT_BYTES: u64 = 32 * 1024 * 1024;
    let Some(path) = o.get("transcript_path").and_then(Value::as_str).filter(|p| !p.is_empty()) else { return };
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_TRANSCRIPT_BYTES) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let (prompt, last) = cursor_transcript_texts(&text);
    if let Some(last) = last
        && !o.contains_key("last_assistant_message")
    {
        o.insert("last_assistant_message".into(), json!(last));
    }
    if with_prompt
        && let Some(prompt) = prompt
        && !o.contains_key("prompt")
    {
        o.insert("prompt".into(), json!(prompt));
    }
}

/// The user's newest request and the agent's newest message that has text, from a Cursor transcript.
fn cursor_transcript_texts(transcript: &str) -> (Option<String>, Option<String>) {
    let (mut prompt, mut last) = (None, None);
    for line in transcript.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let Some(role) = v.get("role").and_then(Value::as_str) else { continue };
        let text = v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        match role {
            // the request sits between <user_query> tags, after a timestamp line
            "user" => {
                let query = text
                    .split_once("<user_query>")
                    .map_or(text, |(_, rest)| rest.split_once("</user_query>").map_or(rest, |(q, _)| q));
                prompt = Some(query.trim().to_owned()).filter(|q| !q.is_empty());
                last = None; // only what the agent said after this request counts
            }
            "assistant" => last = Some(text.to_owned()),
            _ => {}
        }
    }
    (prompt, last)
}

/// Nested lookup in a JSON object: `at(o, &["toolCall", "name"])`.
fn at<'a>(o: &'a Map<String, Value>, path: &[&str]) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    rest.iter().try_fold(o.get(*first)?, |v, k| v.get(*k))
}

fn rename(o: &mut Map<String, Value>, from: &str, to: &str) {
    if !o.contains_key(to)
        && let Some(v) = o.remove(from)
    {
        o.insert(to.into(), v);
    }
}

/// A command given as a string or an argv array.
fn command_text(v: &Value) -> String {
    match v {
        Value::Array(parts) => parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

/// Files touched by an `apply_patch` patch (`*** Add/Update/Delete File: path`, `*** Move to: path`).
pub(crate) fn patch_files(patch: &str) -> Vec<String> {
    let mut files: Vec<String> = vec![];
    for line in patch.lines() {
        let path = ["*** Add File: ", "*** Update File: ", "*** Delete File: ", "*** Move to: "]
            .iter()
            .find_map(|p| line.strip_prefix(p));
        if let Some(p) = path.map(str::trim).filter(|p| !p.is_empty())
            && !files.iter().any(|f| f == p)
        {
            files.push(p.to_owned());
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_patches_become_file_lists() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Add File: src/b.rs\n+z\n*** Update File: src/a.rs\n*** End Patch";
        let v = Harness::Codex
            .normalize(
                HookEvent::PostToolUse,
                json!({"session_id": "s", "tool_name": "apply_patch", "tool_input": {"command": patch}}),
            )
            .unwrap();
        assert_eq!(v["tool_name"], "Edit");
        assert_eq!(v["tool_input"]["file_paths"], json!(["src/a.rs", "src/b.rs"]));
        let bash = Harness::Codex
            .normalize(
                HookEvent::PostToolUse,
                json!({"tool_name": "Bash", "tool_input": {"command": ["cargo", "test"]}}),
            )
            .unwrap();
        assert_eq!(bash["tool_input"]["command"], "cargo test");
    }

    #[test]
    fn cursor_payloads_and_outputs() {
        let v = Harness::Cursor
            .normalize(
                HookEvent::PostToolUse,
                json!({"conversation_id": "c1", "workspace_roots": ["/repo"], "file_path": "/repo/a.ts", "edits": []}),
            )
            .unwrap();
        assert_eq!((v["session_id"].as_str(), v["cwd"].as_str()), (Some("c1"), Some("/repo")));
        assert_eq!(
            (v["tool_name"].as_str(), v["tool_input"]["file_path"].as_str()),
            (Some("Edit"), Some("/repo/a.ts"))
        );
        let shell =
            Harness::Cursor.normalize(HookEvent::PostToolUse, json!({"command": "npm test", "output": "ok"})).unwrap();
        assert_eq!(shell["tool_name"], "Bash");
        let stop = Harness::Cursor.normalize(HookEvent::Stop, json!({"status": "completed", "loop_count": 1})).unwrap();
        assert_eq!(stop["stop_hook_active"], true);

        let start = r#"{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"brief"}}"#;
        assert_eq!(
            Harness::Cursor.render(HookEvent::SessionStart, Some(start.into())).unwrap(),
            r#"{"additional_context":"brief"}"#
        );
        let block = r#"{"decision":"block","reason":"checkpoint first"}"#;
        assert_eq!(
            Harness::Cursor.render(HookEvent::Stop, Some(block.into())).unwrap(),
            r#"{"followup_message":"checkpoint first"}"#
        );
        assert_eq!(
            Harness::Gemini.render(HookEvent::Stop, Some(block.into())).unwrap(),
            r#"{"decision":"deny","reason":"checkpoint first"}"#
        );
        assert_eq!(Harness::Codex.render(HookEvent::Stop, Some(block.into())).unwrap(), block);
        assert_eq!(Harness::Cursor.render(HookEvent::Stop, None), None);
    }

    #[test]
    fn recall_output_per_harness() {
        let out = r#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"recall"}}"#;
        let render = |h: Harness| h.render(HookEvent::UserPromptSubmit, Some(out.into()));
        for h in [Harness::ClaudeCode, Harness::Codex, Harness::OpenCode, Harness::Pi] {
            assert_eq!(render(h).unwrap(), out);
        }
        assert_eq!(
            render(Harness::Gemini).unwrap(),
            r#"{"hookSpecificOutput":{"additionalContext":"recall","hookEventName":"BeforeAgent"}}"#
        );
        assert_eq!(
            render(Harness::Antigravity).unwrap(),
            r#"{"injectSteps":[{"systemMessage":{"systemMessage":"recall"}}]}"#
        );
        assert_eq!(render(Harness::Hermes).unwrap(), r#"{"context":"recall"}"#);
        assert_eq!(render(Harness::Cursor), None);
        assert_eq!(Harness::Hermes.render(HookEvent::UserPromptSubmit, None), None);
    }

    #[test]
    fn gemini_tool_names() {
        let v = Harness::Gemini
            .normalize(
                HookEvent::PostToolUse,
                json!({"tool_name": "replace", "tool_input": {"file_path": "/r/a.py", "old_string": "a"}}),
            )
            .unwrap();
        assert_eq!(v["tool_name"], "Edit");
        let v = Harness::Gemini
            .normalize(
                HookEvent::PostToolUse,
                json!({"tool_name": "run_shell_command", "tool_input": {"command": "ls"}}),
            )
            .unwrap();
        assert_eq!(v["tool_name"], "Bash");
    }

    #[test]
    fn gemini_and_hermes_last_message() {
        let v = Harness::Gemini
            .normalize(HookEvent::Stop, json!({"session_id": "g", "prompt": "q", "prompt_response": "All done."}))
            .unwrap();
        assert_eq!(v["last_assistant_message"], "All done.");
        let v = Harness::Hermes
            .normalize(HookEvent::Stop, json!({"session_id": "h", "extra": {"assistant_response": "Fixed it."}}))
            .unwrap();
        assert_eq!(v["last_assistant_message"], "Fixed it.");
        let v = Harness::Hermes.normalize(HookEvent::Stop, json!({"session_id": "h", "extra": {}})).unwrap();
        assert!(v.get("last_assistant_message").is_none());
    }

    #[test]
    fn claude_code_registers_post_compact_but_codex_does_not() {
        let has =
            |h: Harness| h.events().iter().any(|e| e.native == "PostCompact" && e.event == HookEvent::PostCompact);
        assert!(has(Harness::ClaudeCode));
        assert!(!has(Harness::Codex));
    }

    #[test]
    fn antigravity_payloads_and_outputs() {
        let base = json!({"conversationId": "c9", "workspacePaths": ["/ws"], "lastUserInput": "add refunds"});
        let mut first = base.clone();
        first["invocationNum"] = json!(0);
        let v = Harness::Antigravity.normalize(HookEvent::UserPromptSubmit, first).unwrap();
        assert_eq!(
            (v["session_id"].as_str(), v["cwd"].as_str(), v["prompt"].as_str()),
            (Some("c9"), Some("/ws"), Some("add refunds"))
        );
        let mut later = base.clone();
        later["invocationNum"] = json!(3);
        assert!(
            Harness::Antigravity.normalize(HookEvent::UserPromptSubmit, later).is_none(),
            "only the turn's first call"
        );

        let mut edit = base.clone();
        edit["toolCall"] = json!({"name": "replace_file_content", "args": {"TargetFile": "/ws/a.go"}});
        let v = Harness::Antigravity.normalize(HookEvent::PostToolUse, edit).unwrap();
        assert_eq!((v["tool_name"].as_str(), v["tool_input"]["file_path"].as_str()), (Some("Edit"), Some("/ws/a.go")));
        let mut cmd = base.clone();
        cmd["toolCall"] = json!({"name": "run_command", "args": {"CommandLine": "go test ./..."}});
        assert_eq!(
            Harness::Antigravity.normalize(HookEvent::PostToolUse, cmd).unwrap()["tool_input"]["command"],
            "go test ./..."
        );

        let start = r#"{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"brief"}}"#;
        assert_eq!(
            Harness::Antigravity.render(HookEvent::SessionStart, Some(start.into())).unwrap(),
            r#"{"injectSteps":[{"systemMessage":{"systemMessage":"brief"}}]}"#
        );
        let block = r#"{"decision":"block","reason":"checkpoint first"}"#;
        assert_eq!(
            Harness::Antigravity.render(HookEvent::Stop, Some(block.into())).unwrap(),
            r#"{"decision":"continue","reason":"checkpoint first"}"#
        );
    }

    #[test]
    fn hermes_payloads_and_outputs() {
        let first = json!({"hook_event_name": "pre_llm_call", "session_id": "h1", "cwd": "/r",
                           "extra": {"is_first_turn": true, "user_message": "fix the build"}});
        let later =
            json!({"session_id": "h1", "cwd": "/r", "extra": {"is_first_turn": false, "user_message": "and tests"}});
        assert!(Harness::Hermes.normalize(HookEvent::SessionStart, first.clone()).is_some());
        assert!(Harness::Hermes.normalize(HookEvent::SessionStart, later.clone()).is_none(), "brief only on turn one");
        assert_eq!(Harness::Hermes.normalize(HookEvent::UserPromptSubmit, later).unwrap()["prompt"], "and tests");
        let edit =
            json!({"tool_name": "patch", "tool_input": {"path": "/r/a.py", "old_string": "x"}, "session_id": "h1"});
        let v = Harness::Hermes.normalize(HookEvent::PostToolUse, edit).unwrap();
        assert_eq!((v["tool_name"].as_str(), v["tool_input"]["file_path"].as_str()), (Some("Edit"), Some("/r/a.py")));
        let start = r#"{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"brief"}}"#;
        assert_eq!(
            Harness::Hermes.render(HookEvent::SessionStart, Some(start.into())).unwrap(),
            r#"{"context":"brief"}"#
        );
        let block = r#"{"decision":"block","reason":"checkpoint"}"#;
        assert_eq!(
            Harness::Hermes.render(HookEvent::Stop, Some(block.into())).unwrap(),
            r#"{"action":"continue","message":"checkpoint"}"#
        );
    }

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/harness");

    /// Real `cursor-agent -p` sessionEnd payload (Cursor 2026.11.22): its transcript holds the prompt and last words.
    #[test]
    fn cursor_session_end_reads_prompt_and_last_words_from_the_transcript() {
        let payload = include_str!("../tests/fixtures/harness/cursor-session-end.json")
            .replace("__TRANSCRIPT__", &format!("{FIXTURES}/cursor-transcript.jsonl"));
        let input: Value = serde_json::from_str(&payload).unwrap();
        let v = Harness::Cursor.normalize(HookEvent::SessionEnd, input.clone()).unwrap();
        assert_eq!(v["last_assistant_message"], "ok", "text of the final assistant message only");
        assert_eq!(v["prompt"], "Run the shell command echo hi then reply with just: ok");
        assert_eq!(v["session_id"], "d6c39eb6-3ad6-49e7-add5-6842d178a32b");
        assert_eq!(v["cwd"], "/work/repo");
        assert_eq!(v["reason"], "completed");

        // stop (interactive sessions) gets the last words but never the prompt: beforeSubmitPrompt covers that
        let stop = Harness::Cursor.normalize(HookEvent::Stop, input.clone()).unwrap();
        assert_eq!(stop["last_assistant_message"], "ok");
        assert!(stop.get("prompt").is_none());

        // an unreadable transcript is not an error
        let mut gone = input;
        gone["transcript_path"] = json!("/nonexistent/transcript.jsonl");
        let v = Harness::Cursor.normalize(HookEvent::SessionEnd, gone).unwrap();
        assert!(v.get("last_assistant_message").is_none() && v.get("prompt").is_none());
    }

    #[test]
    fn cursor_transcript_keeps_the_last_message_after_the_newest_request() {
        let t = concat!(
            r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nfirst\n</user_query>"}]}}"#,
            "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"answer one"}]}}"#,
            "\n",
            "not json\n",
            r#"{"role":"user","message":{"content":[{"type":"text","text":"plain second request"}]}}"#,
            "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"Shell","input":{}}]}}"#,
            "\n",
        );
        let (prompt, last) = cursor_transcript_texts(t);
        assert_eq!(prompt.as_deref(), Some("plain second request"));
        assert_eq!(last, None, "answer one belongs to the previous request");
    }
}
