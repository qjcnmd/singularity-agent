from __future__ import annotations

from pathlib import Path
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field

from singularity.edit import EditIntent, EditOperation, EditRuntime, EditScope
from singularity.policy import Capability, OperationKind, ResourceRef
from singularity.tools.models import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolExecutionFailure,
    ToolSensitivityLevel,
    ToolSideEffectKind,
    ToolSpec,
)
from singularity.workspace import MutationError, MutationResult


class EditScopeInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    paths: list[str] = Field(default_factory=list)
    exclude_paths: list[str] = Field(default_factory=list)
    expected_hashes: dict[str, str] = Field(default_factory=dict)
    max_files: int = Field(20, ge=1, le=200)
    targeted_max_changed_lines: int = Field(120, ge=1)
    targeted_max_file_change_ratio: float = Field(0.25, gt=0, le=1)
    rewrite_max_changed_lines: int = Field(500, ge=1)
    max_repair_attempts: int = Field(2, ge=0, le=5)
    max_candidates: int = Field(3, ge=1, le=10)
    allow_create: bool = True
    allow_delete: bool = False
    allow_move: bool = False


class EditOperationInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    kind: str
    path: str
    old_text: str | None = None
    new_text: str | None = None
    marker: str | None = None
    text: str | None = None
    start_line: int | None = None
    end_line: int | None = None
    content: str | None = None
    updates: dict[str, Any] | None = None
    symbol_name: str | None = None
    symbol_kind: str | None = None
    import_name: str | None = None
    new_path: str | None = None
    diff: str | None = None
    expected_sha256: str | None = None
    metadata: dict[str, Any] = Field(default_factory=dict)


class EditIntentInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    summary: str = Field("edit workspace", description="Short summary of the intended edit.")
    operations: list[EditOperationInput] = Field(..., min_length=1)
    scope: EditScopeInput = Field(default_factory=EditScopeInput)
    strategy: str | None = None
    actor: str = "agent"
    dry_run: bool = False
    metadata: dict[str, Any] = Field(default_factory=dict)


class WriteFileInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    path: str = Field(..., description="Workspace-relative file path.")
    content: str = Field(..., description="Complete UTF-8 file content to write.")
    mode: Literal["create", "overwrite", "upsert"] = Field(
        "upsert",
        description="Create only, overwrite only, or create/overwrite as needed.",
    )
    encoding: Literal["utf-8"] = Field("utf-8", description="Text encoding. Phase 1B supports utf-8.")
    reason: str | None = Field(None, description="Short reason for the file write.")


class ApplyPatchInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    patch: str = Field(..., min_length=1, description="Unified diff patch text.")
    reason: str | None = Field(None, description="Short reason for applying this patch.")
    expected_files: list[str] | None = Field(
        None,
        description="Optional exact set of files the patch is expected to touch.",
    )
    allow_new_files: bool = Field(True, description="Whether the patch may create new files.")


class InspectDiffInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    scope: Literal["current_run", "workspace", "changeset"] = Field(
        "current_run",
        description="Diff scope to inspect without requiring Git.",
    )
    changeset_id: str | None = Field(None, description="Required when scope is changeset.")
    paths: list[str] | None = Field(None, description="Optional workspace-relative path filter.")


class EditToolHandlers:
    def __init__(self, runtime: EditRuntime) -> None:
        self.runtime = runtime

    def plan(self, args: EditIntentInput) -> dict[str, Any]:
        intent = _intent(args)
        result = self.runtime.plan_intent(intent)
        return {"edit": result.to_dict()}

    def preview(self, args: EditIntentInput) -> dict[str, Any]:
        intent = _intent(args)
        result = self.runtime.preview_intent(intent)
        return {"edit": result.to_dict()}

    def apply(self, args: EditIntentInput) -> dict[str, Any]:
        intent = _intent(args)
        result = (
            self.runtime.preview_intent(intent)
            if args.dry_run
            else self.runtime.apply_intent(intent)
        )
        if not result.ok:
            raise ToolExecutionFailure(
                result.message or "Edit runtime failed.",
                code=result.error_code or "edit_failed",
                details={"edit": result.to_dict()},
            )
        return {"edit": result.to_dict()}

    def write_file(self, args: WriteFileInput) -> dict[str, Any]:
        try:
            result = self.runtime.write_file(
                path=args.path,
                content=args.content,
                mode=args.mode,
                encoding=args.encoding,
                reason=args.reason,
            )
        except MutationError as exc:
            _raise_mutation_failure(exc)
        return _mutation_facade_output(result, self.runtime, tool_name="write_file")

    def apply_patch(self, args: ApplyPatchInput) -> dict[str, Any]:
        try:
            result = self.runtime.apply_unified_diff(
                patch=args.patch,
                reason=args.reason,
                expected_files=args.expected_files,
                allow_new_files=args.allow_new_files,
            )
        except MutationError as exc:
            _raise_mutation_failure(exc)
        if not result.ok:
            _raise_result_failure(result)
        output = _mutation_facade_output(result, self.runtime, tool_name="apply_patch")
        output["conflicts"] = [] if result.ok else [result.message]
        return output

    def inspect_diff(self, args: InspectDiffInput) -> dict[str, Any]:
        try:
            result = self.runtime.mutation_runtime.inspect_diff(
                scope=args.scope,
                changeset_id=args.changeset_id,
                paths=args.paths,
            )
        except MutationError as exc:
            _raise_mutation_failure(exc)
        return {"status": "ok", **result}


