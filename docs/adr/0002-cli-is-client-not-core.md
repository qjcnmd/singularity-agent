# ADR 0002: CLI Is Client, Not Core

Status: Accepted

## Context

The current shipped interface is CLI, but the target product is a local desktop agent. If CLI remains the implicit core, desktop work will duplicate component assembly, lifecycle, approval, trace, and state recovery.

## Decision

CLI is a client of the component, not the core.

The core execution boundary is:

```text
AgentHost -> KernelBootstrap -> AgentKernel -> AgentGraph -> AgentLoop
```

CLI may:

- parse flags into `ProductionConfig`
- pass a user goal
- render progress, approvals, traces, and final report
- return process exit codes

CLI must not:

- construct private component loops
- execute tools directly
- own policy decisions
- mutate component stores directly

## Consequences

- Desktop can become another client over AgentHost.
- Existing CLI commands remain useful for tests and automation.
- Component behavior can be preserved while UI changes.
