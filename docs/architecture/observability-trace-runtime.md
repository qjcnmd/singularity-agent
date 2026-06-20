# Observability / Trace Runtime

Miniharness v0.0.13 adds `src/miniharness/observability/` as the local trace backbone. This is not print logging. It is an append-only, structured event system for runtime facts, spans, artifacts, timelines, context summaries, and final report summaries.

The compact boundary is:

```txt
PlannerRuntime / ToolRuntime / PolicyRuntime / ApprovalGate
CommandRuntime / SandboxRuntime / MutationRuntime / VerificationRuntime
ContextManager / Finalizer
  -> TraceRuntime
  -> TraceRedactor
  -> TraceStore + TraceArtifactStore
  -> timeline / summary / final report / compact context
```

`TraceRuntime` records facts only. It does not execute tools, approve actions, mutate files, run commands, or alter policy decisions. Trace write failures are downgraded to a warning on stderr and do not change safety logic.

## Runtime Objects

`models.py` defines the stable wire objects:

```txt
TraceEvent
TraceSpan
TraceArtifact
TraceTimelineItem
TraceSummary
TraceSeverity
TraceStatus
TraceEventType
TraceArtifactKind
```

Events carry run, session, task, phase, action, command, sandbox, mutation transaction, verification, policy decision, approval grant, span, and artifact correlation ids. Spans provide nested runtime duration tracking. Artifacts store large or inspectable material outside the event payload.

## Store Layout

Each run writes under:

```txt
work/traces/runs/<run_id>/
  events.jsonl
  spans.jsonl
  artifacts.jsonl
  index.json
  artifacts/
```

Writes are append-only JSONL and are flushed after every append. `spans.jsonl` may contain multiple records for the same span id; the latest record is the current state. This preserves append-only history while exposing `latest_spans()` and `recover_incomplete_spans()` for normal readers.

Large stdout, stderr, diffs, reports, model messages, sandbox logs, and generic binary data must be written to `TraceArtifactStore`. Events only keep `artifact_id` / `artifact_ref` or relative handles. Absolute artifact paths remain internal to the store and are resolved through `TraceArtifactStore.read_artifact(...)`.

## Redaction

All payloads and non-sensitive artifacts pass through `TraceRedactor`.

It recursively redacts dicts, lists, and strings. Default matching covers API keys, token, password, secret, Authorization, Cookie, private keys, `.env`-style values, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GITHUB_TOKEN`, and `NPM_TOKEN`.

Redacted values are replaced with:

```txt
<redacted>
```

The original secret prefix or suffix is not preserved. Payload hashes are computed from the redacted payload so query and deduplication behavior does not leak sensitive values.

## Runtime Integration

`TraceRuntime.create(project_root)` is now the CLI default. It still offers `record(event, data)` so existing runtime code can be migrated gradually, but new code should use `emit(...)`, `span(...)`, and `write_artifact(...)`.

Planner integration records task start, action results, replans, completion assessment, and final report completion.

Tool integration records validation start/failure and dispatch start/completion/failure. Tool arguments are redacted and hashed. Oversized tool results remain bounded by `ToolRuntime`; future large result promotion should use `TraceArtifactStore`.

Policy and approval integration records policy requested/decided/blocked and approval requested/granted/denied. Full policy audit remains in `.miniharness/policy/audit.jsonl`; structured trace stores decision ids and redacted summaries.

Command integration records command requested/started/completed/failed/timeout/killed. Existing command output artifacts remain under `.miniharness/artifacts/commands/` when generated, and events reference artifact handles instead of embedding full output.

Sandbox integration records sandbox requested/prepared/started/completed/violation/cleaned/capability_failed when `TraceRuntime` is passed in. Legacy `.miniharness/sandbox/trace.jsonl` remains supported when `SandboxTraceWriter` is used directly; it records cwd/workspace/sandbox handles rather than absolute sandbox roots.

Mutation integration records mutation transaction start, applied file operations, failures, and rollback completion. Diff bodies stay in diff artifacts.

Verification integration maps plan creation, check completion/failure, evidence, repair hints, and completion assessment into structured trace through the compatibility bridge.

Context integration records snapshot/compaction/observation/render events, but only accepts compact trace summary lines for model context. It never injects full trace payloads or full prompts into the model context.

Final reports include `execution_trace_summary` with actions, failed actions, tool calls, commands, sandboxed commands, workspace mutations, verification checks, policy denials, approvals, replans, key failures, and key artifacts.

## CLI

The trace CLI is intentionally small:

```powershell
miniharness trace list
miniharness trace show <run_id>
miniharness trace timeline <run_id>
miniharness trace errors <run_id>
miniharness trace artifacts <run_id>
```

`trace artifacts` shows artifact id, kind, size, summary-level metadata, and a relative handle. It does not print the internal absolute artifact path. These commands read the local append-only store. They do not replay actions or contact remote telemetry systems.

## Reserved Extensions

The current implementation deliberately does not include:

```txt
OpenTelemetry exporter
remote telemetry upload
web trace viewer
trace replay executor
distributed trace propagation
cross-machine trace aggregation
Git Runtime / PR / branch trace integration
```

Those are extension points on top of the local trace store, not hidden behavior in this release.
