from __future__ import annotations

import pytest
from pydantic import BaseModel, ConfigDict

from singularity.model import ModelCapabilities, ToolChoiceMode, ToolChoicePolicy
from singularity.tool_protocol.errors import ToolProtocolValidationError
from singularity.tool_protocol.models import ToolCallPhase, ToolExecutionMode
from singularity.tool_protocol.validator import ToolProtocolValidator
from singularity.tools import ToolRegistry
from singularity.tools.models import PermissionLevel, ToolSpec


class WriteInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    path: str


def test_validator_builds_readonly_batch_and_schedules_parallel_readonly(tmp_path) -> None:
    validator = ToolProtocolValidator(ToolRegistry(tmp_path))
    assistant_message = {
        "role": "assistant",
        "content": None,
        "tool_calls": [
            {
                "id": "call_1",
                "type": "function",
                "function": {"name": "list_files", "arguments": '{"path": "."}'},
            },
            {
                "id": "call_2",
                "type": "function",
                "function": {"name": "read_file", "arguments": '{"path": "README.md"}'},
            },
        ],
    }

    result = validator.validate_assistant_message(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message=assistant_message,
    )

    assert result.valid is True
    assert result.batch is not None
    assert result.batch.tool_calls[0].phase == ToolCallPhase.VALIDATED
    assert result.batch.tool_calls[0].allowed_tool_names == [
        "list_files",
        "read_file",
        "search_text",
    ]
    plan = validator.schedule(result.batch)
    assert plan.execution_mode == ToolExecutionMode.PARALLEL_READONLY
    assert [[call.tool_call_id for call in group] for group in plan.parallel_groups] == [
        ["call_1", "call_2"]
    ]


def test_validator_schedules_mutation_calls_sequentially(tmp_path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="write_file",
            version="0.0.1",
            description="write",
            input_model=WriteInput,
            handler=lambda args: None,
            permission_level=PermissionLevel.WRITE,
            risk_tags=("write",),
            uses_mutation_runtime=True,
        )
    )
    validator = ToolProtocolValidator(registry)
    assistant_message = {
        "role": "assistant",
        "content": None,
        "tool_calls": [
            {
                "id": "call_1",
                "type": "function",
                "function": {"name": "write_file", "arguments": '{"path": "a.txt"}'},
            },
            {
                "id": "call_2",
                "type": "function",
                "function": {"name": "write_file", "arguments": '{"path": "b.txt"}'},
            },
        ],
    }

    result = validator.validate_assistant_message(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message=assistant_message,
    )

    assert result.valid is True
    assert validator.schedule(result.batch).execution_mode == ToolExecutionMode.SEQUENTIAL


def test_validator_rejects_schema_mismatches(tmp_path) -> None:
    validator = ToolProtocolValidator(ToolRegistry(tmp_path))
    assistant_message = {
        "role": "assistant",
        "content": None,
        "tool_calls": [
            {
                "id": "call_1",
                "type": "function",
                "function": {"name": "read_file", "arguments": '{"path": "README.md", "extra": true}'},
            },
        ],
    }

    result = validator.validate_assistant_message(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message=assistant_message,
    )

    assert result.valid is False
    assert result.batch.tool_calls[0].phase == ToolCallPhase.REJECTED
    assert "schema_mismatch" in result.errors


def test_validator_rejects_missing_tool_call_id(tmp_path) -> None:
    validator = ToolProtocolValidator(ToolRegistry(tmp_path))

    with pytest.raises(ToolProtocolValidationError) as exc_info:
        validator.validate_assistant_message(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            phase_id="phase_1",
            model_request_id="req_1",
            model_response_id="resp_1",
            assistant_message={
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "type": "function",
                        "function": {"name": "list_files", "arguments": "{}"},
                    }
                ],
            },
        )

    assert "missing_tool_call_id" in str(exc_info.value)


def test_validator_raises_on_duplicate_tool_call_id(tmp_path) -> None:
    validator = ToolProtocolValidator(ToolRegistry(tmp_path))
    assistant_message = {
        "role": "assistant",
        "content": None,
        "tool_calls": [
            {
                "id": "call_1",
                "type": "function",
                "function": {"name": "list_files", "arguments": '{"path": "."}'},
            },
            {
                "id": "call_1",
                "type": "function",
                "function": {"name": "read_file", "arguments": '{"path": "README.md"}'},
            },
        ],
    }

    with pytest.raises(ToolProtocolValidationError):
        validator.validate_assistant_message(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            phase_id="phase_1",
            model_request_id="req_1",
            model_response_id="resp_1",
            assistant_message=assistant_message,
        )


