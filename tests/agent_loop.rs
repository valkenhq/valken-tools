//! The agent loop end to end against a scripted model server speaking each
//! wire format: tool call → registry (redaction + audit) → tool result →
//! final answer, with the history valid for the next turn.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use valken_tools::agent::{Agent, AgentError, Budget, Event, Spend, Turn};
use valken_tools::audit::{AuditLog, Filter};
use valken_tools::model::{Block, Message, ModelClient, ModelSettings, ProviderKind, Role, StopReason};
use valken_tools::object::ObjectRef;
use valken_tools::redact::Redactor;
use valken_tools::tool::{Access, Actor, Cx, Registry, Tier, Tool, ToolError, ToolOutput, ToolSpec};

// ── A scripted SSE server ──────────────────────────────────────────────

/// Serves one scripted SSE body per request, in order; records each
/// request body.
async fn model_server(script: Vec<String>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        for body in script {
            let (mut sock, _) = listener.accept().await.unwrap();
            let request = read_request(&mut sock).await;
            log.lock().unwrap().push(request);
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(body.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
        }
    });
    (format!("http://{addr}"), seen)
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Value {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = sock.read(&mut chunk).await.unwrap();
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf);
        if let Some(split) = text.find("\r\n\r\n") {
            let len: usize = text[..split]
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                .unwrap_or(0);
            if buf.len() >= split + 4 + len {
                return serde_json::from_slice(&buf[split + 4..split + 4 + len]).unwrap();
            }
        }
        if n == 0 {
            panic!("client closed early");
        }
    }
}

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or("message"))).collect()
}

fn anthropic_tool_call() -> String {
    sse(&[
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 50}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-1"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "tu_1", "name": "get_status", "input": {}}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"name\": \"checkout\"}"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 20}}),
        json!({"type": "message_stop"}),
    ])
}

fn anthropic_answer(text: &str) -> String {
    sse(&[
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 80, "cache_read_input_tokens": 40}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 9}}),
    ])
}

fn openai(chunks: &[Value]) -> String {
    let mut s: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    s.push_str("data: [DONE]\n\n");
    s
}

// ── A tool that leaks a token (redaction must catch it) ────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusIn {
    name: String,
}

struct GetStatus;

#[async_trait]
impl Tool for GetStatus {
    type Input = StatusIn;
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: "get_status", description: "Status of a workload.", access: Access::Read, tier: Tier::Free }
    }
    fn target(&self, i: &StatusIn) -> String {
        format!("shop/{}", i.name)
    }
    async fn run(&self, _cx: Cx<'_>, i: StatusIn) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(format!("{} OOMKilled x3; log: Authorization: Bearer abcdefghijklmnopqrstuvwxyz", i.name))
            .cite(ObjectRef::new("kind-a", "deployment", Some("shop"), &i.name))
            .note("1 status"))
    }
}

fn agent(base: &str, provider: ProviderKind, audit: Arc<AuditLog>, cap: u32) -> Agent {
    let mut registry = Registry::new(Redactor::default(), Some(audit));
    registry.register(GetStatus);
    let settings = ModelSettings { provider, base_url: base.into(), model: "test-model".into(), watch_model: String::new() };
    Agent { registry: Arc::new(registry), client: ModelClient::new(settings, Some("k".into())), budget: Arc::new(Budget::new(cap)) }
}

fn turn(spend: Spend) -> Turn<'static> {
    Turn { actor: &Actor::Agent, pro: false, model: "test-model", system: "sys", spend, tools: true }
}

