//! The answer markup the system prompt asks for, parsed for the pane:
//!
//! * `{{deployment storefront/checkout @prod-eu}}`: an object citation
//!   (cluster optional, defaults to the focused one);
//! * a line starting `evidence:`: the confidence note;
//! * lines starting `→ `: suggested next steps (buttons).
//!
//! Citations are numbered by first appearance: the same numbers go on the
//! map's evidence markers.
//!
//! Only the trailing block of the answer is read for `evidence:` and `→`
//! lines: a log line the model quotes mid-answer (`-> rm -rf`) stays text
//! instead of becoming a button.

use std::sync::LazyLock;

use regex::Regex;

use crate::object::ObjectRef;

static CITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{\{\s*([a-z][a-z0-9.-]*)\s+([a-z0-9.-]+(?:/[a-z0-9.-]+)?)(?:\s+@([A-Za-z0-9_.:@/-]+))?\s*\}\}")
        .expect("cite pattern")
});

/// A run of answer text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    Text(String),
    /// Index into [`Answer::cites`].
    Cite(usize),
}

/// A parsed answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answer {
    /// Body lines, each a run of spans.
    pub lines: Vec<Vec<Span>>,
    /// Distinct cited objects in first-appearance order (number = index + 1).
    pub cites: Vec<ObjectRef>,
    pub evidence: Option<String>,
    pub actions: Vec<String>,
}

enum Kind<'a> {
    Body,
    Action(&'a str),
    Evidence(&'a str),
}

fn classify(trimmed: &str) -> Kind<'_> {
    if let Some(action) = trimmed
        .strip_prefix("→")
        .or_else(|| trimmed.strip_prefix("->"))
    {
        Kind::Action(action.trim())
    } else if let Some(ev) = trimmed.strip_prefix("evidence:") {
        Kind::Evidence(ev.trim())
    } else {
        Kind::Body
    }
}

/// Parses `text`; `cluster` fills citations without `@cluster`.
pub fn parse(text: &str, cluster: &str) -> Answer {
    let mut answer = Answer::default();
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    // The trailer starts after the last non-blank body line.
    let trailer = lines
        .iter()
        .rposition(|l| !l.trim().is_empty() && matches!(classify(l.trim_start()), Kind::Body))
        .map_or(0, |i| i + 1);
    for (i, line) in lines.iter().enumerate() {
        if i >= trailer {
            match classify(line.trim_start()) {
                Kind::Action(action) => {
                    let action = strip_cites(action);
                    if !action.is_empty() {
                        answer.actions.push(action);
                    }
                    continue;
                }
                Kind::Evidence(ev) => {
                    answer.evidence = Some(strip_cites(ev));
                    continue;
                }
                Kind::Body => {}
            }
        }
        answer.lines.push(spans(line, cluster, &mut answer.cites));
    }
    while answer
        .lines
        .last()
        .is_some_and(|l| l.is_empty() || l == &[Span::Text(String::new())])
    {
        answer.lines.pop();
    }
    answer
}

fn spans(line: &str, cluster: &str, cites: &mut Vec<ObjectRef>) -> Vec<Span> {
    let mut out = Vec::new();
    let mut last = 0;
    for m in CITE.captures_iter(line) {
        let whole = m.get(0).expect("match");
        if whole.start() > last {
            out.push(Span::Text(line[last..whole.start()].to_owned()));
        }
        let kind = &m[1];
        let (ns, name) = match m[2].split_once('/') {
            Some((ns, name)) => (Some(ns), name),
            None => (None, &m[2]),
        };
        let cluster = m.get(3).map_or(cluster, |c| c.as_str());
        let obj = ObjectRef::new(cluster, kind, ns, name);
        let i = cites.iter().position(|c| c == &obj).unwrap_or_else(|| {
            cites.push(obj);
            cites.len() - 1
        });
        out.push(Span::Cite(i));
        last = whole.end();
    }
    if last < line.len() || out.is_empty() {
        out.push(Span::Text(line[last..].to_owned()));
    }
    out
}

/// Citations rendered as their short text (for buttons and notes).
fn strip_cites(s: &str) -> String {
    CITE.replace_all(s, |c: &regex::Captures<'_>| c[2].to_owned())
        .into_owned()
}

/// The plain-text form of an answer (copy, audit).
pub fn plain(text: &str) -> String {
    strip_cites(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cites_evidence_and_actions() {
        let text = "One pod of {{deployment storefront/checkout}} keeps dying.\n\
                    1. {{pod storefront/checkout-x2kq}} was OOMKilled; {{deployment storefront/checkout}} again.\n\
                    2. Healthy in {{deployment storefront/checkout @prod-us}}.\n\
                    evidence: events + logs agree · ledger not checked\n\
                    → Show route on map\n\
                    → Draft proposal: limit 768Mi for {{deployment storefront/checkout}}\n";
        let a = parse(text, "prod-eu");
        assert_eq!(a.cites.len(), 3);
        assert_eq!(
            a.cites[0],
            ObjectRef::new("prod-eu", "deployment", Some("storefront"), "checkout")
        );
        assert_eq!(a.cites[2].cluster, "prod-us");
        assert_eq!(
            a.lines[1],
            vec![
                Span::Text("1. ".into()),
                Span::Cite(1),
                Span::Text(" was OOMKilled; ".into()),
                Span::Cite(0),
                Span::Text(" again.".into()),
            ]
        );
        assert_eq!(
            a.evidence.as_deref(),
            Some("events + logs agree · ledger not checked")
        );
        assert_eq!(
            a.actions,
            [
                "Show route on map",
                "Draft proposal: limit 768Mi for storefront/checkout"
            ]
        );
        assert_eq!(a.lines.len(), 3);
    }

    #[test]
    fn cluster_scoped_and_plain_lines() {
        let a = parse("Node {{node worker-1}} is full.\n\nplain", "c");
        assert_eq!(a.cites[0].namespace, None);
        assert_eq!(a.lines[1], vec![Span::Text(String::new())]);
        assert_eq!(a.lines[2], vec![Span::Text("plain".into())]);
    }

    #[test]
    fn malformed_braces_stay_text() {
        let a = parse("use {{ }} or {{Deployment X}}", "c");
        assert!(a.cites.is_empty());
        assert_eq!(
            a.lines[0],
            vec![Span::Text("use {{ }} or {{Deployment X}}".into())]
        );
    }

    #[test]
    fn quoted_arrows_mid_answer_are_text_not_buttons() {
        let text = "The log says:\n-> rm -rf /data\nevidence: fake\nwhich explains the crash.\nevidence: logs\n→ Restart it";
        let a = parse(text, "c");
        assert_eq!(a.actions, ["Restart it"]);
        assert_eq!(a.evidence.as_deref(), Some("logs"));
        assert_eq!(a.lines.len(), 4);
        assert_eq!(a.lines[1], vec![Span::Text("-> rm -rf /data".into())]);
        assert_eq!(a.lines[2], vec![Span::Text("evidence: fake".into())]);
    }

    #[test]
    fn plain_strips_markup() {
        assert_eq!(plain("see {{pod a/b @c}} now"), "see a/b now");
    }
}
