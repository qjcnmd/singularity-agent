# Context Assembly / Prompt Frame / Model Request Builder Runtime Flow

Runtime flow doc id: context-assembly-prompt-frame
Source paths:
- src/singularity/agent_loop.py
- src/singularity/model/request_builder.py
- src/singularity/instructions/prompt_assembly.py
- src/singularity/context/manager.py
- src/singularity/context/assembler.py
- src/singularity/context/models.py
- src/singularity/context/store.py

Symbols:
- AgentLoop
- AgentLoop.run
- ModelTurnRequestBuilder
- ModelTurnRequestBuilder.build_request
- PromptAssemblyPipeline
- PromptAssemblyPipeline.build_for_model_turn
- ContextManager
- ContextManager.messages
- ContextManager.build_bundle
- ContextManager.persist_bundle
- ContextManager.instruction_sources
- ContextAssembler
- ContextAssembler.build_bundle
- ContextAssembler.assemble
- ContextAssembler.needs_compression
- ContextBundle
- ContextItem
- ContextRenderPolicy
- ContextBudgetPlan
- ObservationStore
- ObservationStore.append_message
- ObservationStore.append_item
- ObservationStore.save_bundle

## Module Boundary

This module owns the path from planner state and stored observations to model-visible prompt messages.

It is responsible for prompt frame assembly, dynamic context selection, message ordering, token budgeting, redaction, context bundle persistence, and the final `ModelTurnRequest.messages` projection.

It is not responsible for provider API transport, tool execution, policy approval, or context compaction internals beyond calling the compaction path when the current message set does not fit.

## Current Source Locations

- `src/singularity/agent_loop.py`: `AgentLoop.run()` calls model request construction per turn.
- `src/singularity/model/request_builder.py`: `ModelTurnRequestBuilder.build_request()` merges stable prompt messages and dynamic context messages.
- `src/singularity/instructions/prompt_assembly.py`: `PromptAssemblyPipeline.build_for_model_turn()` builds the stable prompt frame.
- `src/singularity/context/manager.py`: `ContextManager.messages()`, `build_bundle()`, `persist_bundle()`, and `instruction_sources()`.
- `src/singularity/context/assembler.py`: `ContextAssembler.build_bundle()`, grouping, ordering, budgeting, and rendering.
- `src/singularity/context/models.py`: `ContextItem`, `ContextBundle`, `ContextBudgetPlan`, `ContextRenderPolicy`.
- `src/singularity/context/store.py`: message, item, and bundle persistence.

## Runtime Call Chain

1. `AgentLoop.run()` enters `run_turn()`.
2. `planner.step()` selects the current phase and action.
3. `planner.filtered_tools()` returns active provider tool schemas.
4. `ModelRunner.build_request_from_context()` calls `ModelTurnRequestBuilder.build_request()`.
5. `ModelTurnRequestBuilder.build_request()` calls `ModelToolRenderer.render()` and `to_provider_tools()` for tool schemas and tool token budget.
6. If prompt assembly is enabled, `PromptAssemblyPipeline.build_for_model_turn()` builds the stable prompt frame.
7. `ContextManager.messages(tools=provider_tools, planner_context=..., persist=True)` checks whether compression is needed.
8. `ContextManager.build_bundle()` loads current items with `ObservationStore.query_items()`, adds planner context as a temporary item, includes active summaries, and calls `ContextAssembler.build_bundle()`.
9. `ContextAssembler.build_bundle()` filters visible/current items, groups assistant/tool protocol messages, scores groups, orders selected groups, computes `ContextBudgetPlan`, and returns `ContextBundle`.
10. `ContextManager.persist_bundle()` saves the bundle and emits `context.bundle_built` and `context.rendered_for_model`.
11. `ModelTurnRequestBuilder.build_request()` merges stable and dynamic messages, computes prompt/context/tool hashes, and stores those hashes in `context_metadata` and `trace_metadata`.
12. `ModelTurnRequest.messages` becomes the typed model-boundary list.

## Runtime Objects Passed

