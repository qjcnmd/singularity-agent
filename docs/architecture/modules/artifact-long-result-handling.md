# Artifact / Long Result Handling Runtime Flow

Runtime flow doc id: artifact-long-result-handling
Source paths:
- src/singularity/tools/models.py
- src/singularity/tools/executor.py
- src/singularity/tool_protocol/result.py
- src/singularity/tool_protocol/models.py
- src/singularity/context/manager.py
- src/singularity/context/assembler.py
- src/singularity/context/store.py
- src/singularity/command/output.py
- src/singularity/command/executor.py
- src/singularity/command/models.py
- src/singularity/observability/artifacts.py
- src/singularity/observability/models.py
- src/singularity/planner/engine.py
- src/singularity/planner/finalizer.py

Symbols:
- ToolResult
- ToolSpec
- ToolExecutor
- ToolExecutor._handler_output_to_result
- ToolExecutor._record_trace
- ToolProtocolResultBuilder
- ToolProtocolResultBuilder.build
- ToolObservationView
- ToolObservationView.to_model_payload
- ToolProtocolResultEnvelope
- ContextManager
- ContextManager.add_tool_result
- ContextManager.add_tool_protocol_result
- ContextAssembler
- ContextAssembler._bounded_tool_content
- ObservationStore
- ObservationStore.save_observation
- OutputCollector
- OutputCollector.add
- OutputCollector.snapshot
- OutputCollector._write_artifact
- CommandExecutor
- CommandExecutor.run
- CommandExecutor._record_trace
- CommandResult
- TraceArtifactStore
- TraceArtifactStore.write_text_artifact
- TraceArtifactStore.write_bytes_artifact
- TraceArtifactStore.register_file_artifact
- TraceArtifact
- Planner
- Planner.record_diff_observation
- Planner.record_review_observation
- Planner.finalize
- FinalReportRenderer
- FinalReportRenderer.write_markdown

## Module Boundary

This module owns bounded model-visible previews and internal artifact references for large or sensitive outputs.

It is responsible for output truncation, content digests, raw result references, artifact refs, redacted preview storage, command output artifacts, trace artifacts, and context-level bounding of tool messages.

It is not a single centralized runtime today. Current implementation is distributed across tool execution, command execution, tool protocol, context rendering, observability artifacts, and planner summaries.

## Current Source Locations

- `src/singularity/tools/executor.py`: tool handler output limiting, `ToolResult.truncated`, `output_digest`, trace payloads.
- `src/singularity/tools/models.py`: `ToolResult` and `ToolSpec.max_output_chars` / `artifact_policy`.
- `src/singularity/tool_protocol/result.py`: result envelope preview and optional raw result persistence.
- `src/singularity/tool_protocol/models.py`: `ToolProtocolResultEnvelope`, `ToolObservationView`, `artifact_refs`, `raw_result_ref`.
- `src/singularity/context/manager.py`: tool result and protocol result context messages.
- `src/singularity/context/assembler.py`: bounded tool content rendering.
- `src/singularity/context/store.py`: redacted observation storage.
- `src/singularity/command/output.py`: command output collection, truncation, artifact materialization.
- `src/singularity/command/executor.py`: command result trace and artifact refs.
- `src/singularity/command/models.py`: `CommandResult`.
- `src/singularity/observability/artifacts.py`: `TraceArtifactStore`.
- `src/singularity/observability/models.py`: `TraceArtifact`.
- `src/singularity/planner/engine.py`: planner records artifact refs in diff/review/final evidence.
- `src/singularity/planner/finalizer.py`: final report markdown artifact output.

## Runtime Call Chain

Tool result path:

1. A tool handler returns content to `ToolExecutor._handler_output_to_result()`.
2. `_limit_output()` bounds output by `ToolSpec.max_output_chars`, sets `ToolResult.truncated`, and stores `output_digest` in metadata.
3. `ToolProtocolResultBuilder.build()` creates a redacted `content_preview`, `content_digest`, optional `raw_result_ref`, `artifact_refs`, and truncation flag.
4. `ContextManager.add_tool_protocol_result()` turns the envelope into a bounded tool message and stores internal metadata in `ToolObservation`.
5. `ContextAssembler._bounded_tool_content()` can further bound tool message content before model request assembly.

Command result path:

1. `CommandExecutor.run()` uses `OutputCollector`.
2. `OutputCollector.add()` accumulates bounded stdout/stderr and may call `_write_artifact()` for large output.
3. `CommandResult` carries stdout/stderr previews, combined preview, `output_truncated`, `output_digest`, and `artifact_path`.
4. `CommandExecutor._record_trace()` emits command trace and artifact refs.
5. Verification/planner/context consume summaries and refs, not full command output.

Trace artifact path:

1. Components call `TraceRecorder.write_artifact()`.
2. `TraceArtifactStore` writes or registers a file, enforces size limits, redacts sensitive text artifacts, computes sha256, and returns `TraceArtifact`.
3. `TraceStore.append_artifact()` persists artifact metadata.

## Runtime Objects Passed

