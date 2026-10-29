<p align="center">
  <a href="https://valken.io"><img src=".github/valken-badge-transparent.png" alt="Valken" width="420"></a>
</p>

<h1 align="center">valken-tools</h1>

<p align="center">
  The agent tool layer behind <a href="https://valken.io">Valken</a>.
</p>

[Valken](https://valken.io) is a desktop app for people who run
Kubernetes. It connects to your clusters with your own kubeconfig and shows
what is actually running as a live map — workloads and how they connect,
ingress traffic, events and config changes, and a security lens — so you
can see a problem and act on it from one place. Its agent pane lets you ask
a model of your choosing about what you're looking at, and its MCP server
lets external agents (Claude Code, Cursor, …) see the same thing.

`valken-tools` is the part that keeps those agents honest. One tool
registry serves both consumers, and everything that constrains them lives
in front of every tool, so no client can go around it:

- **Read-only.** Every tool reads. Proposing a change is a tool; approving
  or applying one is not — that happens in Valken, by a person.
- **Redaction** before anything leaves the process: env values, credential
  flags, ConfigMap values, tokens, keys, PEM blocks, `password=` pairs,
  optionally IPs. Secrets can't be read at all. Object names are validated
  so a tool can only ever address the object it names.
- **Tiers and approval.** PRO tools are hidden without the entitlement;
  MCP clients are approved by name in Valken before their first call.
- **An append-only audit log** (JSONL, one file per day, `0600`): every
  call, with its actor.

| Module | Does |
|---|---|
| `tool` | The typed `Tool` trait (input schema derived from the Rust type) and the `Registry` choke point. |
| `redact` | Masking rules, text and object-aware. |
| `audit` | Append-only JSONL log: write, read newest-first with filters, export. |
| `model` | Streaming clients for the Anthropic Messages API and OpenAI-compatible endpoints (Ollama, LM Studio, hosted). |
| `agent` | The tool-calling loop (bounded parallel calls, step limit, history valid after errors and cancellation), hourly budget, prompts, answer markup. |
| `mcp` | MCP over newline-delimited JSON-RPC on a local socket (relayed by `valken-mcp --stdio`) and Streamable HTTP on loopback with a bearer token. |
| `k8s` | Read tools over [valken-core](https://github.com/valkenhq/valken-core): `list_clusters`, `get_object`, `list_objects`, `get_events`, `get_logs`, `get_rollout_history`. |

The application supplies cluster access (`k8s::Clusters`), client approval
(`mcp::Gate`) and its own tools on top.

## Use it

```toml
[dependencies]
valken-tools = { git = "https://github.com/valkenhq/valken-tools" }
```

Register tools once and serve them to both consumers:

```rust
use std::sync::Arc;
use valken_tools::{audit::AuditLog, k8s, redact::Redactor, tool::Registry};

let audit = Arc::new(AuditLog::open("~/.local/state/valken/audit")?);
let mut registry = Registry::new(Redactor::default(), Some(audit));
k8s::register(&mut registry, &k8s::Kube::new(clusters)); // your `Clusters` impl
let registry = Arc::new(registry);

// In-app agent: valken_tools::agent::Agent { registry, client, budget }.run(..)
// MCP:          valken_tools::mcp::Server::new(registry, gate, version)
```

### valken-mcp

`valken-mcp --stdio` relays an MCP client's stdio to the running Valken app.
All policy (approval, redaction, audit) stays in the app; with Valken closed
it answers with an error saying so. Add it to Claude Code with:

```sh
claude mcp add valken -- /path/to/valken-mcp --stdio
```

`VALKEN_MCP_SOCKET` overrides the socket path (default
`$XDG_RUNTIME_DIR/valken/mcp.sock`; a named pipe on Windows).

## Build and test

```sh
make test   # unit + integration: a scripted model server, the relay binary, no cluster
make ci     # what CI runs: fmt, clippy -D warnings, tests, docs
```

The read tools have one live test, `#[ignore]`d and only run against a
cluster you point it at; it never reads `KUBECONFIG` or `~/.kube/config`:

```sh
VALKEN_TEST_KUBECONFIG=~/.kube/kind.yaml cargo test --test k8s_live -- --ignored
```

## Contributing and releases

See [CONTRIBUTING.md](CONTRIBUTING.md). Every PR bumps the version
(`make bump-patch` / `bump-minor` / `bump-major`); merging tags and
releases it. [CHANGELOG.md](CHANGELOG.md) has what changed;
[SECURITY.md](SECURITY.md) says how to report a vulnerability.

Licensed under [Apache-2.0](LICENSE).
