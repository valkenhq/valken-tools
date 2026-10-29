//! Redaction, enforced before anything leaves the process: every tool body
//! passes through [`Redactor::text`] in the registry, and object JSON goes
//! through [`Redactor::object`] first so structure-aware rules apply.
//!
//! Secrets are not a rule here: no tool can read a Secret at all.  ConfigMap
//! values are (people put credentials in them), and so are the annotations
//! that carry a copy of a spec or a command line.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What gets substituted for a masked value.
pub const MASK: &str = "<redacted>";

const LAST_APPLIED: &str = "kubectl.kubernetes.io/last-applied-configuration";

/// Annotations that hold a copy of the spec (env values included) or the
/// command that produced it (`kubectl set env … DB_PASS=…`).
const SPEC_COPY_ANNOTATIONS: [&str; 4] = [
    LAST_APPLIED,
    "kubernetes.io/change-cause",
    "kapp.k14s.io/original",
    "kapp.k14s.io/original-diff",
];

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
    /// ConfigMap `data` / `binaryData` values in `get_object`: keys kept,
    /// values masked.
    pub configmap_values: bool,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            env_values: true,
            tokens: true,
            ips: false,
            allow_logs: true,
            configmap_values: true,
        }
    }
}

struct Pattern {
    re: Regex,
    /// Replacement; `$k` keeps a captured key.
    with: &'static str,
}

fn pattern(re: &str, with: &'static str) -> Pattern {
    Pattern {
        re: Regex::new(re).expect("redaction pattern"),
        with,
    }
}

static TOKENS: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    vec![
        pattern(
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            "<redacted>",
        ),
        pattern(
            r"\beyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}",
            "<redacted>",
        ),
        pattern(
            r"(?i)(?P<k>\b(?:bearer|basic)\s+)[A-Za-z0-9._~+/=-]{8,}",
            "${k}<redacted>",
        ),
        pattern(r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b", "<redacted>"),
        pattern(
            r"\b(?:ghp|gho|ghs|github_pat|glpat|xox[abp]|sk-ant|sk)[-_][A-Za-z0-9_-]{16,}",
            "<redacted>",
        ),
        pattern(
            r#"(?i)(?P<k>\b[a-z0-9_.-]*(?:password|passwd|secret|token|api[_-]?key|access[_-]?key)["']?\s*[:=]\s*["']?)[^\s"',;&]{4,}"#,
            "${k}<redacted>",
        ),
        // Credentials in URLs: scheme://user:pass@host.
        pattern(
            r"(?P<k>[a-z][a-z0-9+.-]*://[^/\s:@]+:)[^@\s/]+@",
            "${k}<redacted>@",
        ),
    ]
});

/// A command-line flag that names a credential: `--password`, `-p`
/// doesn't count, `--db-secret-key` does.
static CRED_FLAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^--?[a-z0-9_.-]*(?:password|passwd|secret|token|api[-_]?key|access[-_]?key)[a-z0-9_.-]*$")
        .expect("flag pattern")
});

static IPV4: LazyLock<Pattern> = LazyLock::new(|| {
    pattern(
        r"\b(?:25[0-5]|2[0-4]\d|1?\d?\d)(?:\.(?:25[0-5]|2[0-4]\d|1?\d?\d)){3}\b",
        "<ip>",
    )
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

    /// Structure-aware pass over an object's JSON, in place: `managedFields`
    /// and the spec-copy annotations go (from every `metadata`, pod and job
    /// templates included); env values, credential flags in `command` /
    /// `args` and ConfigMap values are masked by rule.  Returns how many
    /// values were masked.
    pub fn object(&self, value: &mut Value) -> usize {
        if let Some(meta) = value.get_mut("metadata").and_then(Value::as_object_mut) {
            meta.remove("managedFields");
        }
        let mut n = 0;
        if self.rules.configmap_values && value["kind"] == "ConfigMap" {
            for field in ["data", "binaryData"] {
                if let Some(map) = value.get_mut(field).and_then(Value::as_object_mut) {
                    for v in map.values_mut() {
                        *v = Value::String(MASK.to_owned());
                        n += 1;
                    }
                }
            }
        }
        n + walk(value, self.rules.env_values)
    }
}

/// Every `metadata.annotations` loses the spec-copy keys; `env[].value`
/// (`valueFrom`, a reference, is kept) and credential flags in `command` /
/// `args` are masked at any depth.
fn walk(value: &mut Value, env_values: bool) -> usize {
    match value {
        Value::Object(map) => {
            let mut n = 0;
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    "env" if env_values => {
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
                    "metadata" => {
                        if let Some(ann) =
                            child.get_mut("annotations").and_then(Value::as_object_mut)
                        {
                            for key in SPEC_COPY_ANNOTATIONS {
                                ann.remove(key);
                            }
                        }
                    }
                    "command" | "args" => {
                        if let Value::Array(argv) = child {
                            n += mask_argv(argv);
                            continue;
                        }
                    }
                    _ => {}
                }
                n += walk(child, env_values);
            }
            n
        }
        Value::Array(items) => items.iter_mut().map(|v| walk(v, env_values)).sum(),
        _ => 0,
    }
}

