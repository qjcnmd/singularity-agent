# Local Workspace State Runtime

Miniharness targets a local CLI coding agent. Even when Git is unavailable, disabled, or intentionally out of scope, the agent still needs a trustworthy model of the workspace. `LocalWorkspaceStateRuntime` is the non-Git state layer that records what the workspace looked like at session start, which changes belong to the agent, which changes came from commands, and whether a user or external process changed files while the agent was working.

This runtime does not implement branches, commits, staging, push, pull, or pull requests. Git can still be inspected by other read-only paths, but local workspace state and rollback do not depend on Git.

## Runtime Boundary

```txt
CLI session
  -> LocalWorkspaceStateRuntime
  -> WorkspaceBaseline + FileSnapshot scan
  -> WorkspaceStateStore SQLite index
  -> WorkspaceJournal JSONL event stream
  -> ArtifactStore
  -> health, rollback, recovery, context observation, trace
```

`LocalWorkspaceStateRuntime` is the single local state entrypoint. It exposes:

```txt
begin_session, close_session, create_baseline, capture_snapshot,
detect_changes, record_mutation, record_command_side_effects,
record_external_changes, get_workspace_health, prepare_rollback,
apply_rollback, recover_session
```

## Baseline And Snapshot

`begin_session` creates a session record and baseline. The baseline records:

```txt
workspace_root, baseline_id, session_id, task_id, created_at,
policy_version, snapshots
```

Each `FileSnapshot` records:

```txt
path, canonical_path, sha256, size, mtime_ns, file_type,
encoding, line_ending, is_binary, is_symlink, symlink_target,
file_class, permissions, captured_at
```

Path resolution reuses `WorkspacePathResolver`; containment is not checked with plain string prefix matching. The scan skips protected or noisy directories such as `.git`, `.miniharness`, `node_modules`, `venv`, `.venv`, `dist`, `build`, `__pycache__`, test/cache directories, generated outputs, and large artifacts.

## Journal, Store, And Artifacts

State is not memory-only.

- JSONL journal: `.miniharness/sessions/<session_id>/journal.jsonl`
- SQLite query index: `.miniharness/workspace_state.sqlite3`
- Artifacts: `.miniharness/sessions/<session_id>/artifacts/`

Journal events include baseline creation, file snapshot capture, agent mutations, command side effects, created/deleted files, external changes, rollback events, artifacts, session recovery, and session close. Events carry correlation ids when available:

```txt
event_id, session_id, transaction_id, command_id, mutation_id,
event_type, path, before_snapshot, after_snapshot, ownership,
timestamp, metadata
```

Artifacts store full command output, full diffs, verification evidence, rollback backups, workspace scan reports, large observations, and trace exports. Each artifact records:

```txt
artifact_id, kind, path, digest, size, created_at,
linked_command_id, linked_transaction_id, linked_verification_id
```

The `.miniharness` directory is protected by workspace policy and excluded from normal model mutation and state scans.

## Ownership

StateRuntime classifies every recorded change with one ownership value:

```txt
USER_OWNED
AGENT_MUTATION
COMMAND_SIDE_EFFECT
FORMATTER_SIDE_EFFECT
TEST_ARTIFACT
PACKAGE_MANAGER_SIDE_EFFECT
GENERATED_ARTIFACT
UNKNOWN_EXTERNAL
```

`MutationRuntime` records successful model-authored writes as `AGENT_MUTATION`. `CommandRuntime` snapshots before and after command execution, computes file changes, and classifies them from command purpose. Formatter, verification/test, package manager, code generation, and generic command changes therefore remain distinguishable in `CommandResult`, trace, and context observations.

## External Changes

External change detection compares baseline or last-known snapshots with the current scan. If a file changed without a corresponding mutation or command record, the runtime records `external_change_detected` and marks the workspace health as conflicted.

Before a mutation writes, `MutationRuntime` still performs snapshot/hash preflight. Stale `expected_sha256` values and changed files return structured errors such as `snapshot_mismatch`, `file_changed`, or `external_change_detected` instead of overwriting silently.

## Rollback Without Git

Rollback is agent-owned and hash-checked.

`prepare_rollback` only includes files currently owned by `AGENT_MUTATION`, optionally filtered by transaction id. For each file, the runtime stores the agent's before snapshot and a rollback backup artifact.

`apply_rollback` first checks that the current file hash still equals the agent's after-write hash. If the user, formatter, test, package manager, or another process changed the file afterward, rollback returns `rollback_conflict` and does not overwrite it. Created files are removed; modified or deleted files are restored from rollback artifacts.

## Session Recovery

On startup, the CLI asks StateRuntime to inspect the previous session. Recovery status is one of:

```txt
clean
recoverable
needs_user_review
corrupted
```

Recovery detects interrupted sessions, missing baselines, rollback conflicts, and unknown workspace changes. It reloads recoverable state into the runtime but never force-rolls back conflicting files.

## Health And Context

`WorkspaceHealthReport` is a compact structured report for Planner, MutationRuntime, VerificationRuntime, ContextManager, and CLI:

```txt
status: clean / dirty / conflicted / unknown / corrupted
agent_changes
command_side_effects
external_changes
rollback_available
rollback_conflicts
large_artifacts
warnings
recommended_next_action
```

Context integration uses `WorkspaceHealthReport.to_observation()`. Full journals and artifacts stay out of model messages; the model sees a compact observation with status, changed files, side effects, rollback availability, conflicts, and warnings.

The `workspace_health` tool is the internal entrypoint for this observation. It can refresh external changes before reporting health, then returns only the compact `workspace_state` object. `MiniAgent` injects the same observation after non-health tool calls so the next model turn knows whether it must re-read files before continuing. CLI runs print a final workspace state panel after the final answer, but the panel is not merged into the model-authored answer.

## Trace Integration

Every state event can emit a `workspace_state` trace event with:

```txt
session_id, baseline_id, event_id, event_type, path, ownership,
before_sha256, after_sha256, transaction_id, command_id,
mutation_id, artifact_id, timestamp, warning, error_code
```

Mutation, command, verification, context, and trace records can therefore be correlated by `session_id`, `transaction_id`, `command_id`, and `mutation_id` without relying on Git.

## Current Scope

Implemented now:

- Persistent baseline, snapshots, journal, SQLite index, and artifacts.
- Agent mutation recording from `MutationRuntime`.
- Command side-effect recording from `CommandRuntime`.
- External change detection and workspace health.
- Agent-owned rollback with conflict checks.
- Interrupted session recovery.
- Compact workspace state observations, `workspace_health` tool access, CLI state panel, and trace events.

Reserved extension points:

- Richer partial-journal corruption repair.
- More granular incomplete transaction detection.
- Dedicated CLI rollback approval commands.
- Future GitRuntime integration that remains separate from local state.
