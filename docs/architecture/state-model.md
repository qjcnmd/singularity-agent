# State Model

This document defines the durable state vocabulary for current CLI runs and the future Desktop Transition Runtime.

## Identity

Every record that participates in a run should carry the smallest relevant identity set:

- `run_id`: one user-requested execution attempt.
- `session_id`: resumable local session scope.
- `task_id`: planner task scope.
- `phase_id`: planner/model/tool protocol phase.
- `action_id`: planner action or runtime action.

Desktop clients must treat these ids as opaque strings.

## Session

Current source: `AgentSession`, workspace-state session, context store, trace run directory.

States:

```text
created -> active -> closing -> closed
created|active -> failed
created|active -> cancelled
failed|cancelled -> recovered
```

Contract:

- one active writer holds the workspace lock unless the run is read-only
- resume must recover context, protocol state, workspace state, trace, and pending approval facts before requesting another model turn
- stale or corrupt state must produce a recovery report, not silent continuation

## Run

Current source: `AgentRun`, lifecycle events, final report.

States:

```text
created -> running -> completed
created|running -> failed
created|running -> cancelled
```

Contract:

- every run has one final outcome
- final report references verification, policy, trace, workspace, and recovery summaries
- cancellation still enters shutdown/finalization

## Message

Current source: context store and provider projection cache.

Kinds:

- system instruction
- user goal/message
- assistant message
- tool observation
- planner state
- policy observation
- mutation evidence
- command observation
- verification evidence
- workspace state
- project index
- memory context
- summary/reference

Contract:

- context items are authoritative
- provider messages are projections
- tool results enter context through protocol result envelopes
- secret content is redacted before storage and rendering

## Tool Call

Current source: `ToolCallEnvelope`, `ToolCallRecord`, `ToolProtocolResultEnvelope`, protocol SQLite.

Phases:

```text
proposed -> validated -> scheduled -> running -> succeeded -> result_appended
proposed -> rejected
validated -> waiting_approval -> approved -> scheduled
running -> failed|cancelled
any durable phase -> recovered
```

Contract:

- invalid calls produce synthetic protocol results
- side-effect replay is blocked unless the tool is explicitly idempotent and safe
- result binding must include digest, redaction flag, and artifact refs when output is not inline

## Approval

Current source: `PolicyDecision`, `ApprovalRequirement`, `ApprovalGrant`, policy audit.

States:

```text
requested -> granted -> consumed
requested -> denied
requested -> expired
requested -> unavailable
```

Contract:

- approvals are scoped, session-bound, and single-use by default
- non-interactive mode fails closed when review is needed
- model text is never an approval source

## Trace

Current source: `TraceEvent`, `TraceSpan`, `TraceArtifact`, trace store.

States:

```text
span: running -> success|failed|cancelled|timeout|skipped|blocked
event: appended
artifact: written -> referenced
```

Contract:

- trace is append-only for events
- spans must be repaired or marked incomplete during crash recovery
- payload hashes and artifact refs are preferred over raw payloads

## Artifact

Current sources: trace artifact store and workspace artifact store.

Kinds:

- stdout, stderr, command log
- diff, report, snapshot
- sandbox, verification, edit plan
- model message, prompt manifest
- policy audit ref, generic

Contract:

- artifact ids are opaque refs
- internal absolute paths are not a UI or API contract
- sensitive artifacts are redacted before write

## Memory

Current source: `MemoryEntry`, `MemoryCandidate`, local memory store.

States:

```text
candidate -> active
candidate -> rejected
active -> superseded|expired|tombstoned|quarantined
```

Contract:

- memory items require provenance
- candidate promotion is explicit
- stale or conflicted memory is not injected into context
- memory is local in v0.1.x

## Workspace State

Current source: `LocalWorkspaceStateRuntime`, workspace baseline, journal, artifacts.

States:

```text
clean -> dirty -> clean
clean|dirty -> conflicted
unknown -> recoverable|needs_user_review|corrupted
```

Ownership values:

- `USER_OWNED`
- `AGENT_MUTATION`
- `COMMAND_SIDE_EFFECT`
- `FORMATTER_SIDE_EFFECT`
- `TEST_ARTIFACT`
- `PACKAGE_MANAGER_SIDE_EFFECT`
- `GENERATED_ARTIFACT`
- `UNKNOWN_EXTERNAL`

Contract:

- external edits block unsafe mutation when snapshots mismatch
- command side effects are tracked separately from model-authored mutations
- rollback cannot overwrite user-owned changes
