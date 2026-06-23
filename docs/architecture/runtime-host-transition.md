# Singularity RuntimeHost Transition

RuntimeHost is the core boundary for Singularity. The first Python facade exists in `src/singularity/runtime_host/`; the local daemon, HTTP/WebSocket/JSON-RPC transport, Rust Core, and Tauri shell are still future work.

## Purpose

RuntimeHost prevents the CLI from remaining the accidental product core. It exposes product actions while keeping runtime objects private:

- start run
- resume run
- cancel run
- submit local approval decision
- list state snapshot
- read artifact by ref
- subscribe to run events
- report health and diagnostics

It does not expose `ToolRegistry`, `PolicyRuntime`, `ContextManager`, trace stores, protocol stores, or raw Python objects to UI code or plugins.

## Current Implementation

The current in-process facade exposes:

- `RuntimeHost.start_run(...)`
- `RuntimeHost.resume_run(...)`
- `RuntimeHost.cancel_run(...)`
- `RuntimeHost.submit_approval(...)`
- `RuntimeHost.snapshot(...)`
- `RuntimeHost.events(...)`
- `RuntimeHost.read_artifact(...)`

It projects `TraceEvent` into `RunEvent` with per-run sequence numbers, registers `ApprovalGrant` through the active `PolicyRuntime`, and reads artifacts only by opaque artifact ref. It does not implement daemon transport or make CLI use RuntimeHost yet.

## Current To Target Boundary

Current CLI path:

```text
singularity-agent / sg
-> KernelBootstrap
-> AgentKernel
-> RuntimeGraph
-> SingularityAgent
```

Current RuntimeHost facade path:

```text
RuntimeHost
-> KernelBootstrap
-> AgentKernel
-> RuntimeGraph
-> SingularityAgent
```

The first implementation should be a thin host facade over the existing Python graph. It should not rewrite planner, context, model, tool protocol, policy, mutation, command, verification, trace, memory, or plugin behavior.

## Why CLI Becomes A Client

CLI should parse flags, collect user intent, render progress, and return exit codes. If CLI owns runtime assembly forever, desktop work will duplicate lifecycle, approvals, event streaming, crash recovery, and trace behavior.

Demoting CLI to client means `singularity-agent`, `sg`, and the future desktop client call the same host contract.

## RuntimeHost Responsibilities

RuntimeHost owns:

- runtime graph lifecycle
- run identity and idempotency keys
- cancellation and shutdown
- local approval submission handoff
- state snapshots built from runtime stores
- event streaming from trace/protocol/context/workspace state
- artifact reads through opaque refs
- health and recovery reports

RuntimeHost must fail closed when policy, approval, protocol, trace, command, verification, or sandbox dependencies are missing.

## Tool Broker And MCP Boundary

RuntimeHost is not a tool execution shortcut. MCP and plugin tools must enter through Tool Broker:

```text
MCP adapter / plugin declaration
-> Tool Broker
-> ToolRegistry
-> ToolCallingProtocolRuntime
-> ToolRuntime
-> PolicyRuntime / ApprovalGate
-> owning runtime
-> TraceRuntime
```

This keeps schema validation, side-effect metadata, approval, replay, and trace consistent for local tools, plugins, and MCP servers.

## Desktop Transition Readiness

The transition is ready for desktop only when:

- CLI and tests can use RuntimeHost without behavior drift
- approvals round-trip through RuntimeHost
- run events have stable sequence ids
- artifacts are readable by ref
- `singularity-agent` and `sg` clients share the same path

Until then, Tauri and Rust work stays out of scope.
