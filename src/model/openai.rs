//! OpenAI-compatible Chat Completions (`POST {base}/chat/completions`,
//! streamed).  Covers hosted endpoints and local runtimes (Ollama, LM Studio).

use serde_json::{json, Value};

use super::sse;
use super::types::{Block, Delta, Message, Request, Response, Role, StopReason, Usage};
use super::ModelError;

/// Ollama's OpenAI-compatible endpoint.
pub const OLLAMA_BASE: &str = "http://127.0.0.1:11434/v1";
/// Tool calls per message we'll track (see `anthropic::MAX_BLOCKS`).
const MAX_CALLS: usize = 256;

pub fn url(base: &str) -> String {
    format!("{}/chat/completions", base.trim_end_matches('/'))
}

pub fn body(req: &Request<'_>) -> Value {
    let mut messages = vec![json!({"role": "system", "content": req.system})];
    for m in req.messages {
        push_message(&mut messages, m);
    }
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": messages,
    });
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.schema}}))
            .collect();
    }
    body
}

/// One neutral message can become several wire messages: tool results are
/// their own `tool` role messages.
fn push_message(out: &mut Vec<Value>, m: &Message) {
    let text = m.text();
    match m.role {
        Role::Assistant => {
            let calls: Vec<Value> = m
                .tool_uses()
                .map(|(id, name, input)| {
                    json!({"id": id, "type": "function", "function": {"name": name, "arguments": input.to_string()}})
                })
                .collect();
            let mut msg = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { Value::String(text) }});
            if !calls.is_empty() {
                msg["tool_calls"] = Value::Array(calls);
            }
            out.push(msg);
        }
        Role::User => {
            for b in &m.content {
                if let Block::ToolResult {
                    id,
                    content,
                    is_error,
                } = b
                {
                    let content = if *is_error {
                        format!("ERROR: {content}")
                    } else {
                        content.clone()
                    };
                    out.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
                }
            }
            if !text.is_empty() {
                out.push(json!({"role": "user", "content": text}));
            }
        }
    }
}

#[derive(Debug, Default)]
struct Call {
    id: String,
    name: String,
    args: String,
}

#[derive(Debug, Default)]
pub struct Fold {
    text: String,
    calls: Vec<Call>,
    finish: Option<String>,
    usage: Usage,
    done: bool,
}

impl Fold {
    pub fn event(&mut self, ev: &sse::Event, out: &mut Vec<Delta>) -> Result<(), ModelError> {
        if ev.data.trim() == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let v: Value =
            serde_json::from_str(&ev.data).map_err(|e| ModelError::Protocol(e.to_string()))?;
        if let Some(e) = v.get("error") {
            return Err(ModelError::Provider(
                e["message"].as_str().unwrap_or("unknown error").to_owned(),
            ));
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage.input_tokens = u["prompt_tokens"].as_u64().unwrap_or(0) as u32;
            self.usage.output_tokens = u["completion_tokens"].as_u64().unwrap_or(0) as u32;
            self.usage.cache_read_tokens = u["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0) as u32;
        }
        let Some(choice) = v["choices"].get(0) else {
            return Ok(());
        };
        let d = &choice["delta"];
        if let Some(t) = d["content"].as_str().filter(|t| !t.is_empty()) {
            self.text.push_str(t);
            out.push(Delta::Text(t.to_owned()));
        }
        for tc in d["tool_calls"].as_array().into_iter().flatten() {
            let i = tc["index"].as_u64().unwrap_or(self.calls.len() as u64);
            if i >= MAX_CALLS as u64 {
                return Err(ModelError::Protocol(format!(
                    "tool call index {i} out of range"
                )));
            }
            let i = i as usize;
            if self.calls.len() <= i {
                self.calls.resize_with(i + 1, Call::default);
            }
            let call = &mut self.calls[i];
            if let Some(id) = tc["id"].as_str() {
                call.id = id.to_owned();
            }
            if let Some(name) = tc["function"]["name"].as_str().filter(|n| !n.is_empty()) {
                if call.name.is_empty() {
                    call.name = name.to_owned();
                    out.push(Delta::ToolStart {
                        id: call.id.clone(),
                        name: call.name.clone(),
                    });
                }
            }
            if let Some(args) = tc["function"]["arguments"].as_str() {
                call.args.push_str(args);
            }
        }
        if let Some(f) = choice["finish_reason"].as_str() {
            self.finish = Some(f.to_owned());
        }
        Ok(())
    }

    pub fn finish(self) -> Result<Response, ModelError> {
        if self.finish.is_none() && !self.done {
            return Err(ModelError::Protocol(
                "stream ended without a finish reason".into(),
            ));
        }
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(Block::Text { text: self.text });
        }
        let has_calls = !self.calls.is_empty();
        for (i, c) in self.calls.into_iter().enumerate() {
            let input = if c.args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&c.args).unwrap_or_else(|_| json!({ "INVALID_JSON": c.args }))
            };
            // Some local servers omit ids; tool results still need one.
            let id = if c.id.is_empty() {
                format!("call_{i}")
            } else {
                c.id
            };
            content.push(Block::ToolUse {
                id,
                name: c.name,
                input,
            });
        }
        let stop = match self.finish.as_deref() {
            Some("length") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::Refusal,
            // Local servers often report `stop` even when they called tools.
            _ if has_calls => StopReason::ToolUse,
            Some("stop") | None => StopReason::EndTurn,
            Some(other) => StopReason::Other(other.to_owned()),
        };
        Ok(Response {
            content,
            stop,
            usage: self.usage,
        })
    }
}

