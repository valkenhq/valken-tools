//! The tool registry: one list of tools serving the in-app agent, watch mode
//! and MCP clients.
//!
//! Every call goes through [`Registry::call`], which is the single choke point
//! for the rules no caller can bypass:
//!
//! * **who** may call it ([`Access`] vs [`Actor`]): watch mode only reads, and
//!   nothing can approve or apply — that isn't a tool at all;
//! * **tier**: PRO tools refuse unless the caller is entitled;
//! * **input**: typed through serde (unknown fields rejected);
//! * **redaction**: the model-facing body is masked before it leaves;
//! * **audit**: one append-only line per call, success or not.

mod registry;

use std::fmt;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::object::ObjectRef;
use crate::redact::Redactor;

pub use registry::{Call, Registry, ToolDef};

/// Who is asking.  Shown in the audit log's ACTOR column.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "name", rename_all = "snake_case")]
pub enum Actor {
    /// The signed-in person (their email or local user name).
    User(String),
    /// The in-app agent answering a chat.
    Agent,
    /// Watch mode's investigations.
    Watch,
    /// The app itself (digest, startup).
    System,
    /// An approved external MCP client, by its approved name.
    Mcp(String),
}

impl Actor {
    pub fn is_mcp(&self) -> bool {
        matches!(self, Actor::Mcp(_))
    }
}

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Actor::User(who) | Actor::Mcp(who) => f.write_str(who),
            Actor::Agent => f.write_str("agent"),
            Actor::Watch => f.write_str("watch"),
            Actor::System => f.write_str("system"),
        }
    }
}

/// What a tool does to the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Reads cluster state.  Never changes anything.
    Read,
    /// Points at things in Valken's UI (highlight, focus, open a window).
    Ui,
    /// Drafts a proposal into the human review queue.
    Propose,
}

impl Access {
    /// Watch mode gathers evidence only; everyone else may use every access.
    pub fn allows(self, actor: &Actor) -> bool {
        match actor {
            Actor::Watch => self == Access::Read,
            _ => true,
        }
    }
}

/// Licence tier a tool needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    #[default]
    Free,
    Pro,
}

/// Static description of a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpec {
    /// Wire name (`get_events`, `ui_highlight`).  `[a-z_]` only: every
    /// provider and MCP client accepts that.
    pub name: &'static str,
    /// Model-facing description, including *when* to call it.
    pub description: &'static str,
    pub access: Access,
    pub tier: Tier,
}

/// What a tool hands back.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolOutput {
    /// Model-facing text (usually compact JSON).  Redacted by the registry.
    pub body: String,
    /// Objects this result is about: become citations and map highlights.
    pub cites: Vec<ObjectRef>,
    /// Short human note for the tool trail and audit RESULT ("40 lines").
    pub note: Option<String>,
}

impl ToolOutput {
    pub fn text(body: impl Into<String>) -> Self {
        Self { body: body.into(), ..Self::default() }
    }

    /// Serialises `value` as compact JSON.
    pub fn json(value: &impl Serialize) -> Self {
        Self::text(serde_json::to_string(value).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}")))
    }

    pub fn cite(mut self, object: ObjectRef) -> Self {
        self.cites.push(object);
        self
    }

    pub fn cites(mut self, objects: impl IntoIterator<Item = ObjectRef>) -> Self {
        self.cites.extend(objects);
        self
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

/// Why a call failed.  The message goes back to the model as an error result,
/// so it should say what to do differently.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool `{0}`")]
    Unknown(String),
    #[error("{actor} can't use `{tool}`")]
    Denied { actor: String, tool: &'static str },
    #[error("`{0}` needs Valken PRO")]
    NeedsPro(&'static str),
    #[error("invalid input: {0}")]
    Input(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("cluster `{0}` isn't connected in Valken")]
    NoCluster(String),
    #[error("{0}")]
    Failed(String),
}

impl ToolError {
    pub fn failed(e: impl fmt::Display) -> Self {
        Self::Failed(e.to_string())
    }
}

/// Per-call context handed to [`Tool::run`].
#[derive(Debug, Clone, Copy)]
pub struct Cx<'a> {
    pub actor: &'a Actor,
    pub pro: bool,
    /// The rules in force for this call (structure-aware passes, log gate).
    pub redactor: &'a Redactor,
}

/// A typed tool.  Implement this; the registry erases the type.
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    /// Deserialised from the model's JSON; the schema the model sees is
    /// derived from it.  Use `#[serde(deny_unknown_fields)]`.
    type Input: DeserializeOwned + JsonSchema + Send + 'static;

    fn spec(&self) -> ToolSpec;

    /// The audit TARGET column for this input (`storefront/checkout`).
    fn target(&self, input: &Self::Input) -> String;

    async fn run(&self, cx: Cx<'_>, input: Self::Input) -> Result<ToolOutput, ToolError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_mode_only_reads() {
        assert!(Access::Read.allows(&Actor::Watch));
        assert!(!Access::Ui.allows(&Actor::Watch));
        assert!(!Access::Propose.allows(&Actor::Watch));
        assert!(Access::Propose.allows(&Actor::Mcp("cursor".into())));
        assert!(Access::Ui.allows(&Actor::Agent));
    }

    #[test]
    fn actor_display_is_the_audit_column() {
        assert_eq!(Actor::User("jt@example.com".into()).to_string(), "jt@example.com");
        assert_eq!(Actor::Mcp("claude-code".into()).to_string(), "claude-code");
        assert_eq!(Actor::Agent.to_string(), "agent");
    }

    #[test]
    fn actor_round_trips() {
        for a in [Actor::Agent, Actor::Watch, Actor::System, Actor::Mcp("x".into()), Actor::User("u".into())] {
            let s = serde_json::to_string(&a).unwrap();
            assert_eq!(serde_json::from_str::<Actor>(&s).unwrap(), a);
        }
    }

    #[test]
    fn output_builders() {
        let o = ToolOutput::json(&serde_json::json!({"a": 1}))
            .cite(ObjectRef::new("c", "pod", Some("ns"), "p"))
            .note("1 line");
        assert_eq!(o.body, r#"{"a":1}"#);
        assert_eq!(o.cites.len(), 1);
        assert_eq!(o.note.as_deref(), Some("1 line"));
    }
}
