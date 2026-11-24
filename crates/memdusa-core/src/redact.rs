//! Secret redaction, applied to everything before it is stored.
//!
//! Hooks capture raw prompts and shell commands, and agents paste things into
//! memories and step outputs, so secrets *will* show up. Redaction replaces
//! them with `[REDACTED:<kind>]` markers while keeping the surrounding text
//! (e.g. the variable name in `STRIPE_KEY=…`) so the record stays useful.
//!
//! Precision matters as much as recall: an over-eager redactor that eats
//! `max_tokens=1000` or `author=bob` makes the journal useless, so assignment
//! rules skip variable references, numbers and short values.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde_json::Value;

use crate::{Error, Result};

struct Rule {
    kind: &'static str,
    re: Regex,
    /// Capture group holding the secret; everything else in the match is kept.
    /// 0 = replace the whole match.
    secret_group: usize,
    /// Apply the "does this value look like a secret" check (assignment-style rules).
    check_value: bool,
}

fn rule(kind: &'static str, re: &str, secret_group: usize, check_value: bool) -> Rule {
    Rule { kind, re: Regex::new(re).expect("built-in redaction pattern"), secret_group, check_value }
}

/// Names that mark a value as secret in `NAME=value`, `name: value`, JSON keys and CLI flags.
const SECRET_NAME: &str = r"(?:password|passwd|pwd|passphrase|secret|token|api[_-]?key|apikey|access[_-]?key|private[_-]?key|client[_-]?secret|credentials?|session[_-]?key)";

static BUILTIN: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        // multi-line blocks first, so later rules don't chew on their insides
        rule(
            "private_key",
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|$)",
            0,
            false,
        ),
        // scheme://user:PASSWORD@host
        rule("url_password", r"\b[a-zA-Z][a-zA-Z0-9+.-]*://[^\s/:@]+:([^\s@/]+)@", 1, false),
        rule("bearer", r"(?i)\bbearer\s+([A-Za-z0-9._~+/=-]{16,})", 1, false),
        // Authorization: Basic <base64> (including inside `curl -H '...'`)
        rule("basic_auth", r"(?i)\bAuthorization\s*:\s*Basic\s+([A-Za-z0-9+/=]{6,})", 1, false),
        // provider-specific token formats (high precision)
        rule("anthropic_key", r"\bsk-ant-[A-Za-z0-9_-]{20,}", 0, false),
        rule("openai_key", r"\bsk-(?:proj-|svcacct-)?[A-Za-z0-9_-]{20,}", 0, false),
        rule("aws_access_key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", 0, false),
        rule("github_token", r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})", 0, false),
        rule("gitlab_token", r"\bglpat-[A-Za-z0-9_-]{20,}", 0, false),
        rule("stripe_key", r"\b(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,}|\bwhsec_[A-Za-z0-9]{16,}", 0, false),
        rule("slack_token", r"\bxox[abposr]-[A-Za-z0-9-]{10,}", 0, false),
        rule("google_api_key", r"\bAIza[0-9A-Za-z_-]{35}", 0, false),
        rule("npm_token", r"\bnpm_[A-Za-z0-9]{36}\b", 0, false),
        rule("memdusa_key", r"\bmdk_[0-9a-f]{64}\b", 0, false),
        rule("jwt", r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}", 0, false),
        // --password hunter2, --api-key=abc
        rule(
            "cli_flag",
            &format!(r#"(?i)--[a-z0-9-]*{SECRET_NAME}[a-z0-9-]*(?:=|\s+)("[^"\n]*"|'[^'\n]*'|[^\s"']+)"#),
            1,
            true,
        ),
        // STRIPE_SECRET_KEY=..., "password": "...", db_password: ...
        rule(
            "assignment",
            &format!(r#"(?i)\b[a-z0-9_.-]*{SECRET_NAME}[a-z0-9_.-]*["']?\s*[:=]\s*("[^"\n]*"|'[^'\n]*'|[^\s"',;]+)"#),
            1,
            true,
        ),
    ]
});

static SECRET_KEY_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r"(?i){SECRET_NAME}")).unwrap());