def test_validator_rejects_invalid_json_and_arguments_not_object(tmp_path) -> None:
    validator = ToolProtocolValidator(ToolRegistry(tmp_path))
    invalid_json = validator.validate_assistant_message(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message={
            "role": "assistant",
            "content": None,
            "tool_calls": [
                {
                    "id": "call_bad_json",
                    "type": "function",
                    "function": {"name": "list_files", "arguments": "{bad"},
                }
            ],
        },
    )
    array_args = validator.validate_assistant_message(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_2",
        model_response_id="resp_2",
        assistant_message={
            "role": "assistant",
            "content": None,
            "tool_calls": [
                {
                    "id": "call_array",
                    "type": "function",
                    "function": {"name": "list_files", "arguments": "[]"},
                }
            ],
        },
    )

    assert invalid_json.valid is False
    assert "invalid_json" in invalid_json.errors
    assert array_args.valid is False
    assert "arguments_not_object" in array_args.errors


def test_validator_accepts_dict_arguments_and_normalizes_to_json(tmp_path) -> None:
    validator = ToolProtocolValidator(ToolRegistry(tmp_path))

    result = validator.validate_assistant_message(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message={
            "role": "assistant",
            "content": None,
            "tool_calls": [
                {
                    "id": "call_dict",
                    "type": "function",
                    "function": {"name": "list_files", "arguments": {"path": "."}},
                }
            ],
        },
    )

    assert result.valid is True
    assert result.batch is not None
    assert result.batch.tool_calls[0].raw_arguments == '{"path":"."}'
    assert result.batch.tool_calls[0].normalized_arguments == {"path": ".", "max_depth": 4}


def test_validator_enforces_tool_choice_and_allowed_tools(tmp_path) -> None:
    validator = ToolProtocolValidator(ToolRegistry(tmp_path))
    assistant_with_tool = {
        "role": "assistant",
        "content": None,
        "tool_calls": [
            {
                "id": "call_1",
                "type": "function",
                "function": {"name": "read_file", "arguments": '{"path":"README.md"}'},
            }
        ],
    }

    none_result = validator.validate_assistant_message(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message=assistant_with_tool,
        tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.NONE),
    )
    required_result = validator.validate_assistant_message(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_2",
        model_response_id="resp_2",
        assistant_message={"role": "assistant", "content": "no tools"},
        tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.REQUIRED),
    )
    disallowed_result = validator.validate_assistant_message(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_3",
        model_response_id="resp_3",
        assistant_message=assistant_with_tool,
        allowed_tool_names=["list_files"],
        tool_choice=ToolChoicePolicy(
            mode=ToolChoiceMode.ALLOWED_TOOLS,
            allowed_tool_names=["list_files"],
        ),
    )
    specific_result = validator.validate_assistant_message(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_4",
        model_response_id="resp_4",
        assistant_message=assistant_with_tool,
        tool_choice=ToolChoicePolicy(
            mode=ToolChoiceMode.SPECIFIC_TOOL,
            tool_name="list_files",
        ),
    )

    assert none_result.valid is False
    assert "protocol_violation" in none_result.errors
    assert required_result.valid is False
    assert "protocol_violation" in required_result.errors
    assert disallowed_result.valid is False
    assert "disallowed_tool" in disallowed_result.errors
    assert specific_result.valid is False
    assert "disallowed_tool" in specific_result.errors


def test_validator_enforces_max_tool_calls_and_provider_parallel_capability(tmp_path) -> None:
    validator = ToolProtocolValidator(ToolRegistry(tmp_path))
    assistant_message = {
        "role": "assistant",
        "content": None,
        "tool_calls": [
            {
                "id": "call_1",
                "type": "function",
                "function": {"name": "list_files", "arguments": "{}"},
            },
            {
                "id": "call_2",
                "type": "function",
                "function": {"name": "read_file", "arguments": '{"path":"README.md"}'},
            },
        ],
    }

    max_result = validator.validate_assistant_message(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message=assistant_message,
        max_tool_calls=1,
    )
    tool_choice_max_result = validator.validate_assistant_message(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_tool_choice",
        model_response_id="resp_tool_choice",
        assistant_message=assistant_message,
        tool_choice=ToolChoicePolicy(max_tool_calls=1),
    )
    sequential_result = validator.validate_assistant_message(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_2",
        model_response_id="resp_2",
        assistant_message=assistant_message,
        provider_capabilities=ModelCapabilities(supports_parallel_tool_calls=False),
    )

    assert max_result.valid is False
    assert "max_tool_calls_exceeded" in max_result.errors
    assert tool_choice_max_result.valid is False
    assert "max_tool_calls_exceeded" in tool_choice_max_result.errors
    assert sequential_result.valid is True
    assert sequential_result.batch is not None
    assert sequential_result.batch.supports_parallel_execution is False
    assert "provider_parallel_unsupported_forced_sequential" in sequential_result.warnings
    assert validator.schedule(sequential_result.batch).execution_mode == ToolExecutionMode.SEQUENTIAL


def test_validator_rejects_non_list_tool_calls(tmp_path) -> None:
    validator = ToolProtocolValidator(ToolRegistry(tmp_path))

    with pytest.raises(ToolProtocolValidationError) as exc_info:
        validator.validate_assistant_message(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            phase_id="phase_1",
            model_request_id="req_1",
            model_response_id="resp_1",
            assistant_message={"role": "assistant", "content": None, "tool_calls": "bad"},
        )

    assert "tool_calls_must_be_list" in str(exc_info.value)
