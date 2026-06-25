# Context Compaction / Snapshot / ObservationStore Runtime Flow

Runtime flow doc id: context-compaction-observation-store
Source paths:
- src/singularity/context/manager.py
- src/singularity/context/compaction.py
- src/singularity/context/compression.py
- src/singularity/context/store.py
- src/singularity/context/models.py
- src/singularity/context/usage.py

Symbols:
- ContextManager
- ContextManager._compress_if_possible
- ContextManager._handle_compaction_failure
- ContextManager._observe_compaction
- ContextManager._observe_compaction_committed
- ContextManager._observe_compaction_failed
- ContextManager._fallback_messages_for_compaction_failure
- CompactionGroup
- CompactionPlan
- ContextCompactionPlanner
- ContextCompactionPlanner.prepare
- ContextCompactionExecutor
- ContextCompactionExecutor.render
- ContextCompactionExecutor.run_llm_compaction
- ContextCompactionCommitter
- ContextCompactionCommitter.commit
- ContextCompactionCommitter.recover_after_failure
- ContextCompactionCommitter.compacted_messages
- ContextCompressor
- ContextCompressor.parse_summary
- ObservationStore
- ObservationStore.append_message
- ObservationStore.append_item
- ObservationStore.record_event
- ObservationStore.save_observation
- ObservationStore.save_snapshot
- ObservationStore.latest_snapshot
- ObservationStore.save_bundle
- ContextSnapshot
- ContextSummaryPayload
- ContextSummaryEnvelope
- ToolObservation
- ContextUsageReporter

## Module Boundary

This module owns context history reduction, snapshot persistence, observation persistence, context recovery, and context usage diagnostics.

It is responsible for deciding what can be compacted, rendering deterministic or LLM summaries, validating summary payloads, committing snapshots, marking omitted items stale, storing observations and references, and preserving enough retained messages for continued model interaction.

It is not responsible for generating the main task model request except through `ContextManager.messages()` and `ContextAssembler` after compaction is complete.

## Current Source Locations

- `src/singularity/context/manager.py`: compression trigger, failure handling, compaction events, fallback messages.
- `src/singularity/context/compaction.py`: planner, executor, committer, compaction plan metadata, safe message helpers.
- `src/singularity/context/compression.py`: summary validation and schema parsing.
- `src/singularity/context/store.py`: SQLite persistence for messages, items, observations, events, bundles, references, snapshots, and summaries.
- `src/singularity/context/models.py`: `ContextSnapshot`, `ContextSummaryPayload`, `ContextSummaryEnvelope`, `ToolObservation`.
- `src/singularity/context/usage.py`: usage/cache reporting.

## Runtime Call Chain

1. `ContextManager.messages()` asks `ContextAssembler.needs_compression()` whether the current messages plus tools fit.
2. If needed or forced, `ContextManager._compress_if_possible()` calls `ContextCompactionPlanner.prepare()`.
3. `ContextCompactionPlanner.prepare()` reads current items from `ObservationStore.query_items()`, identifies retained ids, current summaries, recent tail, and compaction buckets.
4. `ContextManager._observe_compaction()` emits `context.compaction_requested`.
5. `ContextCompactionExecutor.render()` optionally calls `run_llm_compaction()` for LLM buckets and produces a normalized summary payload.
6. `ContextCompressor.parse_summary()` validates the summary against source items and prior summary.
7. `ContextCompactionExecutor.summary_envelope_for_plan()` wraps the summary in `ContextSummaryEnvelope`.
8. `ContextCompactionCommitter.commit()` adds a summary `ContextItem`, retires previous summaries, marks omitted items stale via `ObservationStore.compact_items()`, saves summary and snapshot, and replaces manager messages with retained messages.
9. `ContextManager._observe_compaction_committed()` emits `context.compaction_completed`.
10. If any stage fails, `_handle_compaction_failure()` calls `ContextCompactionCommitter.recover_after_failure()` or builds a minimal tail fallback, then emits `context.compaction_failed`.

## Runtime Objects Passed

