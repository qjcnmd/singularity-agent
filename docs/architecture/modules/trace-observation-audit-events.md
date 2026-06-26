# Trace / Observation / Audit Events Runtime Flow

Runtime flow doc id: trace-observation-audit-events
Source paths:
- src/singularity/observability/models.py
- src/singularity/observability/recorder.py
- src/singularity/observability/store.py
- src/singularity/observability/artifacts.py
- src/singularity/observability/spans.py
- src/singularity/observability/summary.py
- src/singularity/context/store.py
- src/singularity/context/models.py
- src/singularity/policy/audit.py
- src/singularity/review/pipeline.py
- src/singularity/review/critic.py
- src/singularity/planner/engine.py
- src/singularity/evaluation/targeted_replay.py

Symbols:
- TraceEventType
- TraceArtifactKind
- TraceEvent
- TraceSpan
- TraceArtifact
- TraceSummary
- TraceRecorder
- TraceRecorder.emit
- TraceRecorder.record
- TraceRecorder.write_artifact
- TraceStore
- TraceStore.append_event
- TraceStore.append_artifact
- TraceStore.query_events
- TraceArtifactStore
- TraceArtifactStore.write_text_artifact
- TraceArtifactStore.write_bytes_artifact
- TraceArtifactStore.register_file_artifact
- SpanManager
- SpanManager.start_span
- SpanManager.end_span
- TraceSummaryBuilder
- ObservationStore
- ObservationStore.record_event
- ObservationStore.save_observation
- ToolObservation
- PolicyAuditWriter
- PolicyAuditWriter.append
- ReviewPipeline
- ReviewPipeline._emit
- ModelCritic
- ModelCritic.review
- Planner
- Planner.record_diff_observation
- Planner.record_review_observation
- TargetedFailureReplayRunner
- TargetedFailureReplayResult

## Module Boundary

This module owns internal observability, context observations, policy audit logs, review trace events, and trace artifact references.

It is responsible for redacted trace event creation, trace event persistence, trace artifact metadata, policy audit JSONL rows, context observation storage, and review/planner observation events.

It is not responsible for deciding model prompts or provider request schemas. Trace and audit data become model-visible only if another module intentionally renders bounded summaries into context.

## Current Source Locations

- `src/singularity/observability/models.py`: trace event/span/artifact/summary models and enums.
- `src/singularity/observability/recorder.py`: `TraceRecorder.emit()`, legacy event mapping, span and artifact APIs.
- `src/singularity/observability/store.py`: trace event, span, and artifact persistence.
- `src/singularity/observability/artifacts.py`: trace artifact storage and limits.
- `src/singularity/observability/spans.py`: `SpanManager` span lifecycle (start/end) with thread-local span stacks.
- `src/singularity/observability/summary.py`: timeline and final report summaries.
- `src/singularity/context/store.py`: context event and tool observation persistence.
- `src/singularity/context/models.py`: `ToolObservation`.
- `src/singularity/policy/audit.py`: policy audit writer.
- `src/singularity/review/pipeline.py`: review trace events.
- `src/singularity/review/critic.py`: model critic request and result boundary.
- `src/singularity/planner/engine.py`: diff and review observations.
- `src/singularity/evaluation/failure_case_replay.py`: bounded live-eval failure replay extraction from `report.json` and trace `events.jsonl`.
- `src/singularity/evaluation/targeted_replay.py`: bounded targeted replay trace refs and planner phase/status summaries derived from targeted smoke trace output.

## Runtime Call Chain

