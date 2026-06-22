# ADR 0001: Local-First Agent

Status: Accepted

## Context

Singularity is a production-oriented local coding agent runtime. It operates on a user's workspace, may execute commands, may mutate files, and stores trace, context, protocol, policy, workspace state, and memory locally.

Future desktop work should not turn the product into a remote-control cloud agent by accident.

## Decision

Singularity is local-first:

- runtime state is local by default
- approvals are local by default
- trace, audit, workspace state, and memory are local stores
- remote model providers are allowed, but only through `ModelRuntime`
- no remote telemetry backend is required
- no remote memory sync is part of v0.1.x

## Consequences

- Desktop Transition Runtime should start as a local RuntimeHost/daemon.
- Local state must be resumable and recoverable.
- Secret handling and redaction happen before local persistence and before provider calls.
- Remote collaboration, remote approval, and shared memory require separate ADRs.