/// The error message of a non-2xx body (`{"error":{"message":…}}` or text).
pub fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.chars().take(300).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::ToolSchema;

    fn fold(events: &[&str]) -> (Vec<Delta>, Result<Response, ModelError>) {
        let mut f = Fold::default();
        let mut out = Vec::new();
        for data in events {
            f.event(
                &sse::Event {
                    event: String::new(),
                    data: (*data).into(),
                },
                &mut out,
            )
            .unwrap();
        }
        (out, f.finish())
    }

    #[test]
    fn folds_text_and_parallel_tool_calls() {
        let (deltas, resp) = fold(&[
            r#"{"choices":[{"delta":{"role":"assistant","content":"Chec"}}]}"#,
            r#"{"choices":[{"delta":{"content":"king"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"get_events","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c2","function":{"name":"get_logs","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"namespace\":\"a\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
            "[DONE]",
        ]);
        let resp = resp.unwrap();
        assert_eq!(deltas.len(), 4);
        assert_eq!(resp.stop, StopReason::ToolUse);
        assert_eq!(resp.content[0], Block::text("Checking"));
        assert_eq!(
            resp.content[1],
            Block::ToolUse {
                id: "c1".into(),
                name: "get_events".into(),
                input: json!({"namespace": "a"})
            }
        );
        assert!(matches!(&resp.content[2], Block::ToolUse { name, .. } if name == "get_logs"));
        assert_eq!(resp.usage.input_tokens, 10);
    }

    #[test]
    fn local_server_quirks() {
        // finish_reason "stop" with tool calls and no ids.
        let (_, resp) = fold(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"ping","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        let resp = resp.unwrap();
        assert_eq!(resp.stop, StopReason::ToolUse);
        assert!(
            matches!(&resp.content[0], Block::ToolUse { id, input, .. } if id == "call_0" && input == &json!({}))
        );
    }

    #[test]
    fn hostile_index_is_an_error() {
        let mut f = Fold::default();
        let ev = sse::Event {
            event: String::new(),
            data: r#"{"choices":[{"delta":{"tool_calls":[{"index":4000000000,"id":"c"}]}}]}"#
                .into(),
        };
        assert!(matches!(
            f.event(&ev, &mut Vec::new()),
            Err(ModelError::Protocol(_))
        ));
        let req = Request {
            model: "m",
            system: "s",
            tools: &[],
            messages: &[],
            max_tokens: 77,
        };
        assert_eq!(body(&req)["max_tokens"], 77);
    }

    #[test]
    fn truncated_stream_is_an_error() {
        let (_, resp) = fold(&[r#"{"choices":[{"delta":{"content":"x"}}]}"#]);
        assert!(matches!(resp, Err(ModelError::Protocol(_))));
    }

    #[test]
    fn body_maps_roles_and_tool_results() {
        let tools = [ToolSchema {
            name: "get_events".into(),
            description: "d".into(),
            schema: json!({"type": "object"}),
        }];
        let msgs = [
            Message::user("why?"),
            Message::assistant(vec![
                Block::Opaque {
                    value: json!({"type": "thinking"}),
                },
                Block::ToolUse {
                    id: "c1".into(),
                    name: "get_events".into(),
                    input: json!({"a": 1}),
                },
            ]),
            Message {
                role: Role::User,
                content: vec![Block::ToolResult {
                    id: "c1".into(),
                    content: "boom".into(),
                    is_error: true,
                }],
            },
        ];
        let req = Request {
            model: "llama3.1",
            system: "sys",
            tools: &tools,
            messages: &msgs,
            max_tokens: 1,
        };
        let b = body(&req);
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m.len(), 4);
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[2]["content"], Value::Null);
        assert_eq!(m[2]["tool_calls"][0]["function"]["arguments"], r#"{"a":1}"#);
        assert_eq!(
            m[3],
            json!({"role": "tool", "tool_call_id": "c1", "content": "ERROR: boom"})
        );
        assert_eq!(b["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn error_messages() {
        assert_eq!(
            error_message(r#"{"error":{"message":"no model"}}"#),
            "no model"
        );
        assert_eq!(
            error_message(r#"{"error":"model not found"}"#),
            "model not found"
        );
    }
}