1. Runtime components call `TraceRecorder.emit()` or legacy `TraceRecorder.record()`.
2. `TraceRecorder.emit()` redacts payload, creates `TraceEvent`, computes `payload_hash`, sets `redaction_applied=True`, appends event to `TraceStore`, and notifies interaction sinks.
3. Legacy `record()` maps event names such as `planner`, `tool_call`, `model_request`, `command`, `mutation`, `verification`, `failure_analysis_requested`, and `repair_signal_consumed` to typed `TraceEventType` values.
4. Components that need files call `TraceRecorder.write_artifact()`, which delegates to `TraceArtifactStore` and stores `TraceArtifact`.
5. Components that need span lifecycle call `TraceRecorder.span()` / `start_span()` / `end_span()`, which delegate to `SpanManager`; span attributes are redacted before the span is appended to `TraceStore`.
6. Context code calls `ObservationStore.record_event()` and `save_observation()` for context-local event and observation state.
7. Policy code calls `PolicyAuditWriter.append()` with `PolicyRequest` and `PolicyDecision`.
8. Review code calls `ReviewPipeline._emit()` to send review lifecycle events.
9. Planner records review and diff observations in `Planner.record_review_observation()` and `record_diff_observation()`.
10. After live eval writes `report.json`, `FailureCaseReplayRunner` may read task trace `events.jsonl` and copy only bounded diagnostic counts, final-report outcome, blocked reasons, and recent phase-policy blocks into `failure_cases.json`. That package is marked `runner_mode="post_run_failure_extraction"`; targeted execution replay is a separate evaluation API.
11. `TargetedFailureReplayRunner` reads its own deterministic smoke trace to derive bounded `trace_refs`, `phase_history`, and `planner_status_history` for `targeted_replay_result.json`. These are evaluator artifacts and do not change trace-store schema.

## Runtime Objects Passed

- `TraceEvent`: `event_id`, `event_type`, `run_id`, `session_id`, `task_id`, `phase_id`, `action_id`, `parent_event_id`, `timestamp`, `monotonic_ms`, `component`, `severity`, `summary`, `payload`, `artifact_refs`, `policy_decision_id`, `approval_grant_id`, `sandbox_id`, `command_id`, `transaction_id`, `verification_id`, `span_id`, `redaction_applied`, `payload_hash`.
- `TraceArtifact`: `artifact_id`, `run_id`, `session_id`, `task_id`, `kind`, `path`, `relative_path`, `size_bytes`, `sha256`, `content_type`, `redacted`, `sensitive`, `summary`, `metadata`.
- `ToolObservation`: persisted context observation with preview, raw digest, source refs, duration, cache, error, and sensitivity.
- `PolicyAuditEntry`: normalized and redacted policy request/decision audit row.
- Review trace payloads: review stage, findings, decision, report id, transaction id, verification id, policy decision id.
- Semantic Planner / Final Reviewer trace events: `semantic_planner.task_contract.model_ok`, `semantic_planner.task_contract.fallback`, `semantic_planner.semantic_plan.model_ok`, `semantic_planner.semantic_plan.fallback`, `semantic_planner.planner_decision.model_ok`, `semantic_planner.planner_decision.fallback`, `final_reviewer.assess.done`, `final_reviewer.assess.model_ok`, and `final_reviewer.assess.fallback`.
- Failure replay trace summary: events path, event count, availability flag, failure-analysis event count, repair event count, final-report outcome, blocked reasons, and recent phase-policy blocks. This is a derived evaluator object, not a trace-store schema change.
- Targeted replay trace refs: JSONL path, event count, failure-analysis event count, repair event count, and planner event count. This is a bounded evaluator object, not a trace-store schema change and not a prompt/context capture.
- Failure replay package metadata: `runner_mode` and `targeted_replay_runner` labels written by `FailureCaseReplayRunner.write()` so readers distinguish extraction from execution replay.

## Model-Visible Objects (模型实际可见对象)

The model does not receive `TraceEvent`, `TraceArtifact`, `PolicyAuditEntry`, raw `ObservationStore` rows, `FailureCaseRecord.trace_summary`, or targeted replay result `trace_refs` / phase histories.

The model can see trace-adjacent data only after bounded projection into context, for example:

- tool observation previews rendered as tool messages;
- planner review summaries recorded into planner/context evidence;
- artifact ids or references included in tool observation payloads;
- context summaries that mention refs or verification status.

`ModelCritic.review()` is a separate model call and sends a bounded review target/report prompt, not the full trace store or audit log.

## Internal Trace Debug Audit Objects (内部 trace/debug/audit 对象)

Internal-only objects include:

