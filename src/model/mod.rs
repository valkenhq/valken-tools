//! Bring-your-own-model clients.  Valken never provides or pays for a model:
//! the user picks Anthropic, any OpenAI-compatible endpoint, or a local
//! runtime, and requests go straight there.
//!
//! Two wire formats cover all three ([`anthropic`], [`openai`]); both stream
//! over SSE into the same [`Response`].

pub mod anthropic;
pub mod openai;
pub mod sse;
mod types;

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use types::{Block, Delta, Message, Request, Response, Role, StopReason, ToolSchema, Usage};

/// Where requests go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    Anthropic,
    /// Any OpenAI-compatible endpoint (needs a key).
    OpenAi,
    /// Ollama, LM Studio: OpenAI-compatible on this machine, no key.
    Local,
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "Anthropic",
            ProviderKind::OpenAi => "OpenAI-compatible",
            ProviderKind::Local => "Local",
        }
    }

    pub fn default_base(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => anthropic::DEFAULT_BASE,
            ProviderKind::OpenAi => "https://api.openai.com/v1",
            ProviderKind::Local => openai::OLLAMA_BASE,
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "claude-sonnet-5-5",
            ProviderKind::OpenAi | ProviderKind::Local => "",
        }
    }

    pub fn needs_key(self) -> bool {
        self != ProviderKind::Local
    }
}

/// Model settings (no secrets: the key lives in the OS keychain).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelSettings {
    pub provider: ProviderKind,
    /// Empty means [`ProviderKind::default_base`].
    pub base_url: String,
    pub model: String,
    /// Cheaper model for watch-mode summaries and the digest; empty = `model`.
    pub watch_model: String,
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            provider: ProviderKind::Anthropic,
            base_url: String::new(),
            model: ProviderKind::Anthropic.default_model().into(),
            watch_model: "claude-haiku-5-5".into(),
        }
    }
}

impl ModelSettings {
    pub fn base(&self) -> &str {
        if self.base_url.trim().is_empty() {
            self.provider.default_base()
        } else {
            self.base_url.trim()
        }
    }

    pub fn watch_model(&self) -> &str {
        if self.watch_model.is_empty() {
            &self.model
        } else {
            &self.watch_model
        }
    }

    /// Requests would leave this machine.
    pub fn is_remote(&self) -> bool {
        let host = url::Url::parse(self.base()).ok().and_then(|u| u.host_str().map(str::to_owned));
        !matches!(host.as_deref(), Some("127.0.0.1" | "localhost" | "::1" | "[::1]"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    #[error("no API key: add one in Settings → Agent")]
    NoKey,
    #[error("no model chosen: pick one in Settings → Agent")]
    NoModel,
    #[error("the provider rejected the key: {0}")]
    Auth(String),
    #[error("rate limited by the provider")]
    RateLimited { retry_after: Option<Duration> },
    #[error("the provider is overloaded: {0}")]
    Overloaded(String),
    #[error("the provider rejected the request: {0}")]
    BadRequest(String),
    #[error("{0}")]
    Provider(String),
    #[error("couldn't reach the model endpoint: {0}")]
    Network(String),
    #[error("unexpected response: {0}")]
    Protocol(String),
}

impl ModelError {
    fn retryable(&self) -> bool {
        matches!(self, ModelError::RateLimited { .. } | ModelError::Overloaded(_) | ModelError::Network(_))
    }
}

const RETRIES: u32 = 2;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest silence between stream chunks before giving up.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Exactly what the last call sent (Settings → "Preview request").
#[derive(Debug, Clone, PartialEq)]
pub struct LastRequest {
    pub at: chrono::DateTime<chrono::Utc>,
    pub provider: ProviderKind,
    pub url: String,
    /// The JSON body, byte for byte (headers, and so the key, excluded).
    pub body: Value,
    /// Input tokens the provider counted, once it answered.
    pub input_tokens: Option<u32>,
}

/// A configured model endpoint.
#[derive(Clone)]
pub struct ModelClient {
    http: reqwest::Client,
    settings: ModelSettings,
    key: Option<String>,
    last: std::sync::Arc<std::sync::Mutex<Option<LastRequest>>>,
}

impl std::fmt::Debug for ModelClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelClient").field("settings", &self.settings).field("key", &self.key.as_ref().map(|_| "…")).finish()
    }
}

impl ModelClient {
    pub fn new(settings: ModelSettings, key: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(IDLE_TIMEOUT)
            .build()
            .expect("reqwest client");
        Self { http, settings, key: key.filter(|k| !k.trim().is_empty()), last: Default::default() }
    }

    /// The last request this client (or a clone of it) sent.
    pub fn last_request(&self) -> Option<LastRequest> {
        self.last.lock().ok()?.clone()
    }

    pub fn settings(&self) -> &ModelSettings {
        &self.settings
    }

