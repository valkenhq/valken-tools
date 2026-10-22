//! Anthropic Messages API (`POST /v1/messages`, streamed).

use serde_json::{json, Map, Value};

use super::sse;
use super::types::{Block, Delta, Message, Request, Response, StopReason, Usage};
use super::ModelError;

pub const DEFAULT_BASE: &str = "https://api.anthropic.com";
pub const VERSION: &str = "2023-06-01";
/// Server-side refusal fallback (`fallbacks: "default"`).
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// Models that accept `fallbacks: "default"` on the first-party API.
fn supports_fallbacks(model: &str) -> bool {
    ["claude-sonnet-5-5", "claude-opus-5-5", "claude-opus-5", "claude-fable-5-1"].contains(&model)
}

pub fn url(base: &str) -> String {
    format!("{}/v1/messages", base.trim_end_matches('/'))
}

/// Extra `anthropic-beta` header for this request, if any.
pub fn beta(base: &str, model: &str) -> Option<&'static str> {
    (base.trim_end_matches('/') == DEFAULT_BASE && supports_fallbacks(model)).then_some(FALLBACK_BETA)
}

pub fn body(base: &str, req: &Request<'_>) -> Value {
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.schema}))
        .collect();
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "stream": true,
        // Tools render before the system prompt, so this breakpoint caches both.
        "system": [{"type": "text", "text": req.system, "cache_control": {"type": "ephemeral"}}],
        "messages": req.messages.iter().map(message).collect::<Vec<_>>(),
        // Automatic breakpoint on the last cacheable block: the growing history.
        "cache_control": {"type": "ephemeral"},
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if beta(base, req.model).is_some() {
        body["fallbacks"] = json!("default");
    }
    body
}

fn message(m: &Message) -> Value {
    let content: Vec<Value> = m
        .content
        .iter()
        .map(|b| match b {
            Block::Text { text } => json!({"type": "text", "text": text}),
            Block::ToolUse { id, name, input } => json!({"type": "tool_use", "id": id, "name": name, "input": input}),
            Block::ToolResult { id, content, is_error } => {
                json!({"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error})
            }
            Block::Opaque { value } => value.clone(),
        })
        .collect();
    json!({"role": m.role, "content": content})
}

/// Folds the SSE stream into deltas and a final [`Response`].
#[derive(Debug, Default)]
pub struct Fold {
    blocks: Vec<Value>,
    /// Partial tool input JSON per block index.
    inputs: Vec<String>,
    stop: Option<StopReason>,
    usage: Usage,
}

impl Fold {
    pub fn event(&mut self, ev: &sse::Event, out: &mut Vec<Delta>) -> Result<(), ModelError> {
        let v: Value = serde_json::from_str(&ev.data).map_err(|e| ModelError::Protocol(e.to_string()))?;
        match v["type"].as_str().unwrap_or(&ev.event) {
            "message_start" => {
                let u = &v["message"]["usage"];
                self.usage.input_tokens = u32_of(&u["input_tokens"]);
                self.usage.cache_read_tokens = u32_of(&u["cache_read_input_tokens"]);
            }
            "content_block_start" => {
                let i = index(&v);
                let block = v["content_block"].clone();
                if block["type"] == "tool_use" {
                    out.push(Delta::ToolStart {
                        id: block["id"].as_str().unwrap_or_default().to_owned(),
                        name: block["name"].as_str().unwrap_or_default().to_owned(),
                    });
                }
                if self.blocks.len() <= i {
                    self.blocks.resize(i + 1, Value::Null);
                    self.inputs.resize(i + 1, String::new());
                }
                self.blocks[i] = block;
            }
            "content_block_delta" => {
                let i = index(&v);
                let d = &v["delta"];
                let Some(block) = self.blocks.get_mut(i) else {
                    return Err(ModelError::Protocol(format!("delta for unknown block {i}")));
                };
                match d["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        let t = d["text"].as_str().unwrap_or_default();
                        append(block, "text", t);
                        out.push(Delta::Text(t.to_owned()));
                    }
                    "input_json_delta" => self.inputs[i].push_str(d["partial_json"].as_str().unwrap_or_default()),
                    "thinking_delta" => append(block, "thinking", d["thinking"].as_str().unwrap_or_default()),
                    "signature_delta" => block["signature"] = d["signature"].clone(),
                    _ => {}
                }
            }
            "content_block_stop" => {
                let i = index(&v);
                if let Some(block) = self.blocks.get_mut(i).filter(|b| b["type"] == "tool_use") {
                    let raw = std::mem::take(&mut self.inputs[i]);
                    block["input"] = if raw.trim().is_empty() {
                        json!({})
                    } else {
                        // Invalid JSON goes to the tool, which reports it back as an input error.
                        serde_json::from_str(&raw).unwrap_or_else(|_| json!({ "INVALID_JSON": raw }))
                    };
                }
            }
            "message_delta" => {
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    self.stop = Some(stop_reason(r));
                }
                if let Some(n) = v["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = n as u32;
                }
            }
            "error" => return Err(stream_error(&v["error"])),
            _ => {}
        }
        Ok(())
    }

    pub fn finish(self) -> Result<Response, ModelError> {
        let stop = self.stop.ok_or_else(|| ModelError::Protocol("stream ended without a stop reason".into()))?;
        let content = self.blocks.into_iter().filter(|b| !b.is_null()).map(block).collect();
        Ok(Response { content, stop, usage: self.usage })
    }
}

fn index(v: &Value) -> usize {
    v["index"].as_u64().unwrap_or(0) as usize
}

