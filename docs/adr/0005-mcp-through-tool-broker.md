# ADR 0005: MCP Through Tool Broker

Status: Accepted

## Context

Future desktop builds may need MCP servers and external tools. Directly wiring MCP into UI or model messages would bypass existing policy, approval, trace, replay, and tool protocol guarantees.

## Decision

MCP access must flow through a tool broker boundary.

In the current Python baseline, the broker is:

```text
PluginRuntime / MCP adapter
-> ToolRegistry
-> ToolCallingProtocolRuntime
-> ToolRuntime
-> PolicyRuntime / ApprovalGate
-> owning execution runtime
-> TraceRuntime
```

MCP tools are exposed as normal tool declarations with schemas, capabilities, side-effect metadata, idempotency, and backend contracts.

## Consequences

- MCP does not bypass local policy.
- Desktop UI does not call MCP servers directly for agent actions.
- Replay, resume, pending approval, and trace behavior remain consistent.
- High-risk MCP tools must declare the same backend delegation as local tools.
