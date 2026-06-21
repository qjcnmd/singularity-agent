# ADR 0004: Python As Plugin Runtime

Status: Accepted

## Context

MiniHarness already has a Python plugin runtime. Python is also the current main runtime. The desktop migration should avoid mixing plugin compatibility with core process ownership.

## Decision

Python remains the plugin/runtime compatibility layer during the desktop migration.

Plugin contracts:

- manifest-first discovery
- disabled by default
- explicit local enablement
- host-controlled registration
- plugin tools become `ToolSpec` entries
- execution still flows through `ToolRuntime`, `PolicyRuntime`, `ApprovalGate`, `CommandRuntime`, `SandboxRuntime`, and trace

Plugins do not receive core runtime objects.

## Consequences

- Existing Python tool ecosystems remain usable.
- Future Rust Core can host Python plugins through a broker/process boundary.
- Dependency installation remains explicit and reviewed.
- High-risk plugin behavior remains policy-gated.
