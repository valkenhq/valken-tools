//! Streamable HTTP transport on loopback (`POST /mcp`, JSON responses).
//!
//! Every request needs `Authorization: Bearer <token>` (the token Valken
//! shows in Settings → MCP server).  `Host` and `Origin` must be loopback,
//! which stops a web page from reaching the port through DNS rebinding.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::TcpListener;

use super::{parse_error, Server, Session, Transport};

pub const DEFAULT_PORT: u16 = 7441;
const SESSION_HEADER: &str = "mcp-session-id";
const MAX_BODY: usize = 1 << 20;

struct State {
    server: Arc<Server>,
    token: String,
    port: u16,
    sessions: Mutex<HashMap<String, Session>>,
}

/// Binds `127.0.0.1:port` (0 = any free port).  Returns the bound address
/// and the serve future, which runs until `shutdown` resolves.
pub async fn bind(
    server: Arc<Server>,
    port: u16,
    token: String,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<(SocketAddr, impl std::future::Future<Output = ()> + Send)> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    let state = Arc::new(State { server, token, port: addr.port(), sessions: Mutex::default() });
    tracing::info!(%addr, "mcp: http listening");
    let serve = async move {
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let state = state.clone();
                    tokio::spawn(async move {
                        let svc = service_fn(move |req| handle(state.clone(), req));
                        if let Err(e) = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await {
                            tracing::debug!(error = %e, "mcp: http connection ended");
                        }
                    });
                }
            }
        }
    };
    Ok((addr, serve))
}

fn reply(status: StatusCode, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(body.into()));
    *r.status_mut() = status;
    r
}

fn json(status: StatusCode, v: &Value) -> Response<Full<Bytes>> {
    let mut r = reply(status, serde_json::to_vec(v).unwrap_or_default());
    r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

/// Constant-time comparison, so the token can't be guessed byte by byte.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn loopback_host(host: &str, port: u16) -> bool {
    ["127.0.0.1", "localhost", "[::1]"].iter().any(|h| host == *h || host == format!("{h}:{port}"))
}

fn loopback_origin(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else { return false };
    matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
}