- `ToolResult`: `ok`, `content`, `error_code`, `error`, `truncated`, `metadata`.
- `ToolProtocolResultEnvelope`: `content_preview`, `content_digest`, `raw_result_ref`, `artifact_refs`, `truncated`, `redacted`, `metadata`.
- `ToolObservationView`: model payload projection with content preview, digest, result ref, reference ids, observation id, truncation, and redaction.
- `ToolObservation`: raw result after projection, preview, raw digest, source refs, metadata, sensitivity, and truncation reason.
- `CommandResult`: command ids, process status, execution status, semantic status, stdout/stderr/combined previews, `output_truncated`, `output_digest`, `artifact_path`, policy and sandbox metadata.
- `OutputSnapshot`: stdout/stderr snippets, output truncation, digest, and artifact refs from command output collection.
- `TraceArtifact`: artifact id, run/session/task ids, kind, path, relative path, size, sha256, content type, redacted, sensitive, summary, metadata.

## Model-Visible Objects (模型实际可见对象)

The model sees bounded results only:

- tool message payload from `ToolObservationView.to_model_payload()`;
- `content` or `content_preview` only when visibility permits;
- `content_digest`;
- `result_ref`;
- `reference_ids`;
- `truncated`;
- `redacted`;
- command/verification summaries if rendered through context.

The model does not receive full raw output, `ToolResult.metadata`, policy ids, approval ids, raw arguments, command artifact file paths unless a bounded summary intentionally includes a ref, or trace artifact absolute paths.

## Internal Trace Debug Audit Objects (内部 trace/debug/audit 对象)

Internal-only data includes:

- `ToolResult.metadata.output_digest`, `duration_seconds`, backend, handler isolation, cache flags, policy decision id, approval grant id;
- `ToolProtocolResultEnvelope.raw_result_ref`, `artifact_refs`, policy/approval ids, and metadata;
- `ToolObservation.raw_result`, raw digest, sensitivity, source refs, and metadata after redaction;
- `CommandResult.artifact_path`, output digest, sandbox metadata, and trace refs;
- `TraceArtifact.path`, sha256, size, content type, sensitivity, and metadata;
- planner artifact refs in diff/review/final report evidence.

## State Transitions And Failure Paths

- Tool output longer than `ToolSpec.max_output_chars` is truncated head/tail and marked `ToolResult.truncated=True`.
- `ToolProtocolResultBuilder` truncates preview when it exceeds `max_preview_chars`.
- `ContextManager.add_tool_result()` truncates raw tool result previews at `TOOL_RESULT_PREVIEW_LIMIT`.
- `ContextAssembler._bounded_tool_content()` truncates tool message payloads again at render time if needed.
- `OutputCollector` marks command output truncated and materializes artifacts for large command streams.
- `TraceArtifactStore` rejects files larger than `max_artifact_bytes` or total artifact storage larger than `max_total_bytes`.
- Sensitive file artifacts must be text-redactable.

## Current Structure Assessment

Artifact and long-result handling is real but distributed. Tool, command, context, and trace layers each own their local part. Planner consumes previews, digests, and refs instead of full raw outputs.

Current gap: there is no unified `ArtifactRuntime`, `LongResultStore`, or planner-level long-result manager. `ToolSpec.artifact_policy` exists as a field, but the current tool execution path primarily uses output truncation and metadata refs rather than a central policy-driven artifact service.

## Production-Grade Target Structure

A production-grade target could introduce a proposed long-result service with:

- proposed `LongResultRef`;
- proposed `visibility` field;
- proposed `model_preview_policy`;
- proposed `retention_policy`;
- proposed `artifact_kind`;
- proposed `source_component`;
- proposed read API for planner and final report generation.

This is not current implementation. Today the behavior is implemented by `ToolExecutor`, `ToolProtocolResultBuilder`, `ContextManager`, `OutputCollector`, `TraceArtifactStore`, and planner summary consumers.

## Harness Usage Example

A command produces megabytes of pytest output. `OutputCollector` keeps bounded stdout/stderr previews, marks `output_truncated=True`, writes an artifact, and computes `output_digest`. `CommandResult` and verification evidence carry the digest and artifact path/ref. The model sees a bounded verification/tool message with failure summary and refs. Trace and final reports can reference artifacts, but the full output is not copied into the model request.

## Maintenance Rules

Update this document when changing:

- `ToolResult`, `ToolProtocolResultEnvelope`, `ToolObservationView`, or `ToolObservation`;
- `ToolSpec.max_output_chars` or `artifact_policy` behavior;
- `ToolExecutor._handler_output_to_result()` or `_record_trace()`;
- `ToolProtocolResultBuilder.build()`;
- `ContextManager.add_tool_result()` or `add_tool_protocol_result()`;
- context bounded tool rendering;
- command `OutputCollector`, `CommandResult`, or command artifact behavior;
- `TraceArtifactStore` or `TraceArtifact`;
- planner/finalizer artifact refs.

## Verification

- `python scripts/verify_runtime_docs.py`
- `python -m pytest tests/test_context.py tests/test_tool_protocol_result.py tests/test_tool_protocol_models.py tests/test_tool_executor.py tests/test_tool_executor_redaction.py tests/test_command_executor.py tests/test_workspace_mutation.py tests/test_trace_artifacts.py --basetemp work/pytest-tmp`

## Last Verified Against

Last verified against commit `5f2202bd8cfcc2a4e4a66c025891550e52f3556e` on 2026-06-25.
