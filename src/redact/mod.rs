//! Redaction, enforced before anything leaves the process: every tool body
//! passes through [`Redactor::text`] in the registry, and object JSON goes
//! through [`Redactor::object`] first so structure-aware rules apply.
//!
//! Secrets are not a rule here: no tool can read a Secret at all.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What gets substituted for a masked value.
pub const MASK: &str = "<redacted>";

const LAST_APPLIED: &str = "kubectl.kubernetes.io/last-applied-configuration";

/// The user-visible redaction switches (Settings → Agent → Privacy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    /// Env var values in pod specs: names kept, values masked.
    pub env_values: bool,
    /// JWTs, bearer tokens, AWS keys, PEM blocks, `password=` style pairs.
    pub tokens: bool,
    /// IPv4 addresses.
    pub ips: bool,
    /// Log lines may be sent at all (selected lines or watch-mode evidence).
    pub allow_logs: bool,
}

impl Default for Rules {
    fn default() -> Self {
        Self { env_values: true, tokens: true, ips: false, allow_logs: true }
    }
}

struct Pattern {
    re: Regex,
    /// Replacement; `$k` keeps a captured key.
    with: &'static str,
}

fn pattern(re: &str, with: &'static str) -> Pattern {
    Pattern { re: Regex::new(re).expect("redaction pattern"), with }
}

static TOKENS: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    vec![
        pattern(
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            "<redacted>",
        ),
        pattern(r"\beyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}", "<redacted>"),
        pattern(r"(?i)(?P<k>\b(?:bearer|basic)\s+)[A-Za-z0-9._~+/=-]{8,}", "${k}<redacted>"),
        pattern(r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b", "<redacted>"),
        pattern(r"\b(?:ghp|gho|ghs|github_pat|glpat|xox[abp]|sk-ant|sk)[-_][A-Za-z0-9_-]{16,}", "<redacted>"),
        pattern(
            r#"(?i)(?P<k>\b[a-z0-9_.-]*(?:password|passwd|secret|token|api[_-]?key|access[_-]?key)["']?\s*[:=]\s*["']?)[^\s"',;&]{4,}"#,
            "${k}<redacted>",
        ),
        // Credentials in URLs: scheme://user:pass@host.
        pattern(r"(?P<k>[a-z][a-z0-9+.-]*://[^/\s:@]+:)[^@\s/]+@", "${k}<redacted>@"),
    ]
});

static IPV4: LazyLock<Pattern> = LazyLock::new(|| {
    pattern(r"\b(?:25[0-5]|2[0-4]\d|1?\d?\d)(?:\.(?:25[0-5]|2[0-4]\d|1?\d?\d)){3}\b", "<ip>")
});

/// Applies [`Rules`].  Cheap to clone the rules; the patterns are global.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Redactor {
    rules: Rules,
}

impl Redactor {
    pub fn new(rules: Rules) -> Self {
        Self { rules }
    }

    pub fn rules(&self) -> Rules {
        self.rules
    }

    /// Masks secrets in free text.  Returns the text (borrowed when nothing
    /// matched) and how many values were masked.
    pub fn text<'a>(&self, input: &'a str) -> (Cow<'a, str>, usize) {
        let mut out = Cow::Borrowed(input);
        let mut count = 0;
        let mut apply = |p: &Pattern| {
            let n = p.re.find_iter(&out).count();
            if n > 0 {
                count += n;
                out = Cow::Owned(p.re.replace_all(&out, p.with).into_owned());
            }
        };
        if self.rules.tokens {
            TOKENS.iter().for_each(&mut apply);
        }
        if self.rules.ips {
            apply(&IPV4);
        }
        (out, count)
    }

    /// Structure-aware pass over an object's JSON, in place: env values,
    /// `managedFields`, the last-applied annotation (it repeats the spec,
    /// env values included).  Returns how many values were masked.
    pub fn object(&self, value: &mut Value) -> usize {
        if let Some(meta) = value.get_mut("metadata").and_then(Value::as_object_mut) {
            meta.remove("managedFields");
            if let Some(ann) = meta.get_mut("annotations").and_then(Value::as_object_mut) {
                ann.remove(LAST_APPLIED);
            }
        }
        if self.rules.env_values {
            mask_env(value)
        } else {
            0
        }
    }
}

