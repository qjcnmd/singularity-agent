# Singularity Patch / Edit Strategy Component

`EditExecutor` sits between `Planner` and `WorkspaceMutationManager`. Model-facing write intent should enter through the Phase 1B facades `write_file` and `apply_patch`; the edit layer lowers those requests into existing workspace mutation operations, validates the patch, applies it through `WorkspaceMutationManager`, and records bounded trace/context evidence. The older `edit_apply` tool remains registered for compatibility and internal tests, but it is no longer the default model-visible write entrypoint.

It does not implement Git behavior. Branches, commits, PRs, remote collaboration, and direct filesystem writes stay outside this component.

## Boundary

Compatibility edit-user data path:

```text
Planner -> edit_plan/edit_preview/edit_apply -> EditExecutor -> WorkspaceMutationManager
```

`WorkspaceMutationManager` remains the only file-writing component. `VerificationRunner` remains the only verification planner/runner. After a successful edit, `EditExecutor` asks `VerificationRunner.plan_verification(...)` for a plan only; it does not run verification commands.

Default model-facing apply path:

```text
Planner -> write_file/apply_patch -> EditExecutor -> WorkspaceMutationManager -> WorkspacePathResolver -> AtomicWriter
```

Low-level `workspace_*` tools and `edit_apply` remain registered for compatibility and internal tests, but Planner default phases expose the Phase 1B facades instead.

See also: `docs/architecture/modules/tool-execution-runtime.md`.

## Strategies

`targeted_patch` is selected for localized text, marker, and line-range edits. It lowers into `ReplaceText`, `InsertBefore`, `InsertAfter`, or `ReplaceRange`.

`full_file_rewrite` is selected for file creation or whole-file replacement. Existing files become a whole-file `ReplaceText`; missing files become `CreateFile`.

`structured_edit` is selected for JSON updates or Python symbol/import edits. JSON lowers to `UpdateJson`. Python uses stdlib `ast` to locate the target and then lowers to `ReplaceRange`. TOML is parsed during validation for syntax risk but does not have a TOML writer.

## Validation

Validation happens before apply and covers:

- path scope and excluded paths
- expected hash freshness
- WorkspaceMutationManager policy decisions for forbidden and review-required paths
- unique text/context matching through changeset creation
- diff size and over-modification thresholds
- Python, JSON, and TOML syntax risk
- format warnings such as missing final newline
- CodeIndex impact and affected test mapping

Defaults are conservative: 20 files per edit, targeted patch max 120 changed lines or 25% of a target file, full-file rewrite review above 500 changed lines.

## Repair

The repair loop is bounded by `EditScope.max_repair_attempts` and `EditScope.max_candidates`. Only recoverable categories are retried:

- stale snapshot or file freshness mismatch by refreshing expected hashes
- context mismatch by falling back to line-range data when the original intent contains it

Forbidden paths, policy denial, review-required patches, diff-budget failures, syntax risk, and over-modification are not auto-repaired.

## Trace And Context

Edit trace events are:

- `edit.plan_created`
- `edit.patch_validated`
- `edit.applied`
- `edit.repair_attempted`
- `edit.failed`

Trace and context evidence are bounded to ids, strategy, changed file paths, diff digests, validation issue codes, repair summaries, changeset/transaction ids, and verification plan ids. Large file contents and full diffs stay in WorkspaceMutationManager artifacts.