- full trace payloads after redaction;
- payload hashes;
- run/session/task/phase/action ids;
- policy decision and approval grant ids;
- sandbox, command, transaction, verification, and span ids;
- trace artifact absolute paths on disk;
- policy audit rows and grant scopes;
- context event rows and raw observation storage after redaction;
- review internal evidence and decision ids.
- full live-eval task trace files read by `FailureCaseReplayRunner`; only bounded summaries are copied into replay records.
- `failure_cases.json` package metadata that identifies extraction-only mode and the separate targeted replay runner.
- `targeted_replay_result.json` / `.md` trace refs and planner phase/status histories. These point to bounded evidence and do not contain full prompts, hidden verification content, full trace payloads, or large context bodies.

## State Transitions And Failure Paths

- Trace write failures are caught and printed as redacted warnings rather than failing the run.
- Interaction sink failures are caught and printed as redacted warnings.
- Artifact writes enforce per-artifact and total-size limits.
- `register_file_artifact` passes source file content through `TraceRedactor.redact_text()` for all text files (both `sensitive=True` and `sensitive=False`); the `redacted` flag is set `True` when text redaction was applied and `False` when a non-sensitive binary file fell back to byte copy. Sensitive binary files raise `TraceArtifactError` because they cannot be text-redacted.
- `TraceStore.__init__` validates `run_id` and raises `ValueError` for path traversal (`..`), absolute paths (leading `/` or drive letter), empty values, or any character outside `[A-Za-z0-9_-]`.
- `SpanManager` stores the active span stack in `threading.local()` so each thread has an independent parent-child span view, and guards `start_span`/`end_span` with an `RLock` to serialize `TraceStore` file I/O; the span body executes without holding the lock.
- `ObservationStore.save_observation()` redacts secret/sensitive content and removes raw keys before storage.
- `PolicyAuditWriter.append()` redacts request and decision fields before JSONL append.
- Review trace events can still be non-blocking if model critic fails.
- Failure replay extraction treats missing or unreadable trace files as non-fatal diagnostic gaps: `events_available=false`, `event_count=0`, and the source report remains the authoritative task-failure record.

## Current Structure Assessment

The structure is intentionally multi-channel: trace captures runtime events, context store captures model-context observations, and policy audit captures permission decisions. This is a reasonable separation because each channel has different retention and visibility expectations.

The risk is that all three channels use dictionary payloads in places. Runtime Flow Docs must keep the model-visible projection distinct from trace/audit payloads.

## Production-Grade Target Structure

Current code has no single `AuditBoundaryClassifier`.

A production-grade target could add proposed classification fields:

- proposed `visibility: model|trace|audit|artifact|storage`;
- proposed `redaction_policy_id`;
- proposed `retention_class`;
- proposed `model_projection_allowed`;
- proposed `external_export_allowed`.

These are proposed only. Current code uses redactors, trace models, context rendering, and audit writers separately.

## Harness Usage Example

A tool call is denied by policy. `PolicyEngine` emits policy trace and writes audit. `ToolExecutor` returns a failure `ToolResult` with an error code. `ToolProtocolEngine` records protocol trace and appends a bounded tool result to context. The next model turn sees the failure code in a tool message, while the full policy request, resource details, audit row, and trace ids remain internal.

## Maintenance Rules

Update this document when changing:

- `TraceEvent`, `TraceArtifact`, trace event types, or trace store schema;
- `TraceRecorder.emit()`, `record()`, or artifact writing;
- `ObservationStore.record_event()` or `save_observation()`;
- policy audit serialization or redaction;
- review trace event payloads;
- planner diff/review observation fields;
- `FailureCaseReplayRunner._trace_summary()` or fields copied from trace into failure replay artifacts;
- `FailureCaseReplayRunner.write()` metadata that classifies failure replay artifacts;
- `TargetedFailureReplayRunner` fields copied from trace into targeted replay artifacts;
- any decision to render trace/audit data into model context.

## Verification

- `python scripts/verify_runtime_docs.py`
- `python -m pytest tests/test_observability_models.py tests/test_trace_store.py tests/test_trace_artifacts.py tests/test_trace_timeline_summary.py tests/test_policy_audit.py tests/test_observability_integration.py tests/review --basetemp work/pytest-tmp`

## Last Verified Against

Last verified against commit `5f2202bd8cfcc2a4e4a66c025891550e52f3556e` on 2026-06-25.
