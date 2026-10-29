# Changelog

Notable changes, newest first. Versions are git tags; see *Contributing
and releases* in the README.

## 0.1.0

Initial public release: the tool registry (access, tier, typed input,
redaction, audit), the read tools over valken-core, the Anthropic and
OpenAI-compatible streaming clients, the agent loop, and the MCP server over
a local socket (`valken-mcp --stdio`) and loopback HTTP.

What a model never gets: Secret values, env values, credential flags on
command lines, ConfigMap values (by default), spec-copy annotations, and
names that would address anything but the named object — see `SECURITY.md`
and `AGENTS.md`.

Requires Rust 1.89 (`rust-version`): `uuid` 1.29 and `enum-ordinalize` 4.4.2
(via `kube-runtime` 3.1) no longer build on 1.88.
