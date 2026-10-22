//! The agent loop: model call → tool calls (in parallel, through the
//! registry) → model call, until the model stops asking for tools.

mod budget;
pub mod markup;
pub mod prompt;

use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;

pub use budget::{Budget, Exhausted, Spend};

use crate::model::{Block, Delta, Message, ModelClient, ModelError, Request, Role, StopReason, ToolSchema, Usage};
use crate::object::ObjectRef;
use crate::redact::Redactor;
use crate::tool::{Actor, Registry};

/// Tool-call rounds per user turn before the loop stops itself.
pub const MAX_STEPS: usize = 12;
const MAX_TOKENS: u32 = 32_000;

/// What the pane sees while a turn runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Text(String),
    /// The model started a tool call (trail row appears, pending).
    ToolStart { id: String, name: String },
    /// A tool call finished (trail row completes; cites light up).
    ToolDone {
        id: String,
        name: String,
        target: String,
        summary: String,
        is_error: bool,
        cites: Vec<ObjectRef>,
        elapsed: Duration,
    },
    /// One model call finished.
    ModelCall { usage: Usage },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Budget(#[from] Exhausted),
    #[error("the model declined to answer this")]
    Refused,
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub stop: StopReason,
    /// Model calls made.
    pub calls: usize,
    pub usage: Usage,
    /// The step limit cut the turn short.
    pub truncated: bool,
}

/// One turn's settings.
#[derive(Debug, Clone, Copy)]
pub struct Turn<'a> {
    pub actor: &'a Actor,
    pub pro: bool,
    pub model: &'a str,
    pub system: &'a str,
    pub spend: Spend,
    /// Offer tools at all (false: chat-only mode, or a summary).
    pub tools: bool,
}

/// The loop's collaborators.  Cheap to clone.
#[derive(Clone)]
pub struct Agent {
    pub registry: Arc<Registry>,
    pub client: ModelClient,
    pub budget: Arc<Budget>,
}

impl Agent {
    /// Runs the model until it stops calling tools, appending every message
    /// to `history`.  `history` always ends valid for the next turn: a
    /// dangling tool call gets an error result.
    #[tracing::instrument(name = "agent.turn", skip_all, fields(actor = %turn.actor, calls = tracing::field::Empty))]
    pub async fn run(
        &self,
        turn: Turn<'_>,
        history: &mut Vec<Message>,
        emit: &mut (dyn FnMut(Event) + Send),
    ) -> Result<Outcome, AgentError> {
        let tools: Vec<ToolSchema> = if turn.tools {
            self.registry
                .defs_for(turn.actor, turn.pro)
                .map(|d| ToolSchema { name: d.name.into(), description: d.description.into(), schema: (*d.schema).clone() })
                .collect()
        } else {
            Vec::new()
        };
        let mut usage = Usage::default();
        for calls in 1..=MAX_STEPS {
            if let Err(e) = self.budget.spend(turn.spend) {
                close_dangling(history, "not run: the hourly model budget is used up");
                return Err(e.into());
            }
            let req = Request { model: turn.model, system: turn.system, tools: &tools, messages: history, max_tokens: MAX_TOKENS };
            let resp = self
                .client
                .send(&req, &mut |d| match d {
                    Delta::Text(t) => emit(Event::Text(t)),
                    Delta::ToolStart { id, name } => emit(Event::ToolStart { id, name }),
                })
                .await?;
            usage.input_tokens += resp.usage.input_tokens;
            usage.output_tokens += resp.usage.output_tokens;
            usage.cache_read_tokens += resp.usage.cache_read_tokens;
            emit(Event::ModelCall { usage: resp.usage });
            if !resp.content.is_empty() {
                history.push(Message::assistant(resp.content));
            }
            tracing::Span::current().record("calls", calls);
            match resp.stop {
                StopReason::ToolUse => {
                    let results = self.call_tools(turn, history.last().expect("assistant message"), emit).await;
                    history.push(Message { role: Role::User, content: results });
                }
                StopReason::Refusal => {
                    close_dangling(history, "not run: the reply was declined");
                    return Err(AgentError::Refused);
                }
                stop => {
                    close_dangling(history, "not run: the reply was cut off");
                    return Ok(Outcome { stop, calls, usage, truncated: false });
                }
            }
        }
        Ok(Outcome { stop: StopReason::ToolUse, calls: MAX_STEPS, usage, truncated: true })
    }

    /// Runs every tool call in `msg` concurrently; results in call order.
    async fn call_tools(&self, turn: Turn<'_>, msg: &Message, emit: &mut (dyn FnMut(Event) + Send)) -> Vec<Block> {
        let calls = msg.tool_uses().map(|(id, name, input)| {
            let registry = &self.registry;
            async move { (id.to_owned(), registry.call(turn.actor, turn.pro, name, input.clone()).await) }
        });
        join_all(calls)
            .await
            .into_iter()
            .map(|(id, call)| {
                let cites = call.output.as_ref().map(|o| o.cites.clone()).unwrap_or_default();
                emit(Event::ToolDone {
                    id: id.clone(),
                    name: call.tool.clone(),
                    target: call.target.clone(),
                    summary: call.summary(),
                    is_error: call.is_error(),
                    cites,
                    elapsed: call.elapsed,
                });
                Block::ToolResult { id, content: call.model_text(), is_error: call.is_error() }
            })
            .collect()
    }
}