/// `["--password", "x"]` and `["--password=x"]` both lose `x`.
fn mask_argv(argv: &mut [Value]) -> usize {
    let mut n = 0;
    let mut mask_next = false;
    for item in argv.iter_mut() {
        let Some(s) = item.as_str() else {
            mask_next = false;
            continue;
        };
        if mask_next {
            *item = Value::String(MASK.to_owned());
            n += 1;
            mask_next = false;
        } else if let Some((flag, _)) = s
            .split_once('=')
            .filter(|(flag, _)| CRED_FLAG.is_match(flag))
        {
            *item = Value::String(format!("{flag}={MASK}"));
            n += 1;
        } else {
            mask_next = CRED_FLAG.is_match(s);
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn all() -> Redactor {
        Redactor::new(Rules {
            ips: true,
            ..Rules::default()
        })
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
            (
                "auth header Bearer abc.def-ghijklmno",
                "auth header Bearer <redacted>",
            ),
            (
                "jwt eyJhbGciOi.eyJzdWIiOiIx.c2lnbmF0dXJl end",
                "jwt <redacted> end",
            ),
            ("key AKIAABCDEFGHIJKLMNOP!", "key <redacted>!"),
            ("DB_PASSWORD=hunter22 next", "DB_PASSWORD=<redacted> next"),
            (r#"{"api_key": "abcd1234"}"#, r#"{"api_key": "<redacted>"}"#),
            (
                "postgres://app:s3cr3t@db:5432/x",
                "postgres://app:<redacted>@db:5432/x",
            ),
            (
                "token sk-ant-api03-aaaaaaaaaaaaaaaaaaaa",
                "token <redacted>",
            ),
        ];
        for (input, want) in cases {
            let (out, n) = all().text(input);
            assert_eq!(out, want, "input: {input}");
            assert!(n >= 1);
        }
    }

    #[test]
    fn masks_pem_blocks() {
        let pem =
            "x\n-----BEGIN RSA PRIVATE KEY-----\nMIIabc\ndef\n-----END RSA PRIVATE KEY-----\ny";
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
        let r = Redactor::new(Rules {
            tokens: false,
            ..Rules::default()
        });
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
    fn object_masks_argv_flags_configmaps_and_nested_annotations() {
        let mut dep = json!({
            "kind": "Deployment",
            "metadata": {"annotations": {"kubernetes.io/change-cause": "kubectl set env DB_PASS=hunter2", "keep": "me"}},
            "spec": {"template": {
                "metadata": {"annotations": {LAST_APPLIED: "{\"value\":\"hunter2\"}", "prometheus.io/scrape": "true"}},
                "spec": {"containers": [{
                    "name": "app",
                    "command": ["/app", "--db-password", "hunter2", "--verbose"],
                    "args": ["--api-key=hunter2", "--port=8080", "-p", "not-a-flag"],
                    "env": [{"name": "A", "value": "hunter2"}]
                }]}
            }}
        });
        let n = all().object(&mut dep);
        assert_eq!(n, 3, "one bare flag, one =flag, one env");
        assert!(!dep.to_string().contains("hunter2"), "{dep}");
        let c = &dep["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(
            c["command"],
            json!(["/app", "--db-password", MASK, "--verbose"])
        );
        assert_eq!(
            c["args"],
            json!([
                format!("--api-key={MASK}"),
                "--port=8080",
                "-p",
                "not-a-flag"
            ])
        );
        assert_eq!(dep["metadata"]["annotations"], json!({"keep": "me"}));
        assert_eq!(
            dep["spec"]["template"]["metadata"]["annotations"],
            json!({"prometheus.io/scrape": "true"})
        );

        let mut cm = json!({"kind": "ConfigMap", "metadata": {"name": "c"}, "data": {"url": "postgres://x", "A": "1"}, "binaryData": {"b": "AAA="}});
        assert_eq!(all().object(&mut cm), 3);
        assert_eq!(cm["data"], json!({"url": MASK, "A": MASK}));
        let off = Redactor::new(Rules {
            configmap_values: false,
            ..Rules::default()
        });
        let mut cm = json!({"kind": "ConfigMap", "data": {"A": "1"}});
        assert_eq!(off.object(&mut cm), 0);
        assert_eq!(cm["data"]["A"], "1");
        // Only ConfigMaps: a Pod's `data`-named field is left alone.
        let mut pod = json!({"kind": "Pod", "data": {"A": "1"}});
        assert_eq!(all().object(&mut pod), 0);
    }

    #[test]
    fn object_env_rule_off_keeps_values() {
        let r = Redactor::new(Rules {
            env_values: false,
            ..Rules::default()
        });
        let mut v = json!({"spec": {"template": {"spec": {"containers": [{"env": [{"name": "A", "value": "1"}]}]}}}});
        assert_eq!(r.object(&mut v), 0);
        assert_eq!(
            v["spec"]["template"]["spec"]["containers"][0]["env"][0]["value"],
            "1"
        );
    }
}