/// Masks `env[].value` at any depth; `valueFrom` (a reference) is kept.
fn mask_env(value: &mut Value) -> usize {
    match value {
        Value::Object(map) => {
            let mut n = 0;
            for (key, child) in map.iter_mut() {
                if key == "env" {
                    if let Value::Array(vars) = child {
                        for var in vars.iter_mut() {
                            if let Some(v) = var.get_mut("value").filter(|v| !v.is_null()) {
                                *v = Value::String(MASK.to_owned());
                                n += 1;
                            }
                        }
                        continue;
                    }
                }
                n += mask_env(child);
            }
            n
        }
        Value::Array(items) => items.iter_mut().map(mask_env).sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn all() -> Redactor {
        Redactor::new(Rules { env_values: true, tokens: true, ips: true, allow_logs: true })
    }

    #[test]
    fn clean_text_is_borrowed() {
        let (out, n) = all().text("payments call failed: deadline exceeded");
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(n, 0);
    }

    #[test]
    fn masks_tokens() {
        let cases = [
            ("auth header Bearer abc.def-ghijklmno", "auth header Bearer <redacted>"),
            ("jwt eyJhbGciOi.eyJzdWIiOiIx.c2lnbmF0dXJl end", "jwt <redacted> end"),
            ("key AKIAABCDEFGHIJKLMNOP!", "key <redacted>!"),
            ("DB_PASSWORD=hunter22 next", "DB_PASSWORD=<redacted> next"),
            (r#"{"api_key": "abcd1234"}"#, r#"{"api_key": "<redacted>"}"#),
            ("postgres://app:s3cr3t@db:5432/x", "postgres://app:<redacted>@db:5432/x"),
            ("token sk-ant-api03-aaaaaaaaaaaaaaaaaaaa", "token <redacted>"),
        ];
        for (input, want) in cases {
            let (out, n) = all().text(input);
            assert_eq!(out, want, "input: {input}");
            assert!(n >= 1);
        }
    }

    #[test]
    fn masks_pem_blocks() {
        let pem = "x\n-----BEGIN RSA PRIVATE KEY-----\nMIIabc\ndef\n-----END RSA PRIVATE KEY-----\ny";
        assert_eq!(all().text(pem).0, "x\n<redacted>\ny");
    }

    #[test]
    fn ips_only_when_enabled() {
        let line = "dial 10.0.3.17:5432 refused; version 1.2.3";
        assert_eq!(all().text(line).0, "dial <ip>:5432 refused; version 1.2.3");
        assert_eq!(Redactor::default().text(line).0, line);
        assert_eq!(all().text("999.1.1.1").1, 0);
    }

    #[test]
    fn tokens_off_leaves_text() {
        let r = Redactor::new(Rules { tokens: false, ..Rules::default() });
        assert_eq!(r.text("Bearer abcdefghijkl").1, 0);
    }

    #[test]
    fn object_masks_env_and_noise() {
        let mut pod = json!({
            "metadata": {
                "name": "x",
                "managedFields": [{}],
                "annotations": {LAST_APPLIED: "{...}", "keep": "me"}
            },
            "spec": {"containers": [{"name": "app", "env": [
                {"name": "PAYMENTS_URL", "value": "http://payments"},
                {"name": "DB_PASSWORD", "valueFrom": {"secretKeyRef": {"name": "db", "key": "pw"}}},
                {"name": "EMPTY"}
            ]}]}
        });
        assert_eq!(all().object(&mut pod), 1);
        assert!(pod["metadata"].get("managedFields").is_none());
        assert_eq!(pod["metadata"]["annotations"], json!({"keep": "me"}));
        let env = &pod["spec"]["containers"][0]["env"];
        assert_eq!(env[0]["value"], MASK);
        assert_eq!(env[0]["name"], "PAYMENTS_URL");
        assert!(env[1]["valueFrom"].is_object());
    }

    #[test]
    fn object_env_rule_off_keeps_values() {
        let r = Redactor::new(Rules { env_values: false, ..Rules::default() });
        let mut v = json!({"spec": {"template": {"spec": {"containers": [{"env": [{"name": "A", "value": "1"}]}]}}}});
        assert_eq!(r.object(&mut v), 0);
        assert_eq!(v["spec"]["template"]["spec"]["containers"][0]["env"][0]["value"], "1");
    }
}