- `CompactionPlan`: `source_item_ids`, `buckets`, `retained_item_ids`, `current_summary_item_ids`, `omitted_item_ids`, `llm_buckets`, `deterministic_buckets`, `archive_buckets`, `recent_tail`, `previous_summary`, `cache_attribution`, `partial_range`.
- `CompactionGroup`: `group_id`, `layer`, `item_type`, `source_component`, `item_ids`, `mode`, `utility_score`, `token_cost`, `volatility`, `reference_density`, `recency_score`, `content_digest`, `fragment`.
- `ContextSummaryPayload`: goal, current state, completed actions, pending actions, verified facts, failed attempts, policy constraints, workspace changes, verification status, open questions, reference ids, omitted item ids, confidence.
- `ContextSummaryEnvelope`: version, summary id, summary payload, source item ids, cache attribution, previous summary digest, summary digest, rendered summary, metadata.
- `ContextSnapshot`: `snapshot_id`, `run_id`, `session_id`, `task_id`, `goal`, `summary`, `retained_item_ids`, `known_observation_ids`, `version`, `created_at`, `retained_messages`, `metadata`.
- `ToolObservation`: id, tool name, call id, ok, raw result, preview, truncation, metadata, run id, turn, token counts, digest, refs, cache, duration, error code, sensitivity.

## Model-Visible Objects (模型实际可见对象)

The model sees compaction only through retained messages:

- base system and user messages;
- a system message like `Context summary:\n...` containing `ContextCompactionExecutor.render_summary_for_context()` output;
- recent tail assistant/tool/user messages that fit;
- bounded tool observation payloads from context rendering.

The model does not see `CompactionPlan`, `CompactionGroup`, `ContextSnapshot.metadata`, `ContextSummaryEnvelope.metadata`, raw store rows, or SQLite state.

## Internal Trace Debug Audit Objects (内部 trace/debug/audit 对象)

Internal-only objects include:

- compaction plan metadata, utility scores, omitted ids, retained ids, bucket modes, and cache attribution;
- full summary envelope and summary payload;
- `ObservationStore` rows for context events, bundles, summaries, snapshots, references, and observations;
- raw observation storage after redaction and removal of raw keys;
- context usage diagnostics, cached input tokens, and cache miss reasons;
- compaction failure payloads with stage, error type, partial range, fallback result, and plan metadata.

## State Transitions And Failure Paths

- Current summary items can be superseded by new summary item ids.
- Omitted item ids are marked stale during `ObservationStore.compact_items()`.
- Summary commit increments `_compaction_generation`.
- `recover_after_failure()` can restore latest snapshot retained messages.
- If snapshot recovery fails or no snapshot exists, the manager falls back to base messages plus recent tail.
- `ContextCompressor.parse_summary()` can reject invalid JSON, invalid schema, missing references, or drifted summaries.
- `ObservationStore` redacts secret/sensitive content before storage unless explicitly configured to allow raw secret storage.

## Current Structure Assessment

The current compaction path is mature enough to be a real runtime mechanism, not a placeholder. It separates plan, execute, and commit roles and persists snapshots for recovery.

The main complexity is that compaction crosses `ContextManager`, `ContextCompaction*`, `ContextCompressor`, `ObservationStore`, and `ContextUsageReporter`. Drift risk is high when any summary fields or store schemas change.

## Production-Grade Target Structure

Current code has no single `ContextRecoveryRuntime` object that owns all recovery and drift policy.

A production-grade target could add proposed fields and objects:

- proposed `compaction_decision_id`;
- proposed `summary_drift_score`;
- proposed `source_coverage_ratio`;
- proposed `recovery_strategy`;
- proposed `model_visible_summary_hash`.

These are proposed only. Current code stores similar evidence across plan metadata, snapshot metadata, context events, and bundle metadata.

## Harness Usage Example

After several read, edit, and verification turns, the message history no longer fits with current tool schemas. `ContextCompactionPlanner` retains pinned system/user context and latest policy/planner state, buckets old tool observations, and keeps the recent tail. `ContextCompactionExecutor` summarizes older failures and verification facts. `ContextCompactionCommitter` stores a snapshot and replaces old messages with a summary plus tail. The next model turn sees the summary, not the full old transcript.

## Maintenance Rules

Update this document when changing:

- `CompactionPlan`, `CompactionGroup`, `ContextSummaryPayload`, `ContextSummaryEnvelope`, or `ContextSnapshot`;
- compaction planner scoring, bucket modes, retained rules, or recent-tail rules;
- LLM summary prompt or validation in compaction/compression;
- snapshot, summary, observation, or bundle persistence in `ObservationStore`;
- fallback/recovery behavior in `ContextManager` or `ContextCompactionCommitter`;
- context usage/cache reporting fields.

## Verification

- `python scripts/verify_runtime_docs.py`
- `python -m pytest tests/test_context_compression.py tests/test_context_store_production.py tests/test_context_production.py tests/test_context_policy_planner_integration.py --basetemp work/pytest-tmp`

## Last Verified Against

Last verified against commit `5f2202bd8cfcc2a4e4a66c025891550e52f3556e` on 2026-06-25.