fn u32_of(v: &Value) -> u32 {
    v.as_u64().unwrap_or(0) as u32
}

fn append(block: &mut Value, field: &str, s: &str) {
    let cur = block[field].as_str().unwrap_or_default();
    block[field] = Value::String(format!("{cur}{s}"));
}

fn block(v: Value) -> Block {
    match v["type"].as_str() {
        Some("text") => Block::Text { text: v["text"].as_str().unwrap_or_default().to_owned() },
        Some("tool_use") => Block::ToolUse {
            id: v["id"].as_str().unwrap_or_default().to_owned(),
            name: v["name"].as_str().unwrap_or_default().to_owned(),
            input: v["input"].clone(),
        },
        _ => Block::Opaque { value: strip_nulls(v) },
    }
}

fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.into_iter().filter(|(_, v)| !v.is_null()).collect::<Map<_, _>>()),
        v => v,
    }
}

fn stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" | "stop_sequence" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        other => StopReason::Other(other.to_owned()),
    }
}

fn stream_error(e: &Value) -> ModelError {
    let msg = e["message"].as_str().unwrap_or("unknown error").to_owned();
    match e["type"].as_str() {
        Some("overloaded_error") | Some("api_error") => ModelError::Overloaded(msg),
        Some("rate_limit_error") => ModelError::RateLimited { retry_after: None },
        _ => ModelError::Provider(msg),
    }
}

/// The `error.message` of a non-2xx JSON body.
pub fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(300).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{Role, ToolSchema};

    fn fold(events: &[&str]) -> (Vec<Delta>, Result<Response, ModelError>) {
        let mut f = Fold::default();
        let mut out = Vec::new();
        for data in events {
            let ev = sse::Event { event: String::new(), data: (*data).to_owned() };
            if let Err(e) = f.event(&ev, &mut out) {
                return (out, Err(e));
            }
        }
        (out, f.finish())
    }

    #[test]
    fn folds_text_thinking_and_tool_use() {
        let (deltas, resp) = fold(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":120,"cache_read_input_tokens":100}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Look"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ing."}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"tu1","name":"get_events","input":{}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"namespace\":"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"shop\"}"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let resp = resp.unwrap();
        assert_eq!(
            deltas,
            [
                Delta::Text("Look".into()),
                Delta::Text("ing.".into()),
                Delta::ToolStart { id: "tu1".into(), name: "get_events".into() },
            ]
        );
        assert_eq!(resp.stop, StopReason::ToolUse);
        assert_eq!(resp.usage, Usage { input_tokens: 120, output_tokens: 42, cache_read_tokens: 100 });
        assert_eq!(resp.content.len(), 3);
        assert_eq!(
            resp.content[0],
            Block::Opaque { value: json!({"type": "thinking", "thinking": "hmm", "signature": "sig"}) }
        );
        assert_eq!(resp.content[1], Block::text("Looking."));
        assert_eq!(
            resp.content[2],
            Block::ToolUse { id: "tu1".into(), name: "get_events".into(), input: json!({"namespace": "shop"}) }
        );
    }

    #[test]
    fn invalid_tool_json_is_kept_for_the_error_result() {
        let (_, resp) = fold(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"x","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
        ]);
        let resp = resp.unwrap();
        assert_eq!(resp.stop, StopReason::MaxTokens);
        assert!(matches!(&resp.content[0], Block::ToolUse { input, .. } if input["INVALID_JSON"] == "{\"a\":"));
    }

    #[test]
    fn stream_errors_and_missing_stop() {
        let (_, r) = fold(&[r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#]);
        assert!(matches!(r, Err(ModelError::Overloaded(m)) if m == "busy"));
        let (_, r) = fold(&[r#"{"type":"message_start","message":{}}"#]);
        assert!(matches!(r, Err(ModelError::Protocol(_))));
    }

    #[test]
    fn body_shape() {
        let tools = [ToolSchema { name: "get_events".into(), description: "d".into(), schema: json!({"type": "object"}) }];
        let msgs = [
            Message::user("hi"),
            Message::assistant(vec![
                Block::Opaque { value: json!({"type": "thinking", "thinking": "", "signature": "s"}) },
                Block::ToolUse { id: "t1".into(), name: "get_events".into(), input: json!({}) },
            ]),
            Message { role: Role::User, content: vec![Block::ToolResult { id: "t1".into(), content: "[]".into(), is_error: false }] },
        ];
        let req = Request { model: "claude-sonnet-5-5", system: "sys", tools: &tools, messages: &msgs, max_tokens: 1000 };
        let b = body(DEFAULT_BASE, &req);
        assert_eq!(b["stream"], true);
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(b["messages"][1]["content"][0]["signature"], "s");
        assert_eq!(b["messages"][2]["content"][0]["tool_use_id"], "t1");
        assert_eq!(b["fallbacks"], "default");
        assert_eq!(beta(DEFAULT_BASE, "claude-sonnet-5-5"), Some(FALLBACK_BETA));
        // Proxies and older models get neither.
        assert_eq!(beta("https://proxy.local", "claude-sonnet-5-5"), None);
        assert!(body("https://proxy.local", &req).get("fallbacks").is_none());
        assert_eq!(beta(DEFAULT_BASE, "claude-haiku-5-5"), None);
    }

    #[test]
    fn error_message_extraction() {
        assert_eq!(error_message(r#"{"type":"error","error":{"type":"x","message":"bad key"}}"#), "bad key");
        assert_eq!(error_message("plain"), "plain");
    }
}
