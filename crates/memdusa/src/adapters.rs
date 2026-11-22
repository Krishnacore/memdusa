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
        }
    }

    /// Native payload -> canonical (Claude Code–shaped) hook input, or `None`
    /// when this native event should be ignored.
    pub fn normalize(self, event: HookEvent, input: Value) -> Option<Value> {
        let Value::Object(mut o) = input else { return Some(input) };
        match self {
            Self::ClaudeCode => {}
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
                    }
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
            Self::Gemini => {
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
            // Claude Code and Codex share the canonical format
            (Self::ClaudeCode | Self::Codex, _) => return Some(out),
            (Self::Gemini, HookEvent::SessionStart) => return Some(out), // same hookSpecificOutput shape
            (Self::Gemini, HookEvent::Stop) => json!({ "decision": "deny", "reason": reason? }),
            (Self::Cursor, HookEvent::SessionStart) => json!({ "additional_context": context? }),
            (Self::Cursor, HookEvent::Stop) => json!({ "followup_message": reason? }),
            (Self::Antigravity, HookEvent::SessionStart) => {
                json!({ "injectSteps": [{ "systemMessage": { "systemMessage": context? } }] })
            }
            (Self::Antigravity, HookEvent::Stop) => json!({ "decision": "continue", "reason": reason? }),
            _ => return Some(out),
        };
        Some(rendered.to_string())
    }
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
fn patch_files(patch: &str) -> Vec<String> {
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
}
