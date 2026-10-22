//! The MCP server: the same registry the in-app agent uses, offered to
//! external agents (Claude Code, Cursor, …).
//!
//! * [`Server::handle`] is transport-free JSON-RPC: `initialize`,
//!   `tools/list`, `tools/call`, `ping`.
//! * [`serve_lines`] speaks newline-delimited JSON over any byte stream: the
//!   app's local socket, which `valken-mcp --stdio` relays to.
//! * [`http`] is Streamable HTTP on loopback, bearer-token protected.
//!
//! Clients are admitted by the application's [`Gate`] (approved once, by
//! name, revocable).  Approving or applying a proposal is not a tool, so it
//! can't be reached from here at all.

pub mod http;
pub mod local;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::tool::{Access, Actor, Registry};

/// Newest first; we answer with the client's version when we support it.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
/// Server-defined: the client hasn't been approved in Valken.
pub const NOT_APPROVED: i64 = -32001;

const INSTRUCTIONS: &str = "Valken's live view of the user's Kubernetes clusters. Read tools return redacted state \
(Secrets are never readable). ui_ tools point at things on Valken's map while you explain. propose_change drafts a \
proposal into the user's review queue; you cannot approve or apply anything.";

/// How a client reached us.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Stdio,
    Http,
}

impl Transport {
    pub fn label(self) -> &'static str {
        match self {
            Transport::Stdio => "stdio",
            Transport::Http => "port",
        }
    }
}

/// What the client said about itself in `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default)]
    pub version: String,
}

impl ClientInfo {
    /// `Claude Code` → `claude-code`: the audit actor.
    pub fn slug(&self) -> String {
        let mut out = String::new();
        for c in self.name.trim().chars() {
            match c {
                c if c.is_ascii_alphanumeric() => out.push(c.to_ascii_lowercase()),
                _ if !out.ends_with('-') && !out.is_empty() => out.push('-'),
                _ => {}
            }
        }
        while out.ends_with('-') {
            out.pop();
        }
        if out.is_empty() {
            "mcp-client".into()
        } else {
            out
        }
    }
}

/// Whether a client may call tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Approved,
    /// Shown in Valken for approval; calls fail until then.
    Pending,
    Revoked,
}

/// The application's policy hook.
pub trait Gate: Send + Sync + 'static {
    /// Called on every tool call (cheap: an in-memory lookup).  Unknown
    /// clients should be recorded as pending.
    fn admit(&self, client: &ClientInfo, transport: Transport) -> Admission;
    /// PRO entitlement right now.
    fn pro(&self) -> bool;
    /// The MCP server is switched on.
    fn enabled(&self) -> bool {
        true
    }
}

/// Per-connection state.
#[derive(Debug, Clone)]
pub struct Session {
    pub transport: Transport,
    pub client: Option<ClientInfo>,
}

impl Session {
    pub fn new(transport: Transport) -> Self {
        Self { transport, client: None }
    }
}

pub struct Server {
    registry: Arc<Registry>,
    gate: Arc<dyn Gate>,
    version: String,
}

impl Server {
    pub fn new(registry: Arc<Registry>, gate: Arc<dyn Gate>, version: impl Into<String>) -> Arc<Self> {
        Arc::new(Self { registry, gate, version: version.into() })
    }

    /// Handles one JSON-RPC message (or batch).  `None` for notifications.
    pub async fn handle(&self, session: &mut Session, msg: Value) -> Option<Value> {
        match msg {
            Value::Array(batch) => {
                let mut out = Vec::new();
                for m in batch {
                    if let Some(r) = Box::pin(self.handle(session, m)).await {
                        out.push(r);
                    }
                }
                (!out.is_empty()).then_some(Value::Array(out))
            }
            msg => self.handle_one(session, msg).await,
        }
    }

    async fn handle_one(&self, session: &mut Session, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let Some(method) = msg["method"].as_str() else {
            // A response from the client (we never send requests): ignore.
            return id.filter(|_| msg.get("result").is_none() && msg.get("error").is_none()).map(|id| error(id, INVALID_REQUEST, "missing method"));
        };
        let id = id?; // Notifications get no reply.
        let params = &msg["params"];
        Some(match method {
            "initialize" => self.initialize(session, id, params),
            "ping" => ok(id, json!({})),
            "tools/list" => ok(id, self.tools_list()),
            "tools/call" => self.tools_call(session, id, params).await,
            "resources/list" => ok(id, json!({"resources": []})),
            "prompts/list" => ok(id, json!({"prompts": []})),
            _ => error(id, METHOD_NOT_FOUND, &format!("unknown method {method}")),
        })
    }

