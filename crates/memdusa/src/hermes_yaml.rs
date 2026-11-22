//! Merging memdusa's hooks and MCP server into Hermes' `config.yaml` by
//! targeted text insertion: the user's file (often thousands of lines with
//! comments) is never re-serialized. Every line memdusa adds sits inside a
//! `# >>> memdusa` / `# <<< memdusa` block, so `uninstall` deletes exactly
//! those blocks and restores the original byte for byte.
//!
//! A bare empty value (`hooks: {}`, `events: []`, `mcp_servers: null`) can't
//! take children, so that one line is replaced by a block that carries the
//! original as a `# memdusa-replaced: <line>` comment, restored on uninstall.

use crate::adapters::Harness;

pub const BEGIN: &str = "# >>> memdusa (managed by `memdusa install hermes`; remove with `memdusa uninstall hermes`)";
pub const END: &str = "# <<< memdusa";
const REPLACED: &str = "# memdusa-replaced: ";

/// Top-level `key:` of a column-0 line, with the text after the colon.
fn top_key(line: &str) -> Option<(&str, &str)> {
    let first = line.chars().next()?;
    if first.is_whitespace() || matches!(first, '#' | '-' | '%') {
        return None;
    }
    let colon =
        line.char_indices().find(|&(i, c)| c == ':' && line[i + 1..].chars().next().is_none_or(char::is_whitespace))?.0;
    Some((line[..colon].trim().trim_matches(['"', '\'']), &line[colon + 1..]))
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_blank_or_comment(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

/// Value after `key:` without a trailing comment.
fn value_of(rest: &str) -> &str {
    let rest = rest.trim();
    if rest.starts_with('#') { "" } else { rest.split(" #").next().unwrap_or("").trim() }
}

/// What to do with the value on a `key:` line.
enum Value {
    /// Children follow on the next lines (or none yet).
    Block,
    /// `{}`, `[]`, `~`, `null`: replace the line.
    Empty,
    /// Anything else (inline content): can't be merged safely.
    Inline,
}

fn classify(rest: &str) -> Value {
    match value_of(rest) {
        "" => Value::Block,
        "{}" | "[]" | "~" | "null" | "Null" | "NULL" => Value::Empty,
        _ => Value::Inline,
    }
}

struct Insert {
    /// Insert before this line index (`lines.len()` appends).
    at: usize,
    /// The line at `at` is replaced (and kept as a comment inside the block).
    replace: bool,
    /// Sorts after other inserts at the same position: a new key must not
    /// land between an existing key and the items appended to it.
    late: bool,
    body: Vec<String>,
}

fn yaml_str(v: &str) -> String {
    format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
}

fn hook_items(bin: &str, db: Option<&str>, native: &str, ind: usize) -> Vec<String> {
    let pad = " ".repeat(ind);
    let mut out = vec![];
    for nh in Harness::Hermes.events().iter().filter(|e| e.native == native) {
        out.push(format!(
            "{pad}- command: {}",
            yaml_str(&crate::install::hook_command(bin, Harness::Hermes, nh.event, db))
        ));
        if let Some(m) = nh.matcher {
            out.push(format!("{pad}  matcher: {}", yaml_str(m)));
        }
        out.push(format!("{pad}  timeout: {}", nh.timeout_secs));
    }
    out
}

/// Distinct native event names in declaration order.
fn native_events() -> Vec<&'static str> {
    let mut seen: Vec<&'static str> = vec![];
    for nh in Harness::Hermes.events() {
        if !seen.contains(&nh.native) {
            seen.push(nh.native);
        }
    }
    seen
}

fn mcp_entry(bin: &str, db: Option<&str>, ind: usize) -> Vec<String> {
    let pad = " ".repeat(ind);
    let mut args = vec![yaml_str("mcp"), yaml_str("--agent"), yaml_str("hermes")];
    if let Some(u) = db {
        args.extend([yaml_str("--database-url"), yaml_str(u)]);
    }
    vec![
        format!("{pad}memdusa:"),
        format!("{pad}  command: {}", yaml_str(bin)),
        format!("{pad}  args: [{}]", args.join(", ")),
    ]
}

/// A top-level section: where its key is, where it ends, where its last
/// content line is, and how its children are indented.
struct Section {
    key: usize,
    end: usize,
    last: usize,
    child: usize,
}

fn find_section(lines: &[&str], name: &str) -> Option<Section> {
    let key = lines.iter().position(|l| top_key(l).is_some_and(|(k, _)| k == name))?;
    let end = (key + 1..lines.len()).find(|&i| top_key(lines[i]).is_some()).unwrap_or(lines.len());
    let is_content =
        |l: &str| !l.trim().is_empty() && (l.starts_with(' ') || l.starts_with('\t') || l.starts_with('-'));
    let last = (key + 1..end).rev().find(|&i| is_content(lines[i])).unwrap_or(key);
    let child = (key + 1..end)
        .find(|&i| !is_blank_or_comment(lines[i]) && is_content(lines[i]))
        .map_or(2, |i| indent(lines[i]));
    Some(Section { key, end, last, child })
}

fn block(lines: Vec<String>, replaced: Option<&str>) -> Vec<String> {
    let mut out = vec![BEGIN.to_owned()];
    if let Some(r) = replaced {
        out.push(format!("{REPLACED}{}", r.trim_end_matches('\n')));
    }
    out.extend(lines);
    out.push(END.to_owned());
    out
}

/// Remove our blocks (restoring any replaced line).
fn strip(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        let t = line.trim_end();
        if !inside && t == BEGIN {
            inside = true;
        } else if inside && t == END {
            inside = false;
        } else if inside {
            if let Some(orig) = t.strip_prefix(REPLACED) {
                out.push_str(orig);
                out.push('\n');
            }
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Add or remove memdusa's hooks (and MCP server) in Hermes' config.yaml.
/// `Err(why)` only when an existing section is in a shape we won't edit
/// (inline content such as `hooks: {a: b}`).
pub fn merge(text: &str, bin: &str, db: Option<&str>, mcp: bool, uninstall: bool) -> Result<String, String> {
    let stripped = strip(text);
    if uninstall {
        return Ok(stripped);
    }
    let lines: Vec<&str> = stripped.split_inclusive('\n').collect();
    let n = lines.len();
    let mut inserts: Vec<Insert> = vec![];

    // ---- hooks ----
    match find_section(&lines, "hooks") {
        None => {
            let mut body = vec!["hooks:".to_owned()];
            for ev in native_events() {
                body.push(format!("  {ev}:"));
                body.extend(hook_items(bin, db, ev, 4));
            }
            inserts.push(Insert { at: n, replace: false, late: true, body: block(body, None) });
        }
        Some(s) => {
            let key_rest = top_key(lines[s.key]).map_or("", |(_, r)| r);
            match classify(key_rest) {
                Value::Inline => return Err("`hooks:` has inline content".into()),
                Value::Empty => {
                    let mut body = vec!["hooks:".to_owned()];
                    for ev in native_events() {
                        body.push(format!("  {ev}:"));
                        body.extend(hook_items(bin, db, ev, 4));
                    }
                    inserts.push(Insert {
                        at: s.key,
                        replace: true,
                        late: false,
                        body: block(body, Some(lines[s.key])),
                    });
                }
                Value::Block => {
                    for ev in native_events() {
                        let prefix = format!("{ev}:");
                        let found = (s.key + 1..s.end)
                            .find(|&i| indent(lines[i]) == s.child && lines[i].trim_start().starts_with(&prefix));
                        let pad = " ".repeat(s.child);
                        let Some(i) = found else {
                            let mut body = vec![format!("{pad}{ev}:")];
                            body.extend(hook_items(bin, db, ev, s.child + 2));
                            inserts.push(Insert {
                                at: s.last + 1,
                                replace: false,
                                late: true,
                                body: block(body, None),
                            });
                            continue;
                        };
                        let rest = lines[i].trim_start().strip_prefix(&prefix).unwrap_or("");
                        // the event's subsection: until the next key at the same or lower indent
                        let end = (i + 1..s.end)
                            .find(|&j| {
                                !is_blank_or_comment(lines[j])
                                    && (indent(lines[j]) < s.child
                                        || (indent(lines[j]) == s.child && !lines[j].trim_start().starts_with('-')))
                            })
                            .unwrap_or(s.end);
                        match classify(rest) {
                            Value::Inline => return Err(format!("`hooks.{ev}` has inline content")),
                            Value::Empty => {
                                let mut body = vec![format!("{pad}{ev}:")];
                                body.extend(hook_items(bin, db, ev, s.child + 2));
                                inserts.push(Insert {
                                    at: i,
                                    replace: true,
                                    late: false,
                                    body: block(body, Some(lines[i])),
                                });
                            }
                            Value::Block => {
                                let item_indent = (i + 1..end)
                                    .find(|&j| lines[j].trim_start().starts_with('-') && !is_blank_or_comment(lines[j]))
                                    .map_or(s.child + 2, |j| indent(lines[j]));
                                let last = (i + 1..end)
                                    .rev()
                                    .find(|&j| {
                                        !lines[j].trim().is_empty()
                                            && (indent(lines[j]) > 0 || lines[j].starts_with('-'))
                                    })
                                    .unwrap_or(i);
                                inserts.push(Insert {
                                    at: last + 1,
                                    replace: false,
                                    late: false,
                                    body: block(hook_items(bin, db, ev, item_indent), None),
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    // ---- mcp_servers ----
    if mcp {
        match find_section(&lines, "mcp_servers") {
            None => {
                let mut body = vec!["mcp_servers:".to_owned()];
                body.extend(mcp_entry(bin, db, 2));
                inserts.push(Insert { at: n, replace: false, late: true, body: block(body, None) });
            }
            Some(s) => {
                let key_rest = top_key(lines[s.key]).map_or("", |(_, r)| r);
                match classify(key_rest) {
                    Value::Inline => return Err("`mcp_servers:` has inline content".into()),
                    Value::Empty => {
                        let mut body = vec!["mcp_servers:".to_owned()];
                        body.extend(mcp_entry(bin, db, 2));
                        inserts.push(Insert {
                            at: s.key,
                            replace: true,
                            late: false,
                            body: block(body, Some(lines[s.key])),
                        });
                    }
                    Value::Block => inserts.push(Insert {
                        at: s.last + 1,
                        replace: false,
                        late: false,
                        body: block(mcp_entry(bin, db, s.child), None),
                    }),
                }
            }
        }
    }

    // Items appended to an existing key, then new keys, then replaced lines (which start a
    // new key themselves). Stable: the same rank at one position keeps declaration order.
    inserts.sort_by_key(|i| (i.at, if i.replace { 2 } else { u8::from(i.late) }));
    let mut out = String::with_capacity(stripped.len() + 1024);
    let mut next = 0;
    for i in 0..=n {
        let line = lines.get(i).copied();
        let mut skip = false;
        while next < inserts.len() && inserts[next].at == i {
            if i == n && !out.is_empty() && !out.ends_with('\n') {
                out.push('\n'); // file lacked a final newline
            }
            for l in &inserts[next].body {
                out.push_str(l);
                out.push('\n');
            }
            skip |= inserts[next].replace;
            next += 1;
        }
        if let Some(l) = line.filter(|_| !skip) {
            out.push_str(l);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yaml_rust2::YamlLoader;

    const BIN: &str = "/usr/local/bin/memdusa";

    fn parse(text: &str) -> yaml_rust2::Yaml {
        YamlLoader::load_from_str(text).unwrap_or_else(|e| panic!("invalid YAML: {e}\n{text}")).remove(0)
    }

    /// Looks like a real Hermes config: nested maps, lists, comments, 2+ MCP servers, a foreign hook.
    const REAL: &str = "\
# Hermes Agent configuration
model:
  default: \"anthropic/claude-opus\"
  provider: auto   # chosen at setup

terminal:
  backend: local
  cwd: \".\"

toolsets:
  - hermes-cli
  - web

hooks:
  # my own hooks
  pre_llm_call:
    - command: \"/home/me/bin/inject-context.sh\"
      timeout: 5
  on_session_end:
  - command: \"/home/me/bin/bye.sh\"

mcp_servers:
  github:
    command: npx
    args: [\"-y\", \"@modelcontextprotocol/server-github\"]
    env:
      GITHUB_TOKEN: \"x\"
  filesystem:
    command: npx
    args: [\"-y\", \"@modelcontextprotocol/server-filesystem\", \"/tmp\"]

# trailing comment about display
display:
  compact: false
";

    #[test]
    fn merges_into_existing_hooks_and_mcp_servers() {
        let out = merge(REAL, BIN, None, true, false).unwrap();
        let doc = parse(&out);
        // user's own entries untouched
        assert_eq!(doc["hooks"]["pre_llm_call"][0]["command"].as_str(), Some("/home/me/bin/inject-context.sh"));
        assert_eq!(doc["hooks"]["on_session_end"][0]["command"].as_str(), Some("/home/me/bin/bye.sh"));
        assert_eq!(doc["mcp_servers"]["github"]["command"].as_str(), Some("npx"));
        assert_eq!(doc["mcp_servers"]["filesystem"]["command"].as_str(), Some("npx"));
        assert_eq!(doc["display"]["compact"].as_bool(), Some(false));
        // ours merged into the same event list, new events created
        let pre = doc["hooks"]["pre_llm_call"].as_vec().unwrap();
        assert_eq!(pre.len(), 3, "user's hook plus our two pre_llm_call hooks");
        assert_eq!(pre[1]["command"].as_str(), Some("/usr/local/bin/memdusa hook session-start --harness hermes"));
        assert_eq!(doc["hooks"]["post_tool_call"][0]["matcher"].as_str(), Some("write_file|patch|terminal"));
        assert_eq!(doc["hooks"]["pre_verify"][0]["timeout"].as_i64(), Some(5));
        assert_eq!(doc["hooks"]["on_session_finalize"][0]["timeout"].as_i64(), Some(10));
        assert_eq!(doc["mcp_servers"]["memdusa"]["command"].as_str(), Some(BIN));
        assert_eq!(doc["mcp_servers"]["memdusa"]["args"][2].as_str(), Some("hermes"));
        assert!(out.contains("      timeout: 5\n"), "{out}");
        // existing text only gained lines
        for l in REAL.lines() {
            assert!(out.lines().any(|o| o == l), "lost line {l:?}");
        }
        // idempotent, and uninstall restores the original byte for byte
        assert_eq!(merge(&out, BIN, None, true, false).unwrap(), out);
        assert_eq!(merge(&out, BIN, None, true, true).unwrap(), REAL);
    }

    #[test]
    fn appends_missing_sections_to_a_fresh_config() {
        let base = "model: hermes-4\nterminal:\n  backend: local\n";
        let out = merge(base, BIN, None, true, false).unwrap();
        assert!(out.starts_with(base));
        let doc = parse(&out);
        assert_eq!(doc["hooks"]["pre_llm_call"].as_vec().unwrap().len(), 2);
        assert_eq!(doc["mcp_servers"]["memdusa"]["command"].as_str(), Some(BIN));
        assert_eq!(out.matches("pre_llm_call:").count(), 1);
        assert_eq!(merge(&out, BIN, None, true, false).unwrap(), out);
        assert_eq!(merge(&out, BIN, None, true, true).unwrap(), base);
        // empty file too
        let from_empty = merge("", BIN, None, false, false).unwrap();
        parse(&from_empty);
        assert_eq!(merge(&from_empty, BIN, None, false, true).unwrap(), "");
    }

    #[test]
    fn only_mcp_servers_present() {
        let base = "mcp_servers:\n    one:\n        command: x\nother: 1\n";
        let out = merge(base, BIN, None, true, false).unwrap();
        let doc = parse(&out);
        assert_eq!(doc["mcp_servers"]["one"]["command"].as_str(), Some("x"));
        assert_eq!(doc["mcp_servers"]["memdusa"]["command"].as_str(), Some(BIN), "{out}");
        assert!(out.contains("\n        command: \"/usr/local/bin/memdusa\"\n") || out.contains("    memdusa:\n"));
        assert_eq!(doc["other"].as_i64(), Some(1));
        assert!(doc["hooks"]["pre_llm_call"].as_vec().is_some());
        assert_eq!(merge(&out, BIN, None, true, true).unwrap(), base);
    }

    #[test]
    fn empty_values_are_replaced_and_restored() {
        for (base, mcp) in [
            ("hooks: {}\nmodel: x\n", false),
            ("model: x\nhooks:\nmcp_servers: ~\n", true),
            ("hooks:\n  pre_llm_call: []\n  pre_verify:\nmcp_servers: {}  # none yet\n", true),
        ] {
            let out = merge(base, BIN, None, mcp, false).unwrap();
            let doc = parse(&out);
            assert_eq!(doc["hooks"]["pre_llm_call"].as_vec().unwrap().len(), 2, "{out}");
            assert!(doc["hooks"]["on_session_finalize"].as_vec().is_some(), "{out}");
            if mcp {
                assert_eq!(doc["mcp_servers"]["memdusa"]["command"].as_str(), Some(BIN), "{out}");
            }
            assert_eq!(merge(&out, BIN, None, mcp, false).unwrap(), out, "idempotent: {base}");
            assert_eq!(merge(&out, BIN, None, mcp, true).unwrap(), base, "restores: {base}");
        }
    }

    #[test]
    fn inline_content_is_refused_not_mangled() {
        assert!(merge("hooks: {a: b}\n", BIN, None, false, false).is_err());
        assert!(merge("mcp_servers: {a: {command: x}}\n", BIN, None, true, false).is_err());
    }

    #[test]
    fn database_url_reaches_hooks_and_mcp() {
        let out = merge(REAL, BIN, Some("sqlite:///x/memdusa.db"), true, false).unwrap();
        let doc = parse(&out);
        assert_eq!(
            doc["hooks"]["pre_verify"][0]["command"].as_str(),
            Some("/usr/local/bin/memdusa hook stop --harness hermes --database-url sqlite:///x/memdusa.db")
        );
        let args = doc["mcp_servers"]["memdusa"]["args"].as_vec().unwrap();
        assert_eq!(args[3].as_str(), Some("--database-url"));
        assert_eq!(args[4].as_str(), Some("sqlite:///x/memdusa.db"));
        // a URL with shell-special characters is shell-quoted, then YAML-escaped
        let pg = merge("", BIN, Some("postgres://u:p w@h/db?x=1"), false, false).unwrap();
        let doc = parse(&pg);
        assert_eq!(
            doc["hooks"]["pre_verify"][0]["command"].as_str(),
            Some("/usr/local/bin/memdusa hook stop --harness hermes --database-url 'postgres://u:p w@h/db?x=1'")
        );
    }

    #[test]
    fn upgrades_a_previous_install_in_place() {
        let first = merge(REAL, "/old/memdusa", None, true, false).unwrap();
        let second = merge(&first, BIN, None, true, false).unwrap();
        assert!(!second.contains("/old/memdusa"));
        assert_eq!(second, merge(REAL, BIN, None, true, false).unwrap());
    }
}
