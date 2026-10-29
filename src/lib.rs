//! Valken's agent tool layer.
//!
//! One tool registry serves two consumers: the in-app agent pane (bring
//! your own model) and the MCP server for external agents.  Redaction,
//! tier gating and the audit log live here, in front of every tool, so no
//! client — in-app or external — can bypass them.
//!
//! | Module | What it does |
//! |---|---|
//! | [`tool`] | The [`tool::Tool`] trait and the [`tool::Registry`] choke point |
//! | [`redact`] | Masking rules applied before anything leaves the process |
//! | [`audit`] | Append-only JSONL audit log |
//! | [`model`] | Anthropic and OpenAI-compatible streaming clients |
//! | [`agent`] | The tool-calling loop, budget, prompts, answer markup |
//! | [`mcp`] | MCP over a local socket (relayed from stdio) and loopback HTTP |
//! | [`k8s`] | Read tools over valken-core |
//! | [`object`] | Object references (citations) |

pub mod agent;
pub mod audit;
pub mod k8s;
pub mod mcp;
pub mod model;
pub mod object;
pub mod redact;
pub mod tool;
