from __future__ import annotations

from uuid import uuid4

from singularity.tool_protocol.models import (
    ToolCallBatch,
    ToolCallEnvelope,
    ToolExecutionMode,
    ToolExecutionPlan,
)
from singularity.tools.models import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolSideEffectKind,
    ToolSpec,
)
from singularity.tools.registry import ToolRegistry


class ToolProtocolScheduler:
    def __init__(self, registry: ToolRegistry | None = None) -> None:
        self.registry = registry

    def schedule(self, batch: ToolCallBatch) -> ToolExecutionPlan:
        readonly_calls: list[ToolCallEnvelope] = []
        mutation_calls: list[ToolCallEnvelope] = []
        command_calls: list[ToolCallEnvelope] = []
        verification_calls: list[ToolCallEnvelope] = []
        other_sequential_calls: list[ToolCallEnvelope] = []
        reasons: list[str] = []
        side_effect_count = 0
        original_order = list(batch.tool_calls)

        for call in original_order:
            spec = self.registry.get(call.tool_name) if self.registry is not None else None
            if _is_readonly(call, spec):
                readonly_calls.append(call)
            elif _is_verification(call, spec):
                verification_calls.append(call)
            elif _is_mutation(call, spec):
                mutation_calls.append(call)
                side_effect_count += 1
            elif _is_command(call, spec):
                command_calls.append(call)
                side_effect_count += 1
            else:
                other_sequential_calls.append(call)
                side_effect_count += 1

        ordered_calls = original_order
        if verification_calls and (mutation_calls or command_calls or other_sequential_calls):
            verification_ids = {call.tool_call_id for call in verification_calls}
            ordered_calls = [
                call for call in original_order if call.tool_call_id not in verification_ids
            ] + [
                call for call in original_order if call.tool_call_id in verification_ids
            ]
            reasons.append("verification_after_mutation")
        parallel_groups = []
        execution_mode = ToolExecutionMode.SEQUENTIAL

        if mutation_calls or command_calls or other_sequential_calls:
            reasons.append("mutation_or_command_tools_run_sequentially")
        elif len(original_order) == 1:
            reasons.append("single_tool_call_runs_sequentially")
        elif readonly_calls:
            if batch.supports_parallel_execution and all(
                _is_parallel_safe_readonly(call, self.registry.get(call.tool_name) if self.registry else None)
                for call in readonly_calls
            ):
                reasons.append("read_only_tools_run_parallel")
                parallel_groups = [readonly_calls]
                execution_mode = ToolExecutionMode.PARALLEL_READONLY
            else:
                reasons.append("read_only_tools_run_sequentially")
        else:
            reasons.append("sequential_tools_run_in_input_order")

        return ToolExecutionPlan(
            plan_id=f"tool_plan_{uuid4().hex[:12]}",
            batch_id=batch.batch_id,
            execution_mode=execution_mode,
            ordered_calls=ordered_calls,
            parallel_groups=parallel_groups,
            blocked_calls=[],
            reasons=reasons,
            requires_approval_count=0,
            side_effect_count=side_effect_count,
        )

    def build_plan(self, batch: ToolCallBatch, **_: object) -> ToolExecutionPlan:
        return self.schedule(batch)


def _is_readonly(call: ToolCallEnvelope, spec: ToolSpec | None) -> bool:
    if spec is None:
        return call.tool_name in {"list_files", "read_file", "search_text", "workspace_health"}
    if spec.permission_level != PermissionLevel.READ_ONLY:
        return False
    if spec.side_effects not in {ToolSideEffectKind.NONE, ToolSideEffectKind.READ_WORKSPACE}:
        return False
    return not (spec.uses_mutation_manager or spec.uses_command_executor)


def _is_parallel_safe_readonly(call: ToolCallEnvelope, spec: ToolSpec | None) -> bool:
    if not _is_readonly(call, spec):
        return False
    if call.validation_errors:
        return False
    if spec is None:
        return True
    return bool(spec.idempotent)


def _is_mutation(call: ToolCallEnvelope, spec: ToolSpec | None) -> bool:
    if spec is None:
        return call.tool_name.startswith("workspace_")
    return (
        spec.permission_level == PermissionLevel.WRITE
        or spec.side_effects == ToolSideEffectKind.MUTATE_WORKSPACE
        or spec.uses_mutation_manager
    )


def _is_command(call: ToolCallEnvelope, spec: ToolSpec | None) -> bool:
    if spec is None:
        return call.tool_name in {"run_command", "start_process", "stop_process"}
    return (
        spec.permission_level in {PermissionLevel.SHELL, PermissionLevel.GIT}
        or spec.side_effects == ToolSideEffectKind.EXECUTE_COMMAND
        or spec.uses_command_executor
    )


def _is_verification(call: ToolCallEnvelope, spec: ToolSpec | None) -> bool:
    if "verification" in call.tool_name or call.tool_name in {"run_verification", "rerun_check"}:
        return True
    if spec is None:
        return False
    return spec.execution_backend == ToolExecutionBackendKind.DELEGATED_VERIFICATION_RUNNER


ToolCallScheduler = ToolProtocolScheduler
