# Phase 1B Execution Primitives

Phase 1B narrows model-facing execution to stable facades:

- `write_file`
- `apply_patch`
- `inspect_diff`
- `run_verification` for task-specific smoke commands

The facades are model-visible tools. They are not alternate runtimes and they do not write files directly.

## Tool Schemas

`write_file`

- `path: str`
- `content: str`
- `mode: "create" | "overwrite" | "upsert"`
- `encoding: "utf-8"`
- `reason: str | None`

`apply_patch`

- `patch: str`
- `reason: str | None`
- `expected_files: list[str] | None`
- `allow_new_files: bool`

`inspect_diff`

- `scope: "current_run" | "workspace" | "changeset"`
- `changeset_id: str | None`
- `paths: list[str] | None`

## Runtime Delegation

`write_file` and `apply_patch` enter through `ToolRuntime`, which still performs schema validation, `PolicyRuntime` checks, optional `ApprovalGate` handling, planner authorization, trace, and audit recording.

After tool dispatch:

```text
ToolRuntime -> EditRuntime facade method -> MutationRuntime -> WorkspacePathResolver -> AtomicWriter
```

`apply_patch` parses text unified diffs before creating a changeset. It supports text file creation and modification. Delete, rename, and binary patches are rejected as `unsupported_operation`. Context mismatch and stale snapshots fail before any file is written.

`inspect_diff` reads the in-process `MutationRuntime` changeset ledger and bounded diff evidence. It does not require Git.

## Low-Level Tools

Low-level mutation tools such as `workspace_create_file`, `workspace_replace_text`, and `edit_apply` remain registered for compatibility and internal coverage. They are not the recommended stable model entrypoints and are hidden from default planner phases.

## Rollback

`rollback_changeset(changeset_id, reason=None)` is an internal controller recovery API on `MutationRuntime`. It is not registered as a model tool.

Rollback uses the existing mutation journal, records policy audit and mutation trace entries, and updates planner workspace evidence when a planner is attached.

## Verification

Task-specific smoke commands still run through:

```text
run_verification -> VerificationRuntime -> CommandRuntime
```

Verification evidence includes command exit code, stdout/stderr excerpts, duration, parsed failures, check status, and completion assessment. Completion remains rejected unless verification evidence is ready or ready with warnings.
