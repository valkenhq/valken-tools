//! `valken-mcp --stdio` end to end: the real binary relaying a client's
//! stdio to an in-process server on a temporary socket, and the "Valken
//! isn't running" answer when nothing listens.
#![cfg(unix)]

use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use valken_tools::mcp::{self, Admission, ClientInfo, Gate, Transport};
use valken_tools::redact::Redactor;
use valken_tools::tool::{Access, Cx, Registry, Tier, Tool, ToolError, ToolOutput, ToolSpec};

#[derive(Deserialize, JsonSchema)]
struct Empty {}

struct Ping;

#[async_trait]
impl Tool for Ping {
    type Input = Empty;
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: "ping", description: "Ping.", access: Access::Read, tier: Tier::Free }
    }
    fn target(&self, _: &Empty) -> String {
        "-".into()
    }
    async fn run(&self, cx: Cx<'_>, _: Empty) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(format!("pong for {}", cx.actor)))
    }
}

struct AllowAll;

impl Gate for AllowAll {
    fn admit(&self, _: &ClientInfo, t: Transport) -> Admission {
        assert_eq!(t, Transport::Stdio);
        Admission::Approved
    }
    fn pro(&self) -> bool {
        false
    }
}

fn relay(socket: &std::path::Path) -> tokio::process::Child {
    Command::new(env!("CARGO_BIN_EXE_valken-mcp"))
        .arg("--stdio")
        .env("VALKEN_MCP_SOCKET", socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

#[tokio::test]
async fn relays_a_session_through_the_binary() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("mcp.sock");
    let mut registry = Registry::new(Redactor::default(), None);
    registry.register(Ping);
    let server = mcp::Server::new(Arc::new(registry), Arc::new(AllowAll), "test");
    // The listener reads VALKEN_MCP_SOCKET too; bind it directly instead of
    // touching this process's environment.
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, w) = tokio::io::split(stream);
        mcp::serve_lines(server, Transport::Stdio, r, w).await.unwrap();
    });

    let mut child = relay(&socket);
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
    let send = |v: Value| format!("{v}\n");
    stdin
        .write_all(send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "Claude Code", "version": "2"}}}))
        .as_bytes())
        .await
        .unwrap();
    let init: Value = serde_json::from_str(&out.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "valken");
    stdin
        .write_all(send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "ping"}})).as_bytes())
        .await
        .unwrap();
    let call: Value = serde_json::from_str(&out.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(call["result"]["content"][0]["text"], "pong for claude-code");
    drop(stdin);
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await.unwrap().unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn says_valken_is_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = relay(&dir.path().join("nobody.sock"));
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n").await.unwrap();
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait_with_output()).await.unwrap().unwrap();
    assert!(!output.status.success());
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["id"], 1);
    assert!(reply["error"]["message"].as_str().unwrap().contains("isn't running"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Open Valken"));
}

#[tokio::test]
async fn rejects_bad_arguments() {
    let output = Command::new(env!("CARGO_BIN_EXE_valken-mcp")).output().await.unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage"));
}