/// Does an assigned value look like an actual secret (rather than a variable
/// reference, a number, a placeholder or an already-redacted marker)?
fn looks_secret(raw: &str) -> bool {
    let v = raw.trim_matches(|c| c == '"' || c == '\'');
    v.len() >= 6
        && !v.starts_with('$')
        // paths (`--token-file ~/.config/token`, `KEY_PATH=/run/secrets/x`) point at secrets, they aren't secrets
        && !["/", "~/", "./", "../"].iter().any(|p| v.starts_with(p))
        && !v.starts_with("[REDACTED")
        && !v.starts_with("<")
        && !v.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '_' || c == '-')
        && !matches!(v.to_ascii_lowercase().as_str(), "true" | "false" | "null" | "none" | "undefined" | "required")
}

#[derive(Default)]
pub struct Redactor {
    enabled: bool,
    custom: Vec<Regex>,
}

impl Redactor {
    /// Built-in rules plus optional extra regexes (whole match is redacted).
    pub fn new(extra_patterns: &[String]) -> Result<Self> {
        let custom = extra_patterns
            .iter()
            .filter(|p| !p.trim().is_empty())
            .map(|p| Regex::new(p.trim()).map_err(|e| Error::Invalid(format!("bad redaction pattern {p:?}: {e}"))))
            .collect::<Result<_>>()?;
        Ok(Self { enabled: true, custom })
    }

