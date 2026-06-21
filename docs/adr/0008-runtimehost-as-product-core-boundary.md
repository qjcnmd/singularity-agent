# ADR 0008: RuntimeHost As Product Core Boundary

Status: Accepted

## Context

The current shipped interface is CLI, but Singularity's target product needs CLI, desktop UI, tests, replay tools, and future local daemon behavior to share one runtime contract.

If CLI remains the core, desktop work will duplicate runtime graph assembly, lifecycle, approval, event streaming, artifact access, crash recovery, and trace handling.

## Decision

RuntimeHost is the future product core boundary.

RuntimeHost exposes:

- start, resume, and cancel run
- submit local approval decision
- state snapshot
- artifact read by ref
- run event subscription
- health and diagnostics

RuntimeHost wraps the existing Python runtime first:

```text
RuntimeHost -> KernelBootstrap -> AgentKernel -> RuntimeGraph -> SingularityAgent
```

CLI is a client. Tauri/TypeScript is a client. Neither receives internal runtime objects.

## Consequences

- `singularity-agent` and `sg` share one client path.
- Policy, tool execution, protocol, trace, and workspace state stay behind runtime boundaries.
- MCP and plugins still enter through Tool Broker and ToolRuntime.
- RuntimeHost must fail closed when core dependencies are missing.
