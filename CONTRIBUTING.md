# Contributing

Thanks for taking a look. The short version: branch, change, `make ci`,
bump the version, open a PR.

## Setup

`rust-toolchain.toml` pins the toolchain; `rustup` picks it up on first
build. No cluster and no model API key are needed for development: the
tests run against a scripted model server and the real relay binary.

```sh
make test   # unit + integration
make ci     # what CI runs: fmt, clippy -D warnings, tests, docs
```

## Live test

`tests/k8s_live.rs` is `#[ignore]`d and needs a kubeconfig in
`VALKEN_TEST_KUBECONFIG`; it never reads `KUBECONFIG` or `~/.kube/config`
and only reads from the cluster. Point it at a throwaway cluster anyway.

## Rules of the crate

* **Read-only.** Tools read. If you add a tool that changes a cluster, it
  doesn't belong here: it belongs behind a human in Valken.
* **Everything goes through `Registry::call`.** Access, tier, typed input,
  redaction and the audit line happen there, in that order, for every
  consumer. Don't add a side door.
* **Nothing sensitive reaches a model.** Env values, credential flags,
  ConfigMap values, tokens, keys and the spec-copy annotations are masked;
  Secrets are refused by kind; names are validated before they touch a
  request path. A new tool returns the minimum a model needs.
* **Models and clusters are untrusted input.** Model output, MCP client
  messages and cluster data can all be hostile; bound every buffer and
  index, and never panic on them.
* **Tests with the change.** Pure logic gets a unit test next to it; the
  loop and the transports get a test over the scripted server or the
  in-memory duplex. A regression test must fail without the fix.

## AI agents

Using an AI coding agent (Claude Code, Codex, Cursor, Copilot, …) to write
or change code here is fine; `AGENTS.md` tells agents the rules. Two
conditions:

* **A human reviews everything.** Every line in a PR has been read and
  understood by the person opening it, and is reviewed by a maintainer like
  any other change. "The agent wrote it" is not an explanation in review.
* **The author carries the risk.** You are responsible for what you submit
  — correctness, licensing of anything it reproduced, and the security
  rules above — exactly as if you had typed it. If you can't vouch for a
  change, don't submit it.

Say in the PR that an agent was involved; it helps reviewers know where to
look harder.

## Pull requests

Every PR into `main` bumps `Cargo.toml` with `make bump-patch`,
`bump-minor` or `bump-major` (the "Version bumped" check enforces it); the
merge is tagged with that version. Label the PR `release:skip` for changes
that shouldn't release (docs, CI). Add a line to `CHANGELOG.md` under
*Unreleased*.
