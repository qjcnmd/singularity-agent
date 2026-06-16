# Workspace Mutation Runtime

Miniharness v0.0.6 routes agent-owned workspace edits through a mutation runtime instead of a raw `write_file` handler. The goal is to make every file change explicit, reviewable, auditable, reversible, and safe against path escape or stale snapshots.

The runtime is intentionally compact, but it establishes the production boundary:

```txt
ToolRuntime
  -> registered mutation tool
  -> MutationRuntime
  -> ChangeSet
  -> policy gate
  -> MutationTransaction
  -> atomic write
  -> MutationJournal
  -> trace + context observation
```

Tool handlers are not allowed to open, write, remove, or rename files directly. `ToolRuntime` rejects any `WRITE` tool unless its `ToolSpec` declares `uses_mutation_runtime=true`.

## Modules

`src/miniharness/workspace/pathing.py`

`WorkspaceRoot` and `WorkspacePathResolver` own workspace root handling. Every user path is canonicalized before use. The resolver rejects relative traversal, absolute paths outside the workspace, symlink escape, Windows drive escape, UNC escape, and path comparisons that only look like they are inside the root. Containment uses normalized `os.path.commonpath`, not string `startswith`.

`src/miniharness/workspace/policy.py`

`FileClassifier` classifies paths into:

```txt
PUBLIC_SOURCE, PROJECT_CONFIG, TEST, DOCUMENTATION, BUILD_SCRIPT,
DEPENDENCY_LOCK, SECRET, VCS_INTERNAL, GENERATED, BINARY,
LARGE_ARTIFACT, UNKNOWN
```

`WorkspacePolicy` returns a structured `PolicyDecision`:

```txt
allow | require_review | deny
```

The default policy denies secrets, VCS internals, binary files, large artifacts, dependency directories, generated outputs, caches, and virtual environments. Project config, dependency lockfiles, build scripts, deletion, moving, and formatting require review.

`src/miniharness/workspace/snapshot.py`

`FileSnapshot` records:

```txt
path, sha256, size, mtime, encoding, line_ending, is_binary
```

`WorkspaceIndex` snapshots files and checks current hashes before apply and rollback. This detects changes made by the user, IDE, another agent, or a command after the agent read the file.

`src/miniharness/workspace/operations.py`

The operation model includes:

```txt
ReplaceText, InsertBefore, InsertAfter, ReplaceRange, ApplyUnifiedDiff,
CreateFile, DeleteFile, MoveFile, UpdateJson, UpdateYaml, UpdateToml,
FormatFile
```

The runtime implements the safe text operations plus `CreateFile`, `DeleteFile`, `MoveFile`, and `UpdateJson`. Parser-backed operations that are not implemented yet return `invalid_operation` instead of writing through an unsafe fallback.

`src/miniharness/workspace/diff.py`

`DiffEngine` emits structured `FileDiff` and `DiffHunk` objects with added and removed line counts, binary and rename flags, digest, truncation status, and artifact path. Large diffs are truncated for model-facing output and saved under `.miniharness/artifacts/diffs/`.

`src/miniharness/workspace/runtime.py`

`MutationRuntime` builds and applies `ChangeSet` objects. A changeset contains:

```txt
id, base snapshots, operations, affected files, intent, risk level,
created_at, created_by, policy decisions, diffs
```

`MutationJournal` stores before-file artifacts and per-file journal entries before each write. `RollbackManager` can roll back a transaction id without using `git reset`. Rollback checks the current file hash against the transaction's after hash. If the user edited a file after the transaction, rollback returns `rollback_conflict` and does not overwrite that user change.

`src/miniharness/tools/mutation.py`

Registered mutation tools are:

```txt
workspace_replace_text
workspace_create_file
workspace_delete_file
workspace_move_file
```

They return compact observations containing mutation status, changed files, diff summary, risk note, and next recommended action.

## Apply Flow

1. ToolRuntime validates tool arguments with Pydantic.
2. ToolRuntime checks tool policy and rejects unsafe write handlers.
3. The mutation tool builds edit operations and calls `MutationRuntime`.
4. `WorkspacePathResolver` canonicalizes each path and proves it is inside the workspace.
5. `WorkspaceIndex` captures base snapshots.
6. The runtime builds final file content in memory.
7. `DiffEngine` creates structured diffs and artifacts for large diffs.
8. `WorkspacePolicy` returns `allow`, `require_review`, or `deny`.
9. `MutationTransaction` performs preflight hash checks.
10. Each write saves a journal entry and uses temp file, flush, fsync, and `os.replace`.
11. After each write, the runtime records the after snapshot.
12. Trace receives structured `mutation` events.
13. The tool result gives Context Manager a compact observation instead of full files or huge diffs.

If a later write fails during a multi-file transaction, already written files are rolled back from the journal. If rollback detects that the current file is no longer the transaction's after hash, it reports `rollback_conflict`.

## Trace Fields

Mutation trace records include:

```txt
transaction_id, changeset_id, operation_id, tool_call_id, path,
operation_type, policy_decision, risk_tags, before_sha256, after_sha256,
diff_digest, added_lines, removed_lines, dry_run, applied, rejected,
rolled_back, error_code, duration_ms, artifact_path, verification_status
```

## Context Integration

Mutation tools return observation-shaped content:

```json
{
  "mutation_status": "applied",
  "changed_files": ["src/app.py"],
  "diff_summary": [
    {
      "path": "src/app.py",
      "added_lines": 1,
      "removed_lines": 1,
      "diff_digest": "..."
    }
  ],
  "risk_note": "Policy allowed mutation.",
  "next_recommended_action": "Run verification hook or project tests."
}
```

The Context Manager stores the full structured result in SQLite, but only appends a bounded preview to model messages. Large diffs stay as artifacts and are referenced by digest/path.

## Git Awareness

The runtime records git state before and after apply:

```txt
branch, HEAD, dirty files, staged files, untracked files
```

Git is only awareness here. Rollback uses mutation journal entries and file hashes, not `git reset`, so pre-existing user changes are not discarded.

## Verification Hook

`MutationRuntime` accepts a verification hook and carries `verification_status` in results and trace. The current runtime does not hard-code formatting, linting, type checking, testing, or builds. Those belong in a future Verification Runtime.

## Error Taxonomy

The runtime returns structured error codes:

```txt
path_outside_workspace
symlink_escape
path_denied
file_class_denied
file_not_found
file_too_large
binary_file_denied
encoding_error
snapshot_mismatch
file_changed
patch_context_not_found
patch_context_ambiguous
invalid_operation
policy_denied
review_required
preflight_failed
atomic_write_failed
transaction_failed
rollback_failed
rollback_conflict
diff_too_large
internal_error
```

Not every reserved code is emitted by the current compact implementation, but the names are part of the runtime contract and test surface.

## Verification

The test suite covers:

```txt
path traversal rejection
symlink escape rejection when the platform allows symlink creation
SECRET and .git denial
ReplaceText changeset, diff, apply, and trace
snapshot hash mismatch rejection
multi-file transaction rollback after a later write failure
rollback_conflict when the user edits after an agent transaction
large diff truncation with artifact and digest
require_review policy state
mutation observation insertion into Context Manager
ToolRuntime rejection of write handlers that bypass MutationRuntime
registered mutation tool application through ToolRuntime
```

Run:

```powershell
.\.venv\Scripts\python.exe -m pytest tests -q --basetemp work/pytest-tmp
```
