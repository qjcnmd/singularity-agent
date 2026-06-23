# Boundary Contracts

This document defines the contracts the current Python RuntimeHost facade, a future local daemon, Tauri desktop shell, and TypeScript UI must obey while the production runtime is still Python.

## Core Rule

Clients request, runtimes decide and execute, stores persist, trace explains. No layer may skip the owner of a side effect.

## Client Boundary

Current client:

- `CLI`

Future clients:

- CLI through RuntimeHost
- Tauri desktop shell
- TypeScript UI
- local daemon control API
- tests and replay tools

Client responsibilities:

- collect user intent and local decisions
- render progress, approvals, traces, artifacts, and final report
- send commands to RuntimeHost with `run_id`, `session_id`, `task_id`, and idempotency key when applicable
- subscribe to run events

Client forbidden behavior:

- execute tools directly
- write runtime state files directly
- grant approval without a `PolicyDecision` that requires review
- parse raw trace files as the only integration contract
- treat model text as policy or approval

## SingularityAgent Boundary

`SingularityAgent` is an orchestration adapter inside the runtime host. It may call:

- `planner.step()`
- `context.build_bundle()`
- `model_runtime.run_turn()`
- `ToolCallingProtocolRuntime.process_model_turn()`
- final report production

It must not:

- call tool handlers directly
- construct protocol result messages by hand
- decide policy outcomes
- write trace/audit records with raw payloads
- own persistence for protocol, context, or workspace state

## Runtime Boundary

Every runtime must expose:

- stable name
- input model or request shape
- result shape
- trace events
- policy request, when the action may read secrets, mutate, execute, access network, or consume user approval
- recovery behavior
- failure mode

Every runtime must fail closed when its owning dependency is missing. Example: command execution cannot fall back to `subprocess` when `CommandRuntime` or required sandbox capability is unavailable.

## Tool Boundary

`ToolRegistry` exposes tool declarations. `ToolRuntime` executes registered handlers only after schema validation, policy, approval, planner authorization, dry-run checks, and backend contract checks pass.

Tool contracts must declare:

- name and version
- input schema
- permission level
- operation and capabilities
- side-effect kind
- execution backend
- idempotency and cache policy
- artifact policy
- whether it delegates to mutation, edit, command, or verification runtime

Forbidden tool behavior:

- write files without `uses_mutation_runtime` or `uses_edit_runtime`
- spawn processes without `uses_command_runtime`
- run test/lint/build checks outside `VerificationRuntime`
- expose raw secret values in `ToolResult`, trace, context, or artifact metadata

## Policy Boundary

`PolicyRuntime` owns allow, deny, review, ask-user, escalate, and sandbox-required decisions. `ApprovalGate` only resolves local review prompts into scoped grants.

Policy inputs must include runtime, operation, capability, subject, resource, risk tags, reversibility, network/workspace/secret flags, and reason.

Policy outputs must be auditable and include a decision id. Any missing policy dependency is a blocking runtime error, not an implicit allow.

## Trace Boundary

`TraceRuntime` owns event, span, artifact, timeline, and summary persistence. It stores references and redacted summaries.

Trace must not be the command bus. Desktop should subscribe to RuntimeHost events generated from runtime trace/state, but should not mutate trace files or depend on private filesystem paths.

## Storage Boundary

Current storage is local-first and file/SQLite backed:

- trace run directory
- context SQLite store
- tool protocol SQLite store
- workspace state journal and artifacts
- policy audit JSONL
- local memory store

Storage contracts:

- append-only where audit or recovery depends on history
- opaque artifact refs instead of internal absolute paths
- schema version or migration point for durable state
- redaction before write
- recovery report when state is incomplete

## RuntimeHost Boundary

The current Python RuntimeHost is the in-process API above Python runtimes. It exposes:

- start/resume/cancel run
- submit local approval decision
- list state snapshot
- list/read artifact by ref
- subscribe to run events

The future local daemon should wrap the same contract and add health, diagnostics, HTTP, WebSocket, and JSON-RPC transport.

It must not expose raw internal objects such as `ToolRegistry`, `PolicyRuntime`, or `ContextManager` to the UI or plugins.