def register_edit_tools(registry: Any, runtime: EditRuntime | None = None) -> None:
    edit_runtime = runtime or EditRuntime(Path(registry.project_root))
    handlers = EditToolHandlers(edit_runtime)
    _register_execution_primitive_tools(registry, handlers)
    registry.register(
        ToolSpec(
            name="edit_plan",
            version="0.1.0",
            description="Create an EditPlan and explain the selected edit strategy without applying changes.",
            input_model=EditIntentInput,
            handler=handlers.plan,
            permission_level=PermissionLevel.READ_ONLY,
            capabilities=(Capability.READ_WORKSPACE,),
            operation=OperationKind.SEARCH,
            resource_resolver=_edit_resources,
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            risk_tags=("edit_runtime", "plan", "read_only"),
            timeout_seconds=10.0,
            max_output_chars=16000,
            cacheable=False,
            idempotent=True,
        )
    )
    registry.register(
        ToolSpec(
            name="edit_preview",
            version="0.1.0",
            description="Build, standardize, and validate an edit patch without writing files.",
            input_model=EditIntentInput,
            handler=handlers.preview,
            permission_level=PermissionLevel.READ_ONLY,
            capabilities=(Capability.READ_WORKSPACE,),
            operation=OperationKind.SEARCH,
            resource_resolver=_edit_resources,
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            risk_tags=("edit_runtime", "preview", "read_only"),
            timeout_seconds=15.0,
            max_output_chars=20000,
            cacheable=False,
            idempotent=True,
        )
    )
    registry.register(
        ToolSpec(
            name="edit_apply",
            version="0.1.0",
            description="Apply an edit through EditRuntime, which delegates all writes to MutationRuntime.",
            input_model=EditIntentInput,
            handler=handlers.apply,
            permission_level=PermissionLevel.WRITE,
            capabilities=(Capability.MUTATE_WORKSPACE,),
            operation=OperationKind.MUTATE_FILE,
            resource_resolver=_edit_resources,
            side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_EDIT_RUNTIME,
            risk_tags=("write", "filesystem", "mutation", "edit_runtime"),
            timeout_seconds=20.0,
            max_output_chars=20000,
            cacheable=False,
            idempotent=False,
            uses_edit_runtime=True,
            uses_mutation_runtime=True,
        )
    )