    fn initialize(&self, session: &mut Session, id: Value, params: &Value) -> Value {
        let client: ClientInfo = serde_json::from_value(params["clientInfo"].clone())
            .unwrap_or(ClientInfo { name: "unknown client".into(), version: String::new() });
        let asked = params["protocolVersion"].as_str().unwrap_or_default();
        let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL_VERSIONS[0]);
        // Record the client now so it shows up for approval before its first call.
        self.gate.admit(&client, session.transport);
        session.client = Some(client);
        ok(
            id,
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "valken", "title": "Valken", "version": self.version},
                "instructions": INSTRUCTIONS,
            }),
        )
    }

    fn tools_list(&self) -> Value {
        let probe = Actor::Mcp(String::new());
        let tools: Vec<Value> = self
            .registry
            .defs_for(&probe, self.gate.pro())
            .map(|d| {
                json!({
                    "name": d.name,
                    "description": d.description,
                    "inputSchema": *d.schema,
                    "annotations": {
                        "readOnlyHint": d.access == Access::Read,
                        "destructiveHint": false,
                        "openWorldHint": false,
                    },
                })
            })
            .collect();
        json!({"tools": tools})
    }

    async fn tools_call(&self, session: &Session, id: Value, params: &Value) -> Value {
        if !self.gate.enabled() {
            return error(id, NOT_APPROVED, "the MCP server is turned off in Valken → Settings → MCP server");
        }
        let Some(client) = &session.client else {
            return error(id, INVALID_REQUEST, "call initialize first");
        };
        match self.gate.admit(client, session.transport) {
            Admission::Approved => {}
            Admission::Pending => {
                return error(id, NOT_APPROVED, &format!("\"{}\" is waiting for approval in Valken → Settings → MCP server", client.name));
            }
            Admission::Revoked => return error(id, NOT_APPROVED, &format!("\"{}\" was revoked in Valken", client.name)),
        }
        let Some(name) = params["name"].as_str() else {
            return error(id, INVALID_PARAMS, "missing tool name");
        };
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let call = self.registry.call(&Actor::Mcp(client.slug()), self.gate.pro(), name, args).await;
        let mut content = vec![json!({"type": "text", "text": call.model_text()})];
        if let Ok(out) = &call.output {
            if !out.cites.is_empty() {
                let cites: Vec<String> = out.cites.iter().map(ToString::to_string).collect();
                content.push(json!({"type": "text", "text": format!("objects: {}", cites.join("; "))}));
            }
        }
        ok(id, json!({"content": content, "isError": call.is_error()}))
    }
}

fn ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// A JSON-RPC parse error reply (no id is known).
pub fn parse_error(detail: &str) -> Value {
    error(Value::Null, PARSE_ERROR, detail)
}