/// Answers any unanswered tool calls in the last assistant message so the
/// next request is valid.
fn close_dangling(history: &mut Vec<Message>, why: &str) {
    let Some(last) = history.last().filter(|m| m.role == Role::Assistant) else { return };
    let results: Vec<Block> = last
        .tool_uses()
        .map(|(id, ..)| Block::ToolResult { id: id.to_owned(), content: why.to_owned(), is_error: true })
        .collect();
    if !results.is_empty() {
        history.push(Message { role: Role::User, content: results });
    }
}

/// Appends user content, merging into a trailing user message (tool results
/// from a step-limited turn) so roles keep alternating.
pub fn push_user(history: &mut Vec<Message>, content: Vec<Block>) {
    match history.last_mut() {
        Some(last) if last.role == Role::User => last.content.extend(content),
        _ => history.push(Message { role: Role::User, content }),
    }
}

/// Something attached to a question (a context chip).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attachment {
    /// An object, with a compact summary (JSON or text).
    Object { object: ObjectRef, summary: String },
    /// Log lines the user selected.
    Logs { object: ObjectRef, lines: Vec<String> },
    /// A whole cluster in scope.
    Cluster { name: String },
}

impl Attachment {
    /// The chip text.
    pub fn label(&self) -> String {
        match self {
            Attachment::Object { object, .. } => object.short(),
            Attachment::Logs { lines, .. } => format!("{} log line{}", lines.len(), if lines.len() == 1 { "" } else { "s" }),
            Attachment::Cluster { name } => name.clone(),
        }
    }
}

/// Builds the user message: attachments (redacted) in a context block, then
/// the question.  Log lines are dropped when the rules forbid them.
pub fn compose(question: &str, attachments: &[Attachment], redactor: &Redactor) -> Vec<Block> {
    let mut ctx = String::new();
    for a in attachments {
        match a {
            Attachment::Object { object, summary } => {
                ctx.push_str(&format!("<object ref=\"{}\">\n{summary}\n</object>\n", cite(object)));
            }
            Attachment::Logs { object, lines } if redactor.rules().allow_logs => {
                ctx.push_str(&format!("<logs of=\"{}\">\n{}\n</logs>\n", cite(object), lines.join("\n")));
            }
            Attachment::Logs { .. } => {}
            Attachment::Cluster { name } => ctx.push_str(&format!("<cluster name=\"{name}\"/>\n")),
        }
    }
    let text = if ctx.is_empty() {
        question.to_owned()
    } else {
        format!("<context>\n{}</context>\n\n{question}", redactor.text(&ctx).0)
    };
    vec![Block::text(text)]
}

fn cite(o: &ObjectRef) -> String {
    format!("{{{{{} {} @{}}}}}", o.kind, o.short(), o.cluster)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::Rules;

    #[test]
    fn close_dangling_answers_every_call() {
        let mut h = vec![
            Message::user("q"),
            Message::assistant(vec![
                Block::ToolUse { id: "a".into(), name: "x".into(), input: serde_json::Value::Null },
                Block::ToolUse { id: "b".into(), name: "y".into(), input: serde_json::Value::Null },
            ]),
        ];
        close_dangling(&mut h, "cut");
        assert_eq!(h.len(), 3);
        assert_eq!(h[2].content.len(), 2);
        // Nothing to close after a plain answer.
        let mut h = vec![Message::user("q"), Message::assistant(vec![Block::text("a")])];
        close_dangling(&mut h, "cut");
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn push_user_merges() {
        let mut h = vec![Message { role: Role::User, content: vec![Block::ToolResult { id: "a".into(), content: "x".into(), is_error: false }] }];
        push_user(&mut h, vec![Block::text("next")]);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].content.len(), 2);
        h.push(Message::assistant(vec![Block::text("ok")]));
        push_user(&mut h, vec![Block::text("more")]);
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn compose_redacts_and_respects_log_rule() {
        let obj = ObjectRef::new("prod-eu", "deployment", Some("storefront"), "checkout");
        let atts = [
            Attachment::Object { object: obj.clone(), summary: "{\"env\":\"DB_PASSWORD=hunter22\"}".into() },
            Attachment::Logs { object: obj.clone(), lines: vec!["auth Bearer abcdefghijkl".into()] },
        ];
        let on = compose("why?", &atts, &Redactor::default());
        let Block::Text { text } = &on[0] else { panic!() };
        assert!(text.contains("{{deployment storefront/checkout @prod-eu}}"));
        assert!(!text.contains("hunter22") && !text.contains("abcdefghijkl"));
        assert!(text.ends_with("why?"));
        let off = compose("why?", &atts, &Redactor::new(Rules { allow_logs: false, ..Rules::default() }));
        let Block::Text { text } = &off[0] else { panic!() };
        assert!(!text.contains("<logs"));
        assert_eq!(compose("plain", &[], &Redactor::default()), vec![Block::text("plain")]);
    }

    #[test]
    fn attachment_labels() {
        let obj = ObjectRef::new("c", "pod", Some("a"), "b");
        assert_eq!(Attachment::Logs { object: obj.clone(), lines: vec!["x".into(); 3] }.label(), "3 log lines");
        assert_eq!(Attachment::Object { object: obj, summary: String::new() }.label(), "a/b");
    }
}
