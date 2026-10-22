# valken-tools

Valken's agent tool layer, open source (Apache-2.0). One tool registry serves
two consumers: Valken's in-app agent pane (bring your own model) and the MCP
server for external agents (Claude Code, Cursor, …). Everything that keeps
those agents honest lives here, in front of every tool, so no client can go
around it:

- **Redaction** before anything leaves the process: env var values, JWTs,
  bearer tokens, AWS keys, PEM blocks, `password=` pairs, optionally IPs.
  Secrets are not a rule: no tool can read a Secret at all.
- **Tiers and access**: watch mode only reads; PRO tools are hidden and
  refused without the entitlement; approving or applying is not a tool.
- **An append-only audit log** (JSONL, one file per day): every call, with
  its actor.

| Module | What it does |
|---|---|
| `tool` | The typed `Tool` trait (input schema derived from the Rust type) and the `Registry` choke point |
| `redact` | Masking rules, text and object-aware |
| `audit` | Append-only JSONL log: write, read newest-first with filters, export |
| `model` | Streaming clients for the Anthropic Messages API and OpenAI-compatible endpoints (Ollama, LM Studio, hosted); request preview; "test connection & tools" |
| `agent` | The tool-calling loop (parallel tool calls, step limit, valid history after errors), hourly budget, prompts, answer markup (citations, evidence, next steps) |
| `mcp` | MCP over newline-delimited JSON-RPC (a local socket the `valken-mcp` relay bridges to stdio) and Streamable HTTP on loopback with a bearer token |
| `k8s` | Read tools over valken-core: `list_clusters`, `get_object`, `list_objects`, `get_events`, `get_logs`, `get_rollout_history` |

The application supplies cluster access (`k8s::Clusters`), client approval
(`mcp::Gate`) and its own tools on top.

## valken-mcp

`valken-mcp --stdio` relays an MCP client's stdio to the running Valken app.
All policy (approval, redaction, audit) stays in the app; with Valken closed
it answers with an error saying so. Add it to Claude Code with:

```sh
claude mcp add valken -- /path/to/valken-mcp --stdio
```

`VALKEN_MCP_SOCKET` overrides the socket path (default
`$XDG_RUNTIME_DIR/valken/mcp.sock`; a named pipe on Windows).

## Testing

```sh
cargo test                       # unit + integration (mock model server, relay binary)
VALKEN_TEST_KUBECONFIG=… cargo test --test k8s_live -- --ignored   # read tools on a real cluster
```

During development valken-core is the sibling checkout (`[patch]` in
`Cargo.toml`); remove it once the core tag is published.
