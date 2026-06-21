from __future__ import annotations

from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict, Field

from singularity.policy import Capability, OperationKind, ResourceRef
from singularity.tools.models import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolSideEffectKind,
    ToolSensitivityLevel,
    ToolExecutionFailure,
    ToolSpec,
)
from singularity.workspace import CreateFile, DeleteFile, MoveFile, MutationRuntime, ReplaceText


class ReplaceTextInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    path: str = Field(..., description="Workspace-relative file path.")
    old_text: str = Field(..., min_length=1, description="Text to replace exactly once.")
    new_text: str = Field(..., description="Replacement text.")
    intent: str = Field("edit file", description="Short reason for the mutation.")
    expected_sha256: str | None = Field(
        None,
        description="Optional file hash read earlier by the agent.",
    )
    dry_run: bool = Field(False, description="Preview without writing.")


class CreateFileInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    path: str
    content: str
    intent: str = "create file"
    dry_run: bool = False


class DeleteFileInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    path: str
    intent: str = "delete file"
    expected_sha256: str | None = None
    dry_run: bool = False


class MoveFileInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    path: str
    new_path: str
    intent: str = "move file"
    expected_sha256: str | None = None
    dry_run: bool = False


class MutationToolHandlers:
    def __init__(self, runtime: MutationRuntime) -> None:
        self.runtime = runtime

    def replace_text(self, args: ReplaceTextInput) -> dict[str, Any]:
        result = self._run(
            [
                ReplaceText(
                    path=args.path,
                    old_text=args.old_text,
                    new_text=args.new_text,
                    expected_sha256=args.expected_sha256,
                )
            ],
            intent=args.intent,
            dry_run=args.dry_run,
        )
        return result.observation

    def create_file(self, args: CreateFileInput) -> dict[str, Any]:
        result = self._run(
            [CreateFile(path=args.path, content=args.content)],
            intent=args.intent,
            dry_run=args.dry_run,
        )
        return result.observation

    def delete_file(self, args: DeleteFileInput) -> dict[str, Any]:
        result = self._run(
            [
                DeleteFile(
                    path=args.path,
                    expected_sha256=args.expected_sha256,
                )
            ],
            intent=args.intent,
            dry_run=args.dry_run,
        )
        return result.observation

    def move_file(self, args: MoveFileInput) -> dict[str, Any]:
        result = self._run(
            [
                MoveFile(
                    path=args.path,
                    new_path=args.new_path,
                    expected_sha256=args.expected_sha256,
                )
            ],
            intent=args.intent,
            dry_run=args.dry_run,
        )
        return result.observation

    def _run(self, operations: list[Any], *, intent: str, dry_run: bool) -> Any:
        result = (
            self.runtime.preview_operations(
                operations,
                intent=intent,
                created_by="tool:workspace_mutation",
            )
            if dry_run
            else self.runtime.apply_operations(
                operations,
                intent=intent,
                created_by="tool:workspace_mutation",
            )
        )
        if not result.ok:
            raise ToolExecutionFailure(
                result.message or "Workspace mutation failed.",
                code=result.error_code or "transaction_failed",
                details=result.observation,
            )
        return result


def register_mutation_tools(
    registry: Any,
    runtime: MutationRuntime | None = None,
) -> None:
    mutation_runtime = runtime or MutationRuntime(Path(registry.project_root))
    handlers = MutationToolHandlers(mutation_runtime)
    registry.register(
        ToolSpec(
            name="workspace_replace_text",
            version="0.0.6",
            description=(
                "Replace exact text in a workspace file via ChangeSet, policy, "
                "snapshot, atomic write, journal, diff, rollback, and trace."
            ),
            input_model=ReplaceTextInput,
            handler=handlers.replace_text,
            permission_level=PermissionLevel.WRITE,
            capabilities=(Capability.MUTATE_WORKSPACE,),
            operation=OperationKind.MUTATE_FILE,
            resource_resolver=lambda args, _root: [
                ResourceRef("file", args.get("path") or ".", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_MUTATION_RUNTIME,
            risk_tags=("write", "filesystem", "mutation"),
            timeout_seconds=10.0,
            max_output_chars=12000,
            cacheable=False,
            idempotent=False,
            uses_mutation_runtime=True,
        )
    )
    registry.register(
        ToolSpec(
            name="workspace_create_file",
            version="0.0.6",
            description="Create a workspace file via Workspace Mutation Runtime.",
            input_model=CreateFileInput,
            handler=handlers.create_file,
            permission_level=PermissionLevel.WRITE,
            capabilities=(Capability.CREATE_FILE,),
            operation=OperationKind.CREATE_FILE,
            resource_resolver=lambda args, _root: [
                ResourceRef("file", args.get("path") or ".", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_MUTATION_RUNTIME,
            risk_tags=("write", "filesystem", "mutation", "create"),
            timeout_seconds=10.0,
            max_output_chars=12000,
            cacheable=False,
            idempotent=False,
            uses_mutation_runtime=True,
        )
    )
    registry.register(
        ToolSpec(
            name="workspace_delete_file",
            version="0.0.6",
            description="Delete a workspace file via Workspace Mutation Runtime.",
            input_model=DeleteFileInput,
            handler=handlers.delete_file,
            permission_level=PermissionLevel.WRITE,
            capabilities=(Capability.DELETE_FILE,),
            operation=OperationKind.DELETE_FILE,
            resource_resolver=lambda args, _root: [
                ResourceRef("file", args.get("path") or ".", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_MUTATION_RUNTIME,
            risk_tags=("write", "filesystem", "mutation", "delete"),
            timeout_seconds=10.0,
            max_output_chars=12000,
            cacheable=False,
            idempotent=False,
            uses_mutation_runtime=True,
        )
    )
    registry.register(
        ToolSpec(
            name="workspace_move_file",
            version="0.0.6",
            description="Move a workspace file via Workspace Mutation Runtime.",
            input_model=MoveFileInput,
            handler=handlers.move_file,
            permission_level=PermissionLevel.WRITE,
            capabilities=(Capability.MOVE_FILE,),
            operation=OperationKind.MUTATE_FILE,
            resource_resolver=lambda args, _root: [
                ResourceRef("file", args.get("path") or ".", workspace_relative=True),
                ResourceRef("file", args.get("new_path") or ".", workspace_relative=True),
            ],
            side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_MUTATION_RUNTIME,
            risk_tags=("write", "filesystem", "mutation", "move"),
            timeout_seconds=10.0,
            max_output_chars=12000,
            cacheable=False,
            idempotent=False,
            uses_mutation_runtime=True,
        )
    )
