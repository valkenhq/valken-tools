//! `valken-mcp --stdio`: lets an MCP client that launches servers over stdio
//! (Claude Code, Cursor, …) talk to the running Valken app.  It relays bytes
//! between stdio and the app's local socket; all policy lives in the app.
//!
//! With Valken closed it answers the client's first request with an error
//! saying so, and exits: there is no headless mode.

use std::process::ExitCode;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const USAGE: &str = "usage: valken-mcp --stdio\n\nRelays MCP over stdio to the running Valken app.";
const NOT_RUNNING: &str = "Valken isn't running. Open Valken, then reconnect this MCP server.";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Accept `valken-mcp --stdio` and `valken-mcp mcp --stdio`.
    let args: Vec<String> = std::env::args().skip(1).filter(|a| a != "mcp").collect();
    match args.as_slice() {
        [flag] if flag == "--stdio" => {}
        [flag] if flag == "--version" => {
            println!("valken-mcp {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    }
    match valken_tools::mcp::local::connect().await {
        Ok(stream) => {
            let (mut app_r, mut app_w) = tokio::io::split(stream);
            let up = async {
                tokio::io::copy(&mut tokio::io::stdin(), &mut app_w).await?;
                app_w.shutdown().await
            };
            let down = async {
                let mut out = tokio::io::stdout();
                tokio::io::copy(&mut app_r, &mut out).await?;
                out.flush().await
            };
            // Either side closing ends the session.
            let result = tokio::select! { r = up => r, r = down => r };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("valken-mcp: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Err(_) => {
            eprintln!("valken-mcp: {NOT_RUNNING}");
            // Answer the pending request so the client shows why.
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            if let Ok(Some(line)) = lines.next_line().await {
                if let Some(reply) = not_running_reply(&line) {
                    let mut out = tokio::io::stdout();
                    let _ = out.write_all(format!("{reply}\n").as_bytes()).await;
                    let _ = out.flush().await;
                }
            }
            ExitCode::FAILURE
        }
    }
}

fn not_running_reply(line: &str) -> Option<serde_json::Value> {
    let msg: serde_json::Value = serde_json::from_str(line).ok()?;
    let id = msg.get("id")?.clone();
    Some(
        serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": NOT_RUNNING}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_only_to_requests() {
        let r = not_running_reply(r#"{"jsonrpc":"2.0","id":7,"method":"initialize"}"#).unwrap();
        assert_eq!(r["id"], 7);
        assert_eq!(r["error"]["message"], NOT_RUNNING);
        assert!(not_running_reply(r#"{"jsonrpc":"2.0","method":"notifications/x"}"#).is_none());
        assert!(not_running_reply("junk").is_none());
    }
}
