# ADR 0003: Rust Core And Tauri Desktop

Status: Accepted

## Context

The target architecture is Rust Core + Tauri Desktop + TypeScript UI + Python Plugin Management. The current component is Python and already owns planner, context, model, tool protocol, policy, mutation, command, verification, trace, memory, plugin, and evaluation behavior.

Rewriting the component before a stable host contract exists would risk behavior drift.

## Decision

Use staged migration:

1. Preserve Python component as the v0.1.x CLI baseline.
2. Add Desktop Transition AgentHost as a AgentHost/local daemon contract.
3. Build Tauri/TypeScript as clients of that contract.
4. Move stable process/control-plane pieces to Rust only after the host API proves stable.

Rust Core is a target, not part of this Documentation Component implementation.

## Consequences

- No Electron is introduced.
- No Python component deletion happens in this phase.
- Rust candidates are local daemon shell, IPC, event fanout, artifact serving, locking, process supervision wrappers, and config loading.
- Planner/model/tool behavior remains Python until intentionally ported.
