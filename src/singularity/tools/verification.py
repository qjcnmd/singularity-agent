from __future__ import annotations

from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict, Field

from singularity.policy import Capability, OperationKind, ResourceRef
from singularity.command import CommandRuntime
from singularity.tools.models import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolExecutionFailure,
    ToolSensitivityLevel,
    ToolSideEffectKind,
    ToolSpec,
)
from singularity.verification import VerificationRuntime


class PlanVerificationInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    changed_files: list[str] = Field(default_factory=list)
    task_intent: str = ""
    smoke_commands: list[list[str]] = Field(default_factory=list)
    transaction_id: str | None = None
    changeset_id: str | None = None


class RunVerificationInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    plan_id: str | None = None
    changed_files: list[str] = Field(default_factory=list)
    task_intent: str = ""
    smoke_commands: list[list[str]] = Field(default_factory=list)
    transaction_id: str | None = None
    changeset_id: str | None = None


class GetVerificationResultInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    plan_id: str | None = None


class RerunCheckInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    plan_id: str
    check_id: str


class VerificationToolHandlers:
    def __init__(self, runtime: VerificationRuntime) -> None:
        self.runtime = runtime

    def plan_verification(self, args: PlanVerificationInput) -> dict[str, Any]:
        plan = self.runtime.plan_verification(
            changed_files=args.changed_files,
            task_intent=args.task_intent,
            smoke_commands=args.smoke_commands,
            transaction_id=args.transaction_id,
            changeset_id=args.changeset_id,
        )
        return {"verification_plan": plan.to_dict()}

    def run_verification(self, args: RunVerificationInput) -> dict[str, Any]:
        plan_id = args.plan_id
        if plan_id is None:
            plan = self.runtime.plan_verification(
                changed_files=args.changed_files,
                task_intent=args.task_intent,
                smoke_commands=args.smoke_commands,
                transaction_id=args.transaction_id,
                changeset_id=args.changeset_id,
            )
            plan_id = plan.id
        return self.runtime.run_plan(plan_id)

    def get_verification_result(self, args: GetVerificationResultInput) -> dict[str, Any]:
        try:
            return self.runtime.get_result(args.plan_id)
        except KeyError as exc:
            raise ToolExecutionFailure(
                str(exc),
                code="verification_plan_failed",
                details={"plan_id": args.plan_id},
            ) from exc

    def rerun_check(self, args: RerunCheckInput) -> dict[str, Any]:
        try:
            return self.runtime.rerun_check(plan_id=args.plan_id, check_id=args.check_id)
        except KeyError as exc:
            raise ToolExecutionFailure(
                str(exc),
                code="check_blocked",
                details={"plan_id": args.plan_id, "check_id": args.check_id},
            ) from exc


def register_verification_tools(
    registry: Any,
    runtime: VerificationRuntime | None = None,
) -> None:
    verification_runtime = runtime or VerificationRuntime(
        Path(registry.project_root),
        command_runtime=CommandRuntime(Path(registry.project_root)),
    )
    handlers = VerificationToolHandlers(verification_runtime)
    registry.register(
        ToolSpec(
            name="plan_verification",
            version="0.0.8",
            description="Plan project verification through VerificationRuntime without executing commands.",
            input_model=PlanVerificationInput,
            handler=handlers.plan_verification,
            permission_level=PermissionLevel.READ_ONLY,
            capabilities=(Capability.READ_WORKSPACE,),
            operation=OperationKind.READ_FILE,
            resource_resolver=lambda _args, _root: [
                ResourceRef("workspace", "plan_verification", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            risk_tags=("verification_runtime", "planning"),
            timeout_seconds=10.0,
            max_output_chars=16000,
            cacheable=False,
            idempotent=True,
        )
    )
    registry.register(
        ToolSpec(
            name="run_verification",
            version="0.0.8",
            description="Run a VerificationRuntime plan; all commands execute through CommandRuntime.",
            input_model=RunVerificationInput,
            handler=handlers.run_verification,
            permission_level=PermissionLevel.SHELL,
            capabilities=(Capability.EXECUTE_PROJECT_CODE,),
            operation=OperationKind.VERIFICATION,
            resource_resolver=lambda _args, _root: [
                ResourceRef("workspace", "run_verification", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.EXECUTE_COMMAND,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_VERIFICATION_RUNTIME,
            risk_tags=("verification_runtime", "command_runtime"),
            timeout_seconds=300.0,
            max_output_chars=20000,
            cacheable=False,
            idempotent=False,
            uses_command_runtime=True,
            delegates_policy_constraints=True,
        )
    )
    registry.register(
        ToolSpec(
            name="get_verification_result",
            version="0.0.8",
            description="Return the latest structured VerificationRuntime result or a specific plan result.",
            input_model=GetVerificationResultInput,
            handler=handlers.get_verification_result,
            permission_level=PermissionLevel.READ_ONLY,
            capabilities=(Capability.READ_WORKSPACE,),
            operation=OperationKind.READ_FILE,
            resource_resolver=lambda _args, _root: [
                ResourceRef("workspace", "get_verification_result", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            risk_tags=("verification_runtime",),
            timeout_seconds=5.0,
            max_output_chars=16000,
            cacheable=False,
            idempotent=True,
        )
    )
    registry.register(
        ToolSpec(
            name="rerun_check",
            version="0.0.8",
            description="Rerun one VerificationRuntime check through CommandRuntime.",
            input_model=RerunCheckInput,
            handler=handlers.rerun_check,
            permission_level=PermissionLevel.SHELL,
            capabilities=(Capability.EXECUTE_PROJECT_CODE,),
            operation=OperationKind.VERIFICATION,
            resource_resolver=lambda _args, _root: [
                ResourceRef("workspace", "rerun_check", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.EXECUTE_COMMAND,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_VERIFICATION_RUNTIME,
            risk_tags=("verification_runtime", "command_runtime"),
            timeout_seconds=180.0,
            max_output_chars=16000,
            cacheable=False,
            idempotent=False,
            uses_command_runtime=True,
            delegates_policy_constraints=True,
        )
    )
