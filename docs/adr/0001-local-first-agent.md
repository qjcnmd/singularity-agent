# ADR 0001: Local-First Agent

Status: Accepted

## Context

Singularity is a production-oriented local coding agent component. It operates on a user's workspace, may execute commands, may mutate files, and stores trace, context, protocol, policy, workspace state, and memory locally.

Future desktop work should not turn the product into a remote-control cloud agent by accident.

## Decision

Singularity is local-first:

- session state is local by default
- approvals are local by default
- trace, audit, workspace state, and memory are local stores
- remote model providers are allowed, but only through `ModelRunner`
- no remote telemetry backend is required
- file-backed remote approval and memory sync may exist as explicit operator-mediated JSON exchanges
- no remote approval server, background memory sync daemon, or remote telemetry backend is required

## Consequences

- Desktop Transition AgentHost should start as a local AgentHost/daemon.
- Local state must be resumable and recoverable.
- Secret handling and redaction happen before local persistence and before provider calls.
- Remote collaboration, networked approval services, and shared-memory servers require separate ADRs.
