# ADR 0008: AgentHost As Product Core Boundary

Status: Accepted

## Context

The current shipped interface is CLI, but Singularity's target product needs CLI, desktop UI, tests, replay tools, and future local daemon behavior to share one execution contract.

If CLI remains the core, desktop work will duplicate agent graph assembly, lifecycle, approval, event streaming, artifact access, crash recovery, and trace handling.

## Decision

AgentHost is the future product core boundary.

AgentHost exposes:

- start, resume, and cancel run
- submit local approval decision
- state snapshot
- artifact read by ref
- run event subscription
- health and diagnostics

AgentHost wraps the existing Python component first:

```text
AgentHost -> KernelBootstrap -> AgentKernel -> AgentGraph -> AgentLoop
```

CLI is a client. Tauri/TypeScript is a client. Neither receives internal component objects.

## Consequences

- `singularity-agent` and `sg` share one client path.
- Policy, tool execution, protocol, trace, and workspace state stay behind execution boundaries.
- MCP and plugins still enter through Tool Broker and ToolExecutor.
- AgentHost must fail closed when core dependencies are missing.