/// Serves newline-delimited JSON-RPC until the reader closes.
pub async fn serve_lines<R, W>(server: Arc<Server>, transport: Transport, reader: R, mut writer: W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut session = Session::new(transport);
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => server.handle(&mut session, msg).await,
            Err(e) => Some(parse_error(&e.to_string())),
        };
        if let Some(reply) = reply {
            let mut bytes = serde_json::to_vec(&reply).map_err(std::io::Error::other)?;
            bytes.push(b'\n');
            writer.write_all(&bytes).await?;
            writer.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::redact::Redactor;
    use crate::tool::{Cx, Tier, Tool, ToolError, ToolOutput, ToolSpec};
    use async_trait::async_trait;
    use schemars::JsonSchema;

    #[derive(Default)]
    pub struct TestGate {
        pub approved: Mutex<Vec<String>>,
        pub seen: Mutex<Vec<(String, Transport)>>,
        pub off: bool,
    }

    impl Gate for TestGate {
        fn admit(&self, client: &ClientInfo, transport: Transport) -> Admission {
            self.seen.lock().unwrap().push((client.name.clone(), transport));
            if self.approved.lock().unwrap().contains(&client.name) {
                Admission::Approved
            } else {
                Admission::Pending
            }
        }
        fn pro(&self) -> bool {
            false
        }
        fn enabled(&self) -> bool {
            !self.off
        }
    }

    #[derive(Deserialize, JsonSchema)]
    pub struct Empty {}

    pub struct Hello;

    #[async_trait]
    impl Tool for Hello {
        type Input = Empty;
        fn spec(&self) -> ToolSpec {
            ToolSpec { name: "hello", description: "Says hello.", access: Access::Read, tier: Tier::Free }
        }
        fn target(&self, _: &Empty) -> String {
            "world".into()
        }
        async fn run(&self, cx: Cx<'_>, _: Empty) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text(format!("hello {}", cx.actor))
                .cite(crate::object::ObjectRef::new("kind-a", "pod", Some("ns"), "p")))
        }
    }

    struct ProOnly;

    #[async_trait]
    impl Tool for ProOnly {
        type Input = Empty;
        fn spec(&self) -> ToolSpec {
            ToolSpec { name: "compare", description: "PRO.", access: Access::Read, tier: Tier::Pro }
        }
        fn target(&self, _: &Empty) -> String {
            String::new()
        }
        async fn run(&self, _: Cx<'_>, _: Empty) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("x"))
        }
    }

    pub fn server(gate: Arc<TestGate>) -> Arc<Server> {
        let mut r = Registry::new(Redactor::default(), None);
        r.register(Hello).register(ProOnly);
        Server::new(Arc::new(r), gate, "0.1.0")
    }

    fn init(name: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": name, "version": "1"}}})
    }

    fn call(id: i64) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "hello", "arguments": {}}})
    }

    #[tokio::test]
    async fn handshake_lists_and_negotiates() {
        let gate = Arc::new(TestGate::default());
        let s = server(gate.clone());
        let mut session = Session::new(Transport::Stdio);
        let r = s.handle(&mut session, init("Claude Code")).await.unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(r["result"]["serverInfo"]["name"], "valken");
        assert_eq!(gate.seen.lock().unwrap()[0], ("Claude Code".into(), Transport::Stdio));
        assert!(s.handle(&mut session, json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await.is_none());
        let list = s.handle(&mut session, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).await.unwrap();
        let names: Vec<_> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].clone()).collect();
        assert_eq!(names, [json!("hello")], "PRO tools hidden without entitlement");
        assert_eq!(list["result"]["tools"][0]["annotations"]["readOnlyHint"], true);
        // Unknown protocol versions get our newest.
        let mut other = Session::new(Transport::Http);
        let r = s
            .handle(&mut other, json!({"jsonrpc": "2.0", "id": 9, "method": "initialize", "params": {"protocolVersion": "1999-01-01"}}))
            .await
            .unwrap();
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[tokio::test]
    async fn calls_need_approval() {
        let gate = Arc::new(TestGate::default());
        let s = server(gate.clone());
        let mut session = Session::new(Transport::Stdio);
        let r = s.handle(&mut session, call(1)).await.unwrap();
        assert_eq!(r["error"]["code"], INVALID_REQUEST, "initialize first");
        s.handle(&mut session, init("Claude Code")).await;
        let r = s.handle(&mut session, call(2)).await.unwrap();
        assert_eq!(r["error"]["code"], NOT_APPROVED);
        gate.approved.lock().unwrap().push("Claude Code".into());
        let r = s.handle(&mut session, call(3)).await.unwrap();
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(r["result"]["content"][0]["text"], "hello claude-code");
        assert!(r["result"]["content"][1]["text"].as_str().unwrap().contains("pod ns/p @kind-a"));
    }

    #[tokio::test]
    async fn disabled_server_refuses_calls() {
        let gate = Arc::new(TestGate { off: true, ..Default::default() });
        gate.approved.lock().unwrap().push("x".into());
        let s = server(gate);
        let mut session = Session::new(Transport::Stdio);
        s.handle(&mut session, init("x")).await;
        assert_eq!(s.handle(&mut session, call(2)).await.unwrap()["error"]["code"], NOT_APPROVED);
    }

    #[tokio::test]
    async fn errors_batches_and_unknown_methods() {
        let gate = Arc::new(TestGate::default());
        gate.approved.lock().unwrap().push("x".into());
        let s = server(gate);
        let mut session = Session::new(Transport::Stdio);
        let r = s.handle(&mut session, json!([init("x"), call(2), {"jsonrpc": "2.0", "method": "notifications/x"}])).await.unwrap();
        assert_eq!(r.as_array().unwrap().len(), 2);
        let r = s.handle(&mut session, json!({"jsonrpc": "2.0", "id": 5, "method": "sampling/x"})).await.unwrap();
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        let r = s.handle(&mut session, json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {}})).await.unwrap();
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        let r = s
            .handle(&mut session, json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "nope"}}))
            .await
            .unwrap();
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn slugs() {
        let slug = |n: &str| ClientInfo { name: n.into(), version: String::new() }.slug();
        assert_eq!(slug("Claude Code"), "claude-code");
        assert_eq!(slug("  Cursor!! "), "cursor");
        assert_eq!(slug("??"), "mcp-client");
    }

    #[tokio::test]
    async fn serve_lines_round_trip() {
        let gate = Arc::new(TestGate::default());
        gate.approved.lock().unwrap().push("t".into());
        let s = server(gate);
        let (client, srv) = tokio::io::duplex(4096);
        let (sr, sw) = tokio::io::split(srv);
        let task = tokio::spawn(serve_lines(s, Transport::Stdio, sr, sw));
        let (cr, mut cw) = tokio::io::split(client);
        let mut lines = BufReader::new(cr).lines();
        for msg in [init("t"), json!({"jsonrpc":"2.0","method":"notifications/initialized"}), call(2)] {
            cw.write_all(format!("{msg}\n").as_bytes()).await.unwrap();
        }
        cw.write_all(b"not json\n").await.unwrap();
        let first: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first["id"], 1);
        let second: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(second["result"]["content"][0]["text"], "hello t");
        let third: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(third["error"]["code"], PARSE_ERROR);
        // Both split halves share the stream: drop both so the server sees EOF.
        drop(cw);
        drop(lines);
        tokio::time::timeout(std::time::Duration::from_secs(5), task).await.expect("server exits on EOF").unwrap().unwrap();
    }
}
