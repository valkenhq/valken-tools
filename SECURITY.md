# Security policy

valken-tools sits between language models (the in-app agent, external MCP
clients) and the user's Kubernetes clusters. A bug here can hand cluster
data to a model that shouldn't see it, or let a model do more than read.
Please report anything in that category privately.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting: **Security → Report a
vulnerability** on this repository. Do not open a public issue.

Include what you found, how to reproduce it, and what an attacker (a
prompt-injected model, a malicious MCP client, another local user) could do
with it. You'll get an acknowledgement within a few days and a fix or a
plan before anything is disclosed.

## What counts

* A tool returning Secret values, env values, tokens, ConfigMap values or
  anything else the redaction rules promise to mask — to a model, an MCP
  client, the audit log or a log line.
* A read tool reaching anything but a read: the `proxy` subresource, a
  write verb, a different cluster than the one named.
* Bypassing the registry choke point: tier gating, access checks, approval
  of MCP clients, the audit line.
* The MCP transports accepting a request from a browser page, another host,
  or another user on the machine.

## Supported versions

Only the latest release is supported. Fixes are released as a new version
and noted in [CHANGELOG.md](CHANGELOG.md).
