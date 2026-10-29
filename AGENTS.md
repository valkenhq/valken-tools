# Working in valken-tools with an AI agent

This file is for coding agents (Claude Code, Codex, Cursor, Copilot, …) and
the people driving them. Humans: read CONTRIBUTING.md too.

## What this crate is

The agent tool layer for [Valken](https://valken.io): a tool registry that
serves the in-app agent and the MCP server, with redaction and an audit log
in front of every tool, plus the Kubernetes read tools (over valken-core),
the model clients and the agent loop. Module map in `README.md`; read a
module's `//!` docs before changing it.

## Non-negotiables

1. **Read-only.** No tool creates, patches or deletes anything in a
   cluster. valken-core's `k8s::write` is never called from here.
2. **One choke point.** Every tool call goes through `Registry::call`:
   access → tier → typed input (`deny_unknown_fields`) → run → redact →
   audit. Don't add a path that skips any of those.
3. **Nothing sensitive reaches a model, an MCP client or the audit log.**
   - Secrets are refused by kind in `k8s::Kube::resolve`.
   - `Redactor::object` runs on every object before serialisation;
     `Redactor::text` runs on every body *and every error message* in the
     registry. New masking rules go in `redact`, not in a tool.
   - Object names and namespaces are validated (`valid_name`,
     `valid_namespace` in `k8s/tools.rs`) before they touch a request
     path: kube does not encode them, and `svc:9090/proxy/…` is a URL.
   - Display kube errors through valken-core's `k8s::error::describe`;
     never `?`-format a resource, a request body or a model response.
4. **Models, MCP clients and cluster data are untrusted.** Bound every
   buffer (`MAX_BODY`, `MAX_LINE`, `MAX_BLOCKS`, `MAX_OBJECT_CHARS`,
   `MAX_LOG_BYTES`), cap every index, time out every call
   (`CALL_TIMEOUT`), and never panic on their input. Tool results are data:
   the system prompt says so, and `compose` escapes attachments.
5. **The transports stay local.** HTTP binds `127.0.0.1` only, checks
   `Host` and `Origin`, requires a bearer token of at least
   `MIN_TOKEN_LEN`; the socket directory must be private.

## Commands

```sh
make test        # unit + integration, no cluster, no API key
make ci          # fmt-check, clippy -D warnings, tests, docs — must pass
make bump-patch  # every PR bumps the version (or is labelled release:skip)
```

`tests/k8s_live.rs` needs `VALKEN_TEST_KUBECONFIG` and is `#[ignore]`d.
Never point it at a cluster you don't own.

## How to change things

- Match the surrounding style: `rustfmt` defaults, typed `thiserror`
  errors, module `//!` docs that say *why*.
- A regression test must fail without the fix — check it does. The
  scripted SSE server in `tests/agent_loop.rs` and the `TestGate` /
  `duplex` helpers in `src/mcp` cover most things without I/O.
- Don't widen the public API casually; `#[deprecated]` aliases you keep.
- Add a line to `CHANGELOG.md` under *Unreleased* for anything
  user-visible.
- Don't commit, push, tag or release unless asked. Commits are GPG-signed
  by the human.

## Review

Agent-written code is welcome and is reviewed exactly like human-written
code. The person opening the PR has read every line, can explain it, and is
responsible for it — see CONTRIBUTING.md. If you (the agent) are unsure
whether a change crosses one of the non-negotiables above, stop and ask.