    /// The exact JSON body a request would send (no headers, so no key):
    /// what "Preview request" shows.
    pub fn body(&self, req: &Request<'_>) -> Value {
        match self.settings.provider {
            ProviderKind::Anthropic => anthropic::body(self.settings.base(), req),
            ProviderKind::OpenAi | ProviderKind::Local => openai::body(req),
        }
    }

    fn check(&self, model: &str) -> Result<(), ModelError> {
        if self.settings.provider.needs_key() && self.key.is_none() {
            return Err(ModelError::NoKey);
        }
        if model.trim().is_empty() {
            return Err(ModelError::NoModel);
        }
        Ok(())
    }

    /// One streamed call, retrying rate limits / overloads / network errors
    /// that happen before any output.
    #[tracing::instrument(name = "agent.model.call", skip_all, fields(model = req.model, input = tracing::field::Empty, output = tracing::field::Empty))]
    pub async fn send(&self, req: &Request<'_>, on_delta: &mut (dyn FnMut(Delta) + Send)) -> Result<Response, ModelError> {
        self.check(req.model)?;
        let body = self.body(req);
        if let Ok(mut last) = self.last.lock() {
            let url = match self.settings.provider {
                ProviderKind::Anthropic => anthropic::url(self.settings.base()),
                _ => openai::url(self.settings.base()),
            };
            *last = Some(LastRequest { at: chrono::Utc::now(), provider: self.settings.provider, url, body: body.clone(), input_tokens: None });
        }
        let mut attempt = 0;
        loop {
            let mut emitted = false;
            let mut sink = |d: Delta| {
                emitted = true;
                on_delta(d);
            };
            match self.stream_once(req.model, &body, &mut sink).await {
                Err(e) if e.retryable() && !emitted && attempt < RETRIES => {
                    attempt += 1;
                    let wait = match &e {
                        ModelError::RateLimited { retry_after: Some(d) } => (*d).min(Duration::from_secs(30)),
                        _ => Duration::from_millis(500 * 2u64.pow(attempt)),
                    };
                    tracing::warn!(error = %e, attempt, "model call failed, retrying");
                    tokio::time::sleep(wait).await;
                }
                Ok(resp) => {
                    if let Ok(mut last) = self.last.lock() {
                        if let Some(l) = last.as_mut() {
                            l.input_tokens = Some(resp.usage.input_tokens + resp.usage.cache_read_tokens);
                        }
                    }
                    let span = tracing::Span::current();
                    span.record("input", resp.usage.input_tokens);
                    span.record("output", resp.usage.output_tokens);
                    return Ok(resp);
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn stream_once(&self, model: &str, body: &Value, on_delta: &mut (dyn FnMut(Delta) + Send)) -> Result<Response, ModelError> {
        let base = self.settings.base();
        let mut rb = match self.settings.provider {
            ProviderKind::Anthropic => {
                let mut rb = self
                    .http
                    .post(anthropic::url(base))
                    .header("anthropic-version", anthropic::VERSION)
                    .header("x-api-key", self.key.as_deref().unwrap_or_default());
                if let Some(beta) = anthropic::beta(base, model) {
                    rb = rb.header("anthropic-beta", beta);
                }
                rb
            }
            ProviderKind::OpenAi | ProviderKind::Local => self.http.post(openai::url(base)),
        };
        if self.settings.provider != ProviderKind::Anthropic {
            if let Some(key) = &self.key {
                rb = rb.bearer_auth(key);
            }
        }
        rb = rb.header("accept", "text/event-stream").json(body);
        let resp = rb.send().await.map_err(|e| ModelError::Network(e.without_url().to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs);
            let text = resp.text().await.unwrap_or_default();
            let msg = match self.settings.provider {
                ProviderKind::Anthropic => anthropic::error_message(&text),
                _ => openai::error_message(&text),
            };
            return Err(match status.as_u16() {
                401 | 403 => ModelError::Auth(msg),
                429 => ModelError::RateLimited { retry_after },
                500..=599 => ModelError::Overloaded(msg),
                _ => ModelError::BadRequest(msg),
            });
        }
        let mut decoder = sse::Decoder::default();
        let mut fold = Fold::new(self.settings.provider);
        let mut deltas = Vec::new();
        let mut resp = resp;
        while let Some(chunk) = resp.chunk().await.map_err(|e| ModelError::Network(e.without_url().to_string()))? {
            for ev in decoder.push(&chunk) {
                fold.event(&ev, &mut deltas)?;
            }
            deltas.drain(..).for_each(&mut *on_delta);
        }
        if let Some(ev) = decoder.finish() {
            fold.event(&ev, &mut deltas)?;
            deltas.drain(..).for_each(&mut *on_delta);
        }
        fold.finish()
    }

    /// Model ids the endpoint offers (for the picker).
    pub async fn list_models(&self) -> Result<Vec<String>, ModelError> {
        if self.settings.provider.needs_key() && self.key.is_none() {
            return Err(ModelError::NoKey);
        }
        let base = self.settings.base().trim_end_matches('/');
        let rb = match self.settings.provider {
            ProviderKind::Anthropic => self
                .http
                .get(format!("{base}/v1/models?limit=100"))
                .header("anthropic-version", anthropic::VERSION)
                .header("x-api-key", self.key.as_deref().unwrap_or_default()),
            _ => {
                let rb = self.http.get(format!("{base}/models"));
                match &self.key {
                    Some(k) => rb.bearer_auth(k),
                    None => rb,
                }
            }
        };
        let resp = rb.send().await.map_err(|e| ModelError::Network(e.without_url().to_string()))?;
        if !resp.status().is_success() {
            let code = resp.status().as_u16();
            let msg = resp.text().await.unwrap_or_default();
            return Err(if code == 401 || code == 403 { ModelError::Auth(msg) } else { ModelError::BadRequest(msg) });
        }
        let v: Value = resp.json().await.map_err(|e| ModelError::Protocol(e.to_string()))?;
        let mut ids: Vec<String> = v["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().map(str::to_owned))
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// "Test connection & tools": one call offering every tool plus a probe
    /// tool the model is asked to call.  A model that answers in text instead
    /// works in chat-only mode.
    pub async fn probe(&self, tools: &[ToolSchema]) -> Result<Probe, ModelError> {
        let mut offered = tools.to_vec();
        offered.push(ToolSchema {
            name: PROBE_TOOL.into(),
            description: "Connection test. Call this when asked to run the connection test.".into(),
            schema: json!({"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"]}),
        });
        let messages = [Message::user(format!(
            "Run the connection test: call the {PROBE_TOOL} tool with ok=true. Do not call any other tool and do not write anything else."
        ))];
        let req = Request {
            model: &self.settings.model,
            system: "You are testing a tool integration.",
            tools: &offered,
            messages: &messages,
            max_tokens: 2048,
        };
        let started = Instant::now();
        let resp = self.send(&req, &mut |_| {}).await?;
        let tool_calling = resp.content.iter().any(|b| matches!(b, Block::ToolUse { name, .. } if name == PROBE_TOOL));
        Ok(Probe { latency: started.elapsed(), tool_calling, tools_offered: tools.len() })
    }
}

const PROBE_TOOL: &str = "valken_probe";

/// Per-wire stream state.
enum Fold {
    Anthropic(anthropic::Fold),
    OpenAi(openai::Fold),
}

impl Fold {
    fn new(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::Anthropic => Fold::Anthropic(Default::default()),
            ProviderKind::OpenAi | ProviderKind::Local => Fold::OpenAi(Default::default()),
        }
    }

    fn event(&mut self, ev: &sse::Event, out: &mut Vec<Delta>) -> Result<(), ModelError> {
        match self {
            Fold::Anthropic(f) => f.event(ev, out),
            Fold::OpenAi(f) => f.event(ev, out),
        }
    }

    fn finish(self) -> Result<Response, ModelError> {
        match self {
            Fold::Anthropic(f) => f.finish(),
            Fold::OpenAi(f) => f.finish(),
        }
    }
}

/// Result of [`ModelClient::probe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    pub latency: Duration,
    /// The model called the probe tool.  False: offer chat-only mode.
    pub tool_calling: bool,
    /// Every Valken tool schema was accepted by the endpoint.
    pub tools_offered: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_defaults_and_remote() {
        let s = ModelSettings::default();
        assert_eq!(s.base(), "https://api.anthropic.com");
        assert_eq!(s.model, "claude-sonnet-5-5");
        assert!(s.is_remote());
        let local = ModelSettings { provider: ProviderKind::Local, model: "llama3.1".into(), ..ModelSettings::default() };
        assert_eq!(local.base(), "http://127.0.0.1:11434/v1");
        assert!(!local.is_remote());
        let blank = ModelSettings { watch_model: String::new(), ..ModelSettings::default() };
        assert_eq!(blank.watch_model(), "claude-sonnet-5-5");
    }

    #[tokio::test]
    async fn missing_key_or_model_fails_fast() {
        let c = ModelClient::new(ModelSettings::default(), Some("  ".into()));
        let req = Request { model: "m", system: "", tools: &[], messages: &[], max_tokens: 1 };
        assert_eq!(c.send(&req, &mut |_| {}).await, Err(ModelError::NoKey));
        let local = ModelClient::new(ModelSettings { provider: ProviderKind::Local, ..Default::default() }, None);
        let req = Request { model: "", ..req };
        assert_eq!(local.send(&req, &mut |_| {}).await, Err(ModelError::NoModel));
    }

    #[test]
    fn debug_hides_the_key() {
        let c = ModelClient::new(ModelSettings::default(), Some("sk-ant-secret".into()));
        assert!(!format!("{c:?}").contains("secret"));
    }
}
