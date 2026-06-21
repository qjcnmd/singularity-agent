from __future__ import annotations

from pydantic import BaseModel

from singularity.tools import ToolRegistry
from singularity.tools.models import PermissionLevel, ToolExecutionBackendKind, ToolSpec
from singularity.tool_protocol.models import ToolCallBatch, ToolCallEnvelope, ToolExecutionMode
from singularity.tool_protocol.scheduler import ToolProtocolScheduler


class EmptyInput(BaseModel):
    pass


def _call(tool_call_id: str, tool_name: str) -> ToolCallEnvelope:
    return ToolCallEnvelope(
        protocol_version="1.0",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message_id="msg_1",
        tool_call_id=tool_call_id,
        tool_name=tool_name,
        raw_arguments="{}",
        parsed_arguments={},
        normalized_arguments={},
    )


def _batch(calls: list[ToolCallEnvelope]) -> ToolCallBatch:
    batch = ToolCallBatch(
        batch_id="batch_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message={"role": "assistant"},
        tool_calls=calls,
    )
    return batch


def test_scheduler_keeps_read_only_calls_and_mutations_sequential() -> None:
    read_call = _call("call_read", "read_file")
    write_call = _call("call_write", "write_file")

    plan = ToolProtocolScheduler().schedule(_batch([read_call, write_call]))

    assert plan.execution_mode == ToolExecutionMode.SEQUENTIAL
    assert plan.parallel_groups == []
    assert [call.tool_call_id for call in plan.ordered_calls] == ["call_read", "call_write"]
    assert plan.ordered_calls[1].tool_call_id == "call_write"


def test_scheduler_preserves_mixed_batch_model_order_except_verification() -> None:
    write_call = _call("call_write", "write_file")
    read_call = _call("call_read", "read_file")

    plan = ToolProtocolScheduler().schedule(_batch([write_call, read_call]))

    assert plan.execution_mode == ToolExecutionMode.SEQUENTIAL
    assert plan.parallel_groups == []
    assert [call.tool_call_id for call in plan.ordered_calls] == ["call_write", "call_read"]


def test_scheduler_runs_multiple_read_only_idempotent_calls_sequentially(tmp_path) -> None:
    registry = ToolRegistry(tmp_path)
    read_one = _call("call_list", "list_files")
    read_two = _call("call_read", "read_file")

    plan = ToolProtocolScheduler(registry).schedule(_batch([read_one, read_two]))

    assert plan.execution_mode == ToolExecutionMode.SEQUENTIAL
    assert plan.parallel_groups == []
    assert [call.tool_call_id for call in plan.ordered_calls] == ["call_list", "call_read"]
    assert "read_only_tools_run_sequentially" in plan.reasons


def test_scheduler_forces_command_and_verification_after_mutation(tmp_path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="write_file",
            description="write",
            input_model=EmptyInput,
            handler=lambda _args: {},
            permission_level=PermissionLevel.WRITE,
            uses_mutation_runtime=True,
        )
    )
    registry.register(
        ToolSpec(
            name="run_command",
            description="run",
            input_model=EmptyInput,
            handler=lambda _args: {},
            permission_level=PermissionLevel.SHELL,
            uses_command_runtime=True,
            execution_backend=ToolExecutionBackendKind.DELEGATED_COMMAND_RUNTIME,
        )
    )
    registry.register(
        ToolSpec(
            name="run_verification",
            description="verify",
            input_model=EmptyInput,
            handler=lambda _args: {},
            permission_level=PermissionLevel.SHELL,
            uses_command_runtime=True,
            execution_backend=ToolExecutionBackendKind.DELEGATED_VERIFICATION_RUNTIME,
            delegates_policy_constraints=True,
        )
    )
    verification = _call("call_verify", "run_verification")
    mutation = _call("call_write", "write_file")
    command = _call("call_cmd", "run_command")

    plan = ToolProtocolScheduler(registry).schedule(_batch([verification, mutation, command]))

    assert plan.execution_mode == ToolExecutionMode.SEQUENTIAL
    assert [call.tool_call_id for call in plan.ordered_calls] == [
        "call_write",
        "call_cmd",
        "call_verify",
    ]
    assert "verification_after_mutation" in plan.reasons
