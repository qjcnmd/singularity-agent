# ADR 0004: Python As Plugin Management

Status: Accepted

## Context

Singularity already has a Python plugin component. Python is also the current main component. The desktop migration should avoid mixing plugin compatibility with core process ownership.

## Decision

Python remains the plugin/component compatibility layer during the desktop migration.

Plugin contracts:

- manifest-first discovery
- disabled by default
- explicit local enablement
- host-controlled registration
- plugin tools become `ToolSpec` entries
- execution still flows through `ToolExecutor`, `PolicyEngine`, `ApprovalGate`, `CommandExecutor`, `SandboxManager`, and trace

Plugins do not receive core component objects.

## Consequences

- Existing Python tool ecosystems remain usable.
- Future Rust Core can host Python plugins through a broker/process boundary.
- Dependency installation remains explicit and reviewed.
- High-risk plugin behavior remains policy-gated.