    /// Store everything verbatim.
    pub fn disabled() -> Self {
        Self { enabled: false, custom: vec![] }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn text<'a>(&self, input: &'a str) -> Cow<'a, str> {
        if !self.enabled || input.is_empty() {
            return Cow::Borrowed(input);
        }
        let mut s = Cow::Borrowed(input);
        for r in BUILTIN.iter() {
            if !r.re.is_match(&s) {
                continue;
            }
            let replaced =
                r.re.replace_all(&s, |c: &Captures| {
                    let whole = c.get(0).unwrap();
                    let Some(secret) = c.get(r.secret_group) else { return whole.as_str().to_owned() };
                    if r.check_value && !looks_secret(secret.as_str()) {
                        return whole.as_str().to_owned();
                    }
                    let (start, end) = (secret.start() - whole.start(), secret.end() - whole.start());
                    format!("{}[REDACTED:{}]{}", &whole.as_str()[..start], r.kind, &whole.as_str()[end..])
                })
                .into_owned();
            s = Cow::Owned(replaced);
        }
        for re in &self.custom {
            if re.is_match(&s) {
                s = Cow::Owned(re.replace_all(&s, "[REDACTED:custom]").into_owned());
            }
        }
        s
    }

    pub fn string(&self, input: String) -> String {
        match self.text(&input) {
            Cow::Borrowed(_) => input,
            Cow::Owned(s) => s,
        }
    }

    pub fn opt(&self, input: Option<String>) -> Option<String> {
        input.map(|s| self.string(s))
    }

    pub fn list(&self, input: Vec<String>) -> Vec<String> {
        input.into_iter().map(|s| self.string(s)).collect()
    }

    /// Redact every string in a JSON value. String values under secret-looking
    /// keys (`"password": …`, `"api_key": …`) are redacted entirely.
    pub fn json(&self, v: Value) -> Value {
        if !self.enabled {
            return v;
        }
        match v {
            Value::String(s) => Value::String(self.string(s)),
            Value::Array(a) => Value::Array(a.into_iter().map(|v| self.json(v)).collect()),
            Value::Object(o) => Value::Object(
                o.into_iter()
                    .map(|(k, v)| {
                        let v = match v {
                            Value::String(s) if SECRET_KEY_NAME.is_match(&k) && looks_secret(&s) => {
                                Value::String("[REDACTED:field]".into())
                            }
                            other => self.json(other),
                        };
                        (k, v)
                    })
                    .collect(),
            ),
            other => other,
        }
    }

    pub fn opt_json(&self, v: Option<Value>) -> Option<Value> {
        v.map(|v| self.json(v))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn r(s: &str) -> String {
        Redactor::new(&[]).unwrap().text(s).into_owned()
    }

    #[test]
    fn redacts_known_secret_formats() {
        let cases = [
            ("export STRIPE_SECRET_KEY=sk_live_51HxQabcdefghijklmnop", "export STRIPE_SECRET_KEY=[REDACTED:"),
            ("curl -H 'Authorization: Bearer abcdefghijklmnopqrstuvwxyz123456'", "Bearer [REDACTED:bearer]"),
            (
                "git remote add origin https://bob:hunter2hunter2@github.com/x/y",
                "https://bob:[REDACTED:url_password]@github.com",
            ),
            ("DATABASE_URL=postgres://app:s3cr3tpass@db:5432/prod", "postgres://app:[REDACTED:url_password]@db"),
            ("token ghp_abcdefghijklmnopqrstuvwxyz0123456789", "token [REDACTED:github_token]"),
            ("key AKIAIOSFODNN7EXAMPLE here", "key [REDACTED:aws_access_key] here"),
            ("ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnopqrstuv", "ANTHROPIC_API_KEY=[REDACTED:"),
            ("use sk-proj-abcdefghijklmnopqrstuvwxyz", "use [REDACTED:openai_key]"),
            ("mysql --password=hunter2hunter2 -u root", "--password=[REDACTED:cli_flag]"),
            (r#"{"password": "correct horse battery"}"#, r#""password": [REDACTED:assignment]"#),
            ("db_password: Zx9!kq2Lmm", "db_password: [REDACTED:assignment]"),
            ("xoxb-1234567890-abcdefghij", "[REDACTED:slack_token]"),
            ("my key mdk_", "my key mdk_"), // not a full key: untouched
        ];
        for (input, expect) in cases {
            let out = r(input);
            assert!(out.contains(expect), "\ninput:  {input}\noutput: {out}\nwanted: {expect}");
        }
        let key = format!("mdk_{}", "a1".repeat(32));
        assert_eq!(r(&format!("login --token {key}")), "login --token [REDACTED:memdusa_key]");
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----";
        assert_eq!(r(&format!("cat id: {pem} done")), "cat id: [REDACTED:private_key] done");
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        assert_eq!(r(&format!("t={jwt}")), "t=[REDACTED:jwt]");
    }

    #[test]
    fn redacts_basic_auth_and_passphrase() {
        let basic = "Authorization: Basic dXNlcjpwYXNz";
        assert_eq!(r(basic), "Authorization: Basic [REDACTED:basic_auth]");
        let curl = "curl -H 'Authorization: Basic dXNlcjpwYXNz' https://example.com";
        assert_eq!(r(curl), "curl -H 'Authorization: Basic [REDACTED:basic_auth]' https://example.com");
        let lower = "authorization: basic dXNlcjpwYXNz";
        assert_eq!(r(lower), "authorization: basic [REDACTED:basic_auth]");
        assert_eq!(r("passphrase=hunter2xyz"), "passphrase=[REDACTED:assignment]");
        assert_eq!(r(r#"{"passphrase": "hunter2xyz"}"#), r#"{"passphrase": [REDACTED:assignment]}"#);
        // ordinary text stays verbatim
        for s in ["Authorization required for this endpoint", "passphrase_count: 12", "Basic auth is deprecated"] {
            assert_eq!(r(s), s, "false positive on {s:?}");
        }
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        for s in [
            "max_tokens=1000",
            "export GITHUB_TOKEN=$GH_TOKEN",
            "TOKEN=${{ secrets.TOKEN }}",
            "author=bob, reviewer=alice",
            "the token is stored hashed; rotate tokens monthly",
            "password: required",
            "git checkout 9b5ddaf1c2e3 && cargo test",
            "id 01a20030-e82b-7690-88e5-7e315bcc86c5",
            "Add Stripe checkout to the cart page",
            "secret_count: 12",
            "--token-file ~/.config/token",
        ] {
            assert_eq!(r(s), s, "false positive on {s:?}");
        }
    }

    #[test]
    fn json_and_custom_patterns() {
        let red = Redactor::new(&[r"ACME-[0-9]{6}".into()]).unwrap();
        let v = red.json(json!({
            "api_key": "abcdef123456",
            "note": "ticket ACME-123456, ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "tokens": 42,
            "nested": [{"secret": "$FROM_ENV"}]
        }));
        assert_eq!(v["api_key"], "[REDACTED:field]");
        assert_eq!(v["note"], "ticket [REDACTED:custom], [REDACTED:github_token]");
        assert_eq!(v["tokens"], 42);
        assert_eq!(v["nested"][0]["secret"], "$FROM_ENV");
        assert!(Redactor::new(&["(unclosed".into()]).is_err());
        assert_eq!(Redactor::disabled().text("pw=hunter2hunter2"), "pw=hunter2hunter2");
    }
}
