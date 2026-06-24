# Singularity AgentHost Transition

AgentHost is the core boundary for Singularity. The first Python facade exists in `src/singularity/agent_host/`; the local daemon, HTTP/WebSocket/JSON-RPC transport, Rust Core, and Tauri shell are still future work.

## Purpose

AgentHost prevents the CLI from remaining the accidental product core. It exposes product actions while keeping component objects private:

- start run
- resume run
- cancel run
- submit local approval decision
- list state snapshot
- read artifact by ref
- subscribe to run events
- report health and diagnostics

It does not expose `ToolRegistry`, `PolicyEngine`, `ContextManager`, trace stores, protocol stores, or raw Python objects to UI code or plugins.

## Current Implementation

The current in-process facade exposes:

- `AgentHost.start_run(...)`
- `AgentHost.resume_run(...)`
- `AgentHost.cancel_run(...)`
- `AgentHost.submit_approval(...)`
- `AgentHost.snapshot(...)`
- `AgentHost.events(...)`
- `AgentHost.read_artifact(...)`

It projects `TraceEvent` into `RunEvent` with per-run sequence numbers, registers `ApprovalGrant` through the active `ApprovalGate`, and reads artifacts only by opaque artifact ref. It does not implement daemon transport or make CLI use AgentHost yet.

## Current To Target Boundary

Current CLI path:

```text
singularity-agent / sg
-> KernelBootstrap
-> AgentKernel
-> AgentGraph
-> AgentLoop
```

Current AgentHost facade path:

```text
AgentHost
-> KernelBootstrap
-> AgentKernel
-> AgentGraph
-> AgentLoop
```

The first implementation should be a thin host facade over the existing Python graph. It should not rewrite planner, context, model, tool protocol, policy, mutation, command, verification, trace, memory, or plugin behavior.

## Why CLI Becomes A Client

CLI should parse flags, collect user intent, render progress, and return exit codes. If CLI owns component assembly forever, desktop work will duplicate lifecycle, approvals, event streaming, crash recovery, and trace behavior.

Demoting CLI to client means `singularity-agent`, `sg`, and the future desktop client call the same host contract.

## AgentHost Responsibilities

AgentHost owns:

- agent graph lifecycle
- run identity and idempotency keys
- cancellation and shutdown
- local approval submission handoff
- state snapshots built from component stores
- event streaming from trace/protocol/context/workspace state
- artifact reads through opaque refs
- health and recovery reports

AgentHost must fail closed when policy, approval, protocol, trace, command, verification, or sandbox dependencies are missing.

## Tool Broker And MCP Boundary

AgentHost is not a tool execution shortcut. MCP and plugin tools must enter through Tool Broker:

```text
MCP adapter / plugin declaration
-> Tool Broker
-> ToolRegistry
-> ToolProtocolEngine
-> ToolExecutor
-> PolicyEngine / ApprovalGate
-> owning component
-> TraceRecorder
```

This keeps schema validation, side-effect metadata, approval, replay, and trace consistent for local tools, plugins, and MCP servers.

## Desktop Transition Readiness

The transition is ready for desktop only when:

- CLI and tests can use AgentHost without behavior drift
- approvals round-trip through AgentHost
- run events have stable sequence ids
- artifacts are readable by ref
- `singularity-agent` and `sg` clients share the same path

Until then, Tauri and Rust work stays out of scope.
