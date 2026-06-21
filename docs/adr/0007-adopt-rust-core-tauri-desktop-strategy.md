# ADR 0007: Adopt Rust Core And Tauri Desktop Strategy

Status: Accepted

## Context

Singularity is moving from a Python CLI baseline toward a local desktop product. The runtime already has meaningful Python behavior around planning, context, model calls, tool protocol, policy, mutation, command, verification, sandbox, trace, memory, plugin loading, and evaluation.

A direct rewrite would risk behavior drift. A pure CLI path would delay product work. A pure web path would break the local-first safety model.

## Decision

Adopt Rust Core + Tauri Desktop + TypeScript UI + Python Plugin Runtime as the target architecture.

The implementation order is staged:

1. preserve the Python runtime
2. introduce RuntimeHost around the Python runtime
3. make CLI and future desktop clients use RuntimeHost
4. build Tauri/TypeScript as clients
5. move stable host/control-plane pieces into Rust only after contracts prove stable

Rejected alternatives:

- pure Python GUI: too much runtime/UI/process coupling
- Electron: too heavy for a local-first runtime product
- Go Core: less aligned with Tauri and sensitive native control-plane ownership
- Java/Spring: server-style weight does not fit local desktop runtime
- all Rust UI: slower iteration for dense product views
- pure Web SaaS: conflicts with local workspace, approval, trace, and artifact boundaries

## Consequences

- No Tauri or Rust workspace is introduced by this ADR.
- RuntimeHost is the next implementation boundary.
- Python remains the compatibility and plugin runtime.
- Desktop UI must be a client, not a runtime owner.