#[tokio::test]
async fn anthropic_loop_runs_tools_redacts_and_audits() {
    let (base, seen) = model_server(vec![anthropic_tool_call(), anthropic_answer("{{deployment shop/checkout}} is OOMKilled.")]).await;
    let dir = tempfile::tempdir().unwrap();
    let audit = Arc::new(AuditLog::open(dir.path()).unwrap());
    let agent = agent(&base, ProviderKind::Anthropic, audit.clone(), 60);
    let mut history = vec![Message::user("why is checkout down?")];
    let mut events = Vec::new();
    let outcome = agent.run(turn(Spend::Chat), &mut history, &mut |e| events.push(e)).await.unwrap();

    assert_eq!(outcome.stop, StopReason::EndTurn);
    assert_eq!(outcome.calls, 2);
    assert_eq!(outcome.usage.input_tokens, 130);
    // user, assistant(thinking + tool_use), user(tool_result), assistant(text)
    assert_eq!(history.len(), 4);
    let Block::ToolResult { content, is_error, .. } = &history[2].content[0] else { panic!("{:?}", history[2]) };
    assert!(!is_error);
    assert!(content.contains("OOMKilled") && !content.contains("abcdefghijklmnop"), "redacted: {content}");
    assert_eq!(history[3].text(), "{{deployment shop/checkout}} is OOMKilled.");

    let done = events.iter().find_map(|e| match e {
        Event::ToolDone { summary, cites, .. } => Some((summary.clone(), cites.clone())),
        _ => None,
    });
    let (summary, cites) = done.unwrap();
    assert_eq!(summary, "1 status · 1 masked");
    assert_eq!(cites[0].name, "checkout");

    // The second request echoed the thinking block (signature intact) and the result.
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["tools"][0]["name"], "get_status");
    assert_eq!(requests[1]["messages"][1]["content"][0]["signature"], "sig-1");
    assert_eq!(requests[1]["messages"][2]["content"][0]["type"], "tool_result");

    let rows = audit.read(&Filter::default(), 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].actor.as_str(), rows[0].action.as_str(), rows[0].target.as_str()), ("agent", "get_status", "shop/checkout"));
    // The client remembers the exact last request for the preview.
    let last = agent.client.last_request().unwrap();
    assert_eq!(last.body, requests[1]);
    assert_eq!(last.input_tokens, Some(120));
}

#[tokio::test]
async fn openai_compatible_loop() {
    let call = openai(&[
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "get_status", "arguments": "{\"name\":"}}]}}]}),
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\"ledger\"}"}}]}, "finish_reason": "tool_calls"}]}),
    ]);
    let answer = openai(&[
        json!({"choices": [{"delta": {"content": "Ledger "}}]}),
        json!({"choices": [{"delta": {"content": "is crashing."}, "finish_reason": "stop"}]}),
    ]);
    let (base, seen) = model_server(vec![call, answer]).await;
    let dir = tempfile::tempdir().unwrap();
    let agent = agent(&base, ProviderKind::Local, Arc::new(AuditLog::open(dir.path()).unwrap()), 60);
    let mut history = vec![Message::user("ledger?")];
    let mut text = String::new();
    agent
        .run(turn(Spend::Chat), &mut history, &mut |e| {
            if let Event::Text(t) = e {
                text.push_str(&t);
            }
        })
        .await
        .unwrap();
    assert_eq!(text, "Ledger is crashing.");
    let requests = seen.lock().unwrap().clone();
    let msgs = requests[1]["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[2]["tool_calls"][0]["function"]["arguments"], "{\"name\":\"ledger\"}");
    assert_eq!(msgs[3]["role"], "tool");
    assert!(msgs[3]["content"].as_str().unwrap().contains("ledger OOMKilled"));
}

#[tokio::test]
async fn auto_spend_respects_the_budget_and_leaves_history_valid() {
    let (base, _) = model_server(vec![anthropic_tool_call()]).await;
    let dir = tempfile::tempdir().unwrap();
    let agent = agent(&base, ProviderKind::Anthropic, Arc::new(AuditLog::open(dir.path()).unwrap()), 1);
    let mut history = vec![Message::user("watch")];
    let err = agent.run(turn(Spend::Auto), &mut history, &mut |_| {}).await.unwrap_err();
    assert!(matches!(err, AgentError::Budget(_)));
    // The first call ran its tool; the second was refused.  The tool results
    // are already in history, so the next turn's request stays valid.
    assert_eq!(history.last().unwrap().role, Role::User);
    assert!(history.iter().all(|m| m.role == Role::User || !m.content.is_empty()));
}

#[tokio::test]
async fn provider_errors_surface() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        read_request(&mut sock).await;
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        let resp = format!("HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
        sock.write_all(resp.as_bytes()).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let agent = agent(&format!("http://{addr}"), ProviderKind::Anthropic, Arc::new(AuditLog::open(dir.path()).unwrap()), 60);
    let mut history = vec![Message::user("hi")];
    let err = agent.run(turn(Spend::Chat), &mut history, &mut |_| {}).await.unwrap_err();
    assert_eq!(err.to_string(), "the provider rejected the key: invalid x-api-key");
}
