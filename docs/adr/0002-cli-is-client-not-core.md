# ADR 0002: CLI Is Client, Not Core

Status: Accepted

## Context

The current shipped interface is CLI, but the target product is a local desktop agent. If CLI remains the implicit core, desktop work will duplicate runtime assembly, lifecycle, approval, trace, and state recovery.

## Decision

CLI is a client of the runtime, not the core.

The core runtime boundary is:

```text
RuntimeHost -> KernelBootstrap -> AgentKernel -> RuntimeGraph -> SingularityAgent
```

CLI may:

- parse flags into `ProductionRuntimeConfig`
- pass a user goal
- render progress, approvals, traces, and final report
- return process exit codes

CLI must not:

- construct private runtime loops
- execute tools directly
- own policy decisions
- mutate runtime stores directly

## Consequences

- Desktop can become another client over RuntimeHost.
- Existing CLI commands remain useful for tests and automation.
- Runtime behavior can be preserved while UI changes.
