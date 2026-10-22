//! Provider-neutral conversation types.  Each provider module maps these to
//! and from its own wire format.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// One content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
    ToolResult { id: String, content: String, is_error: bool },
    /// A provider block we don't interpret (Anthropic `thinking`,
    /// `redacted_thinking`, `fallback`) — echoed back verbatim to the same
    /// provider, dropped for others.
    Opaque { value: Value },
}

impl Block {
    pub fn text(text: impl Into<String>) -> Self {
        Block::Text { text: text.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, content: vec![Block::text(text)] }
    }

    pub fn assistant(content: Vec<Block>) -> Self {
        Self { role: Role::Assistant, content }
    }

    /// All text blocks joined.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|b| match b {
            Block::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }
}

/// A tool as offered to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// One model call.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub tools: &'a [ToolSchema],
    pub messages: &'a [Message],
    pub max_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    /// The provider's safety system declined.
    Refusal,
    Other(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
}

/// A finished model call.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub content: Vec<Block>,
    pub stop: StopReason,
    pub usage: Usage,
}

/// Incremental output while a call streams.
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Text(String),
    /// The model started writing a tool call.
    ToolStart { id: String, name: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_helpers() {
        let m = Message::assistant(vec![
            Block::text("a"),
            Block::ToolUse { id: "1".into(), name: "get_events".into(), input: Value::Null },
            Block::Opaque { value: serde_json::json!({"type": "thinking"}) },
            Block::text("b"),
        ]);
        assert_eq!(m.text(), "ab");
        assert_eq!(m.tool_uses().map(|(id, ..)| id).collect::<Vec<_>>(), ["1"]);
    }

    #[test]
    fn blocks_round_trip() {
        let m = Message { role: Role::User, content: vec![Block::ToolResult { id: "x".into(), content: "ok".into(), is_error: false }] };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<Message>(&s).unwrap(), m);
    }
}
