//! The app's local MCP endpoint, which `valken-mcp --stdio` relays to: a
//! Unix socket (owner-only) on Linux/macOS, a named pipe on Windows.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};

use super::{serve_lines, Server, Transport};

/// Where the running app listens.  `VALKEN_MCP_SOCKET` overrides it (tests,
/// several instances).
pub fn endpoint() -> PathBuf {
    if let Some(p) = std::env::var_os("VALKEN_MCP_SOCKET") {
        return PathBuf::from(p);
    }
    default_endpoint()
}

#[cfg(target_os = "linux")]
fn default_endpoint() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("valken-{}", uid())));
    base.join("valken").join("mcp.sock")
}

#[cfg(target_os = "macos")]
fn default_endpoint() -> PathBuf {
    // $TMPDIR is per-user on macOS; Application Support paths can exceed sun_path.
    std::env::temp_dir().join("valken-mcp.sock")
}

#[cfg(windows)]
fn default_endpoint() -> PathBuf {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    PathBuf::from(format!(r"\\.\pipe\valken-mcp-{user}"))
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn default_endpoint() -> PathBuf {
    std::env::temp_dir().join(format!("valken-{}", uid())).join("mcp.sock")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and can't fail.
    unsafe { libc::getuid() }
}

/// Accepts relay connections until `shutdown` resolves.  Each connection is
/// one MCP session over newline-delimited JSON.
#[cfg(unix)]
pub async fn listen(server: Arc<Server>, shutdown: impl std::future::Future<Output = ()>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    let path = endpoint();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    // A socket left by a crashed instance: only remove it if nobody answers.
    if path.exists() {
        if tokio::net::UnixStream::connect(&path).await.is_ok() {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, "another Valken is serving MCP"));
        }
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(path = %path.display(), "mcp: local socket listening");
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            _ = &mut shutdown => break Ok(()),
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => spawn_session(server.clone(), stream),
                Err(e) => break Err(e),
            },
        }
    };
    let _ = std::fs::remove_file(&path);
    result
}

#[cfg(windows)]
pub async fn listen(server: Arc<Server>, shutdown: impl std::future::Future<Output = ()>) -> io::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let name = endpoint();
    let mut pipe = ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true).create(&name)?;
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            connected = pipe.connect() => {
                connected?;
                let next = ServerOptions::new().reject_remote_clients(true).create(&name)?;
                spawn_session(server.clone(), std::mem::replace(&mut pipe, next));
            }
        }
    }
}

fn spawn_session<S: AsyncRead + AsyncWrite + Send + 'static>(server: Arc<Server>, stream: S) {
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(stream);
        if let Err(e) = serve_lines(server, Transport::Stdio, r, w).await {
            tracing::debug!(error = %e, "mcp: relay session ended");
        }
    });
}

/// Connects to the running app (the relay side).
#[cfg(unix)]
pub async fn connect() -> io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(endpoint()).await
}

#[cfg(windows)]
pub async fn connect() -> io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    tokio::net::windows::named_pipe::ClientOptions::new().open(endpoint())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::mcp::tests::{server, TestGate};
    use serde_json::Value;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn socket_serves_sessions_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v").join("mcp.sock");
        // Safe: this test is the only user of the variable in this binary.
        unsafe { std::env::set_var("VALKEN_MCP_SOCKET", &path) };
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(listen(server(Default::default()), async move {
            let _ = stop_rx.await;
        }));
        for _ in 0..50 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

        let stream = connect().await.unwrap();
        let (r, mut w) = tokio::io::split(stream);
        w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n").await.unwrap();
        let line = BufReader::new(r).lines().next_line().await.unwrap().unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["result"], serde_json::json!({}));

        // A second listener refuses while the first is alive.
        let second = listen(server(std::sync::Arc::new(TestGate::default())), std::future::pending()).await;
        assert_eq!(second.unwrap_err().kind(), io::ErrorKind::AddrInUse);

        stop_tx.send(()).unwrap();
        task.await.unwrap().unwrap();
        assert!(!path.exists());
        unsafe { std::env::remove_var("VALKEN_MCP_SOCKET") };
    }
}