async fn handle(state: Arc<State>, req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let headers = req.headers();
    let host_ok = headers.get(HOST).and_then(|h| h.to_str().ok()).is_some_and(|h| loopback_host(h, state.port));
    let origin_ok = headers.get(ORIGIN).and_then(|o| o.to_str().ok()).is_none_or(loopback_origin);
    if !host_ok || !origin_ok {
        return Ok(reply(StatusCode::FORBIDDEN, "loopback only"));
    }
    if req.uri().path() != "/mcp" {
        return Ok(reply(StatusCode::NOT_FOUND, ""));
    }
    let authed = headers
        .get(AUTHORIZATION)
        .and_then(|a| a.to_str().ok())
        .and_then(|a| a.strip_prefix("Bearer "))
        .is_some_and(|t| same(t.trim().as_bytes(), state.token.as_bytes()));
    if !authed {
        return Ok(reply(StatusCode::UNAUTHORIZED, "bearer token required: see Valken → Settings → MCP server"));
    }
    let session_id = headers.get(SESSION_HEADER).and_then(|s| s.to_str().ok()).map(str::to_owned);
    match *req.method() {
        Method::POST => {}
        Method::DELETE => {
            if let Some(id) = session_id {
                state.sessions.lock().expect("sessions").remove(&id);
            }
            return Ok(reply(StatusCode::OK, ""));
        }
        // No server-initiated stream: we never send requests or notifications.
        _ => return Ok(reply(StatusCode::METHOD_NOT_ALLOWED, "")),
    }
    let body = match Limited::new(req.into_body(), MAX_BODY).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return Ok(reply(StatusCode::PAYLOAD_TOO_LARGE, "")),
    };
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return Ok(json(StatusCode::BAD_REQUEST, &parse_error(&e.to_string()))),
    };
    let initializing = msg["method"] == "initialize";
    let (id, mut session) = if initializing {
        (uuid::Uuid::new_v4().simple().to_string(), Session::new(Transport::Http))
    } else {
        let Some(id) = session_id else {
            return Ok(reply(StatusCode::BAD_REQUEST, "missing Mcp-Session-Id"));
        };
        let Some(s) = state.sessions.lock().expect("sessions").get(&id).cloned() else {
            return Ok(reply(StatusCode::NOT_FOUND, "unknown session"));
        };
        (id, s)
    };
    let out = state.server.handle(&mut session, msg).await;
    state.sessions.lock().expect("sessions").insert(id.clone(), session);
    let mut resp = match out {
        Some(v) => json(StatusCode::OK, &v),
        None => reply(StatusCode::ACCEPTED, ""),
    };
    if initializing {
        if let Ok(v) = HeaderValue::from_str(&id) {
            resp.headers_mut().insert(SESSION_HEADER, v);
        }
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tests::{server, TestGate};
    use serde_json::json;

    async fn start() -> (SocketAddr, tokio::sync::oneshot::Sender<()>, Arc<TestGate>) {
        let gate = Arc::new(TestGate::default());
        gate.approved.lock().unwrap().push("cursor".into());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let (addr, serve) = bind(server(gate.clone()), 0, "tok".into(), async move {
            let _ = rx.await;
        })
        .await
        .unwrap();
        tokio::spawn(serve);
        (addr, tx, gate)
    }

    fn post(addr: SocketAddr, token: Option<&str>, session: Option<&str>, body: Value) -> reqwest::RequestBuilder {
        let mut rb = reqwest::Client::new().post(format!("http://{addr}/mcp")).json(&body);
        if let Some(t) = token {
            rb = rb.bearer_auth(t);
        }
        if let Some(s) = session {
            rb = rb.header(SESSION_HEADER, s);
        }
        rb
    }

    fn init() -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "cursor"}}})
    }

    #[tokio::test]
    async fn session_flow() {
        let (addr, _stop, gate) = start().await;
        let r = post(addr, Some("tok"), None, init()).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let sid = r.headers()[SESSION_HEADER].to_str().unwrap().to_owned();
        assert_eq!(gate.seen.lock().unwrap()[0].1, Transport::Http);

        let r = post(addr, Some("tok"), Some(&sid), json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 202);

        let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "hello"}});
        let v: Value = post(addr, Some("tok"), Some(&sid), call.clone()).send().await.unwrap().json().await.unwrap();
        assert_eq!(v["result"]["content"][0]["text"], "hello cursor");

        assert_eq!(post(addr, Some("tok"), Some("nope"), call.clone()).send().await.unwrap().status(), 404);
        assert_eq!(post(addr, Some("tok"), None, call).send().await.unwrap().status(), 400);
    }

    #[tokio::test]
    async fn rejects_bad_token_origin_and_paths() {
        let (addr, _stop, _) = start().await;
        assert_eq!(post(addr, None, None, init()).send().await.unwrap().status(), 401);
        assert_eq!(post(addr, Some("tok2"), None, init()).send().await.unwrap().status(), 401);
        let evil = post(addr, Some("tok"), None, init()).header("origin", "https://evil.example").send().await.unwrap();
        assert_eq!(evil.status(), 403);
        let fine = post(addr, Some("tok"), None, init()).header("origin", "http://localhost:3000").send().await.unwrap();
        assert_eq!(fine.status(), 200);
        let rebinding = post(addr, Some("tok"), None, init()).header("host", "evil.example").send().await.unwrap();
        assert_eq!(rebinding.status(), 403);
        let wrong = reqwest::Client::new().post(format!("http://{addr}/other")).bearer_auth("tok").send().await.unwrap();
        assert_eq!(wrong.status(), 404);
        let get = reqwest::Client::new().get(format!("http://{addr}/mcp")).bearer_auth("tok").send().await.unwrap();
        assert_eq!(get.status(), 405);
        let bad = reqwest::Client::new()
            .post(format!("http://{addr}/mcp"))
            .bearer_auth("tok")
            .body("{oops")
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);
    }

    #[test]
    fn helpers() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
        assert!(loopback_host("127.0.0.1:7441", 7441));
        assert!(loopback_host("localhost", 7441));
        assert!(!loopback_host("127.0.0.1:80", 7441));
        assert!(loopback_origin("http://127.0.0.1:9"));
        assert!(!loopback_origin("http://example.com"));
        assert!(!loopback_origin("null"));
    }
}