- `ContextItem`: `item_id`, `run_id`, `session_id`, `task_id`, `phase_id`, `layer`, `source_component`, `item_type`, `content`, `content_digest`, timestamps, `importance`, `relevance_score`, `authority`, `freshness`, `sensitivity`, `token_count`, `references`, `metadata`, `pinned`, `expires_at`.
- `ContextRenderPolicy`: `include_raw_tool_outputs`, `include_policy_details`, `include_secret_content`, `include_full_diff`, `include_failed_attempts`, `max_tool_preview_tokens`, `max_evidence_items`, `max_recent_turns`, `require_references_for_claims`, `redact_sensitive`, `phase_aware`.
- `ContextBundle`: `bundle_id`, `run_id`, `task_id`, `phase_id`, `model`, `provider`, `messages`, `included_item_ids`, `excluded_item_ids`, `budget`, `compression_snapshot_id`, `retrieval_query`, `render_policy`, `created_at`, `bundle_digest`, `metadata`.
- `ContextBudgetPlan`: token window, reserves, tool schema tokens, system/pinned/evidence/recent/summary tokens, available/used/overflow tokens, limits, message tokens.
- `ModelTurnRequest.context_metadata`: context budget and prompt/context/tool hashes generated by request builder.

## Model-Visible Objects (模型实际可见对象)

The model sees only `ModelTurnRequest.messages` and provider tool schemas.

Context messages include:

- pinned system and user goal messages;
- stable prompt frame messages from `PromptAssemblyPipeline`;
- planner context rendered into message content;
- selected `ContextItem` projections rendered by `ContextAssembler._message_for_item()`;
- bounded/redacted tool messages from tool observations;
- compressed history summary messages when present.

The model can see context identifiers, content digests, reference ids, truncation markers, and bounded previews when those are intentionally rendered into message content.

## Internal Trace Debug Audit Objects (内部 trace/debug/audit 对象)

Internal-only context data includes:

- full `ContextItem` metadata, freshness, authority, sensitivity, token counts, and persistence sequence;
- `ContextBundle.included_item_ids`, `excluded_item_ids`, `budget`, and metadata such as `context_shape_hash`, `context_ordering_hash`, `context_usage_report`, and cache attribution;
- `ObservationStore` SQLite tables for messages, context items, events, bundles, references, snapshots, and summaries;
- prompt manifest ids, prompt hashes, stable/dynamic tail hashes, and tool schema hashes in request metadata;
- trace events emitted by `ContextManager.persist_bundle()`.

## State Transitions And Failure Paths

- If `ContextAssembler.needs_compression()` returns true, `ContextManager.messages()` calls `_compress_if_possible()` before building the bundle.
- If required messages plus tool schema tokens exceed the context window, `ContextOverflowError` is raised.
- Secret items are not rendered unless policy permits secret inclusion; otherwise they are redacted.
- Stale items are excluded unless pinned/current.
- If compaction fails after being attempted, `ContextManager.messages()` falls back to minimal messages and emits compaction failure events.
- `ObservationStore.append_item()` uses optimistic versioning and can raise `ContextVersionConflict`.

## Current Structure Assessment

The current structure has a clear separation between control plane (`ContextManager`), rendering and token budgeting (`ContextAssembler`), storage (`ObservationStore`), and request composition (`ModelTurnRequestBuilder`).

The main drift risk is that `ContextBundle.metadata` can grow into an implicit API. Anything that is meant to be model-visible must be rendered into `messages`; metadata alone is internal.

## Production-Grade Target Structure

Current code does not have a separate `PromptFrameContract` object.

A production-grade target could add a proposed contract containing:

- proposed `stable_prefix_schema_version`;
- proposed `dynamic_tail_schema_version`;
- proposed `model_visible_message_hash`;
- proposed `internal_metadata_hash`;
- proposed leak-check results for raw tool output and secret content.

These are proposed fields, not current code.

## Harness Usage Example

During an edit task, the context store may contain the user goal, project index context, memory context, planner state, assistant tool calls, tool result observations, and verification evidence. `ContextAssembler` scores and orders these items. The next model request receives only the selected message projections and tool schemas, while the bundle stores which items were excluded and why.

## Maintenance Rules

Update this document when changing:

- prompt assembly output shape or supported message roles;
- `ContextItem`, `ContextBundle`, `ContextBudgetPlan`, or `ContextRenderPolicy`;
- `ContextManager.messages()`, `build_bundle()`, `persist_bundle()`, or `instruction_sources()`;
- `ContextAssembler` grouping, ordering, redaction, bounding, or budgeting;
- `ModelTurnRequestBuilder.build_request()` message merge behavior.

## Verification

- `python scripts/verify_runtime_docs.py`
- `python -m pytest tests/test_context.py tests/test_context_production.py tests/test_context_budget.py tests/test_context_assembler_retrieval.py tests/test_prompt_assembly.py tests/test_instruction_integration.py --basetemp work/pytest-tmp`

## Last Verified Against

Last verified against commit `5f2202bd8cfcc2a4e4a66c025891550e52f3556e` on 2026-06-25.
