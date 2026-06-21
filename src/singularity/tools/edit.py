from __future__ import annotations

from pathlib import Path
from typing import Any

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


def register_edit_tools(registry: Any, runtime: EditRuntime | None = None) -> None:
    edit_runtime = runtime or EditRuntime(Path(registry.project_root))
    handlers = EditToolHandlers(edit_runtime)
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
