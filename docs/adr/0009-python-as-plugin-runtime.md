# ADR 0009: Python As Plugin Runtime

Status: Accepted

## Context

Python is the current implementation language and it already works. It is also the natural extension language for local tools, model/provider adapters, evaluation hooks, and project-specific automation.

Keeping Python as the long-term product core would make desktop process ownership, native lifecycle, local IPC, event fanout, and UI-independent supervision harder to evolve.

## Decision

Python becomes the Plugin Runtime during the Singularity desktop transition.

Python keeps:

- current runtime behavior until replaced intentionally
- plugin manifests and host API
- local tool/provider/eval extension points

Python does not become:

- the permanent desktop process core
- the UI integration boundary
- a bypass around ToolRuntime, PolicyRuntime, ApprovalGate, CommandRuntime, SandboxRuntime, or TraceRuntime

## Consequences

- Existing Python CLI runtime remains safe to use.
- Future Rust Core can host Python plugins through a process or broker boundary.
- Plugin tools must flow through Tool Broker and runtime policy.
- Plugin imports and host APIs must stay behind runtime policy boundaries.