def _register_execution_primitive_tools(registry: Any, handlers: EditToolHandlers) -> None:
    registry.register(
        ToolSpec(
            name="write_file",
            version="0.1.0",
            description="Create, overwrite, or upsert one UTF-8 workspace file through EditRuntime and MutationRuntime.",
            input_model=WriteFileInput,
            handler=handlers.write_file,
            permission_level=PermissionLevel.WRITE,
            capabilities=(Capability.MUTATE_WORKSPACE,),
            operation=OperationKind.MUTATE_FILE,
            resource_resolver=lambda args, _root: [
                ResourceRef("file", args.get("path") or ".", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_EDIT_RUNTIME,
            risk_tags=("write", "filesystem", "mutation", "edit_runtime", "facade"),
            timeout_seconds=20.0,
            max_output_chars=20000,
            cacheable=False,
            idempotent=False,
            uses_edit_runtime=True,
            uses_mutation_runtime=True,
        )
    )
    registry.register(
        ToolSpec(
            name="apply_patch",
            version="0.1.0",
            description="Apply a text unified diff through EditRuntime and MutationRuntime.",
            input_model=ApplyPatchInput,
            handler=handlers.apply_patch,
            permission_level=PermissionLevel.WRITE,
            capabilities=(Capability.MUTATE_WORKSPACE,),
            operation=OperationKind.MUTATE_FILE,
            resource_resolver=_patch_resources,
            side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_EDIT_RUNTIME,
            risk_tags=("write", "filesystem", "mutation", "edit_runtime", "facade", "patch"),
            timeout_seconds=30.0,
            max_output_chars=24000,
            cacheable=False,
            idempotent=False,
            uses_edit_runtime=True,
            uses_mutation_runtime=True,
        )
    )
    registry.register(
        ToolSpec(
            name="inspect_diff",
            version="0.1.0",
            description="Inspect current run or changeset diffs from MutationRuntime evidence without requiring Git.",
            input_model=InspectDiffInput,
            handler=handlers.inspect_diff,
            permission_level=PermissionLevel.READ_ONLY,
            capabilities=(Capability.READ_WORKSPACE,),
            operation=OperationKind.SEARCH,
            resource_resolver=lambda _args, _root: [
                ResourceRef("workspace", "inspect_diff", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            risk_tags=("read", "filesystem", "mutation", "facade", "diff"),
            timeout_seconds=10.0,
            max_output_chars=20000,
            cacheable=False,
            idempotent=True,
        )
    )


def _mutation_facade_output(
    result: MutationResult,
    runtime: EditRuntime,
    *,
    tool_name: str,
) -> dict[str, Any]:
    if not result.ok:
        _raise_result_failure(result)
    classes = (
        runtime.mutation_runtime._changeset_file_classes(result.changeset_id)
        if result.changeset_id
        else {"added_files": set(), "modified_files": set(), "deleted_files": set()}
    )
    observation = result.observation or {}
    changed_files = list(result.affected_files or observation.get("changed_files") or [])
    diff_summary = list(observation.get("diff_summary") or [diff.summary() for diff in result.diffs])
    diff_digest = observation.get("diff_digest") or _digest_from_summary(diff_summary)
    artifact_refs = list(
        observation.get("artifact_refs")
        or _artifact_refs(
            changeset_id=result.changeset_id,
            changed_files=changed_files,
            diff_summary=diff_summary,
        )
    )
    return {
        "status": result.status,
        "tool": tool_name,
        "changed_files": changed_files,
        "created": sorted(classes["added_files"]),
        "overwritten": sorted(classes["modified_files"]),
        "deleted": sorted(classes["deleted_files"]),
        "diff_summary": diff_summary,
        "diff_digest": diff_digest,
        "changeset_id": result.changeset_id,
        "transaction_id": result.transaction_id,
        "artifact_refs": artifact_refs,
        "warnings": list(observation.get("warnings") or []),
    }


def _raise_mutation_failure(exc: MutationError) -> None:
    raise ToolExecutionFailure(
        exc.message,
        code=exc.code,
        details={
            "status": "error",
            "reason": exc.message,
            "changed_files": [],
            "changeset_id": None,
            "diff_digest": None,
            "warnings": [exc.message],
            "error_details": exc.details,
        },
    ) from exc


def _raise_result_failure(result: MutationResult) -> None:
    observation = result.observation or {}
    diff_summary = list(observation.get("diff_summary") or [diff.summary() for diff in result.diffs])
    raise ToolExecutionFailure(
        result.message or "Workspace mutation failed.",
        code=result.error_code or "transaction_failed",
        details={
            "status": "error",
            "reason": result.message,
            "changed_files": result.affected_files,
            "changeset_id": result.changeset_id,
            "transaction_id": result.transaction_id,
            "diff_summary": diff_summary,
            "diff_digest": observation.get("diff_digest") or _digest_from_summary(diff_summary),
            "artifact_refs": observation.get("artifact_refs") or [],
            "warnings": observation.get("warnings") or [result.message],
            "error_code": result.error_code,
            "error_details": observation.get("error_details"),
        },
    )


def _digest_from_summary(diff_summary: list[dict[str, Any]]) -> str:
    import hashlib
    import json

    payload = json.dumps(diff_summary, ensure_ascii=False, sort_keys=True, default=str)
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()


def _artifact_refs(
    *,
    changeset_id: str | None,
    changed_files: list[str],
    diff_summary: list[dict[str, Any]],
) -> list[str]:
    refs = [f"workspace:{path}" for path in sorted(set(changed_files))]
    if changeset_id:
        refs.append(f"changeset:{changeset_id}")
    refs.extend(
        f"diff:{item['diff_digest']}"
        for item in diff_summary
        if item.get("diff_digest")
    )
    refs.extend(
        f"workspace:{item['artifact_path']}"
        for item in diff_summary
        if item.get("artifact_path")
    )
    return refs


def _patch_resources(args: dict[str, Any], _root: Path) -> list[ResourceRef]:
    expected = args.get("expected_files")
    if isinstance(expected, list) and expected:
        return [
            ResourceRef("file", str(path), workspace_relative=True)
            for path in expected
        ]
    return [ResourceRef("workspace", "apply_patch", workspace_relative=True)]


def _intent(args: EditIntentInput) -> EditIntent:
    return EditIntent(
        summary=args.summary,
        operations=[EditOperation(**operation.model_dump(mode="json")) for operation in args.operations],
        scope=EditScope(**args.scope.model_dump(mode="json")),
        strategy=args.strategy,
        actor=args.actor,
        metadata=args.metadata,
    )


def _edit_resources(args: dict[str, Any], _root: Path) -> list[ResourceRef]:
    paths: list[str] = []
    for operation in args.get("operations") or []:
        if isinstance(operation, dict):
            if operation.get("path"):
                paths.append(str(operation["path"]))
            if operation.get("new_path"):
                paths.append(str(operation["new_path"]))
    for path in (args.get("scope") or {}).get("paths") or []:
        paths.append(str(path))
    if not paths:
        return [ResourceRef("workspace", "edit", workspace_relative=True)]
    return [
        ResourceRef("file", path, workspace_relative=True)
        for path in sorted(set(paths))
    ]
