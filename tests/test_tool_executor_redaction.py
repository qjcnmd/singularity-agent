import json
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict, Field

from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.tools import ToolExecutor, ToolPolicy, ToolRegistry, ToolSpec
from tests.tool_executor_helpers import make_test_policy_engine


class SecretInput(BaseModel):
    model_config = ConfigDict(extra="forbid")
    api_key: str
    authorization: str | None = None


def make_tool_call(name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": "call_redact",
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }


def test_trace_redacts_secret_arguments_and_errors(tmp_path: Path) -> None:
    trace = JsonlTraceRecorder.create(tmp_path)

    def handler(args: SecretInput) -> dict[str, str]:
        raise RuntimeError(f"failed with token {args.api_key}")

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="secret_args",
            description="secret args",
            input_model=SecretInput,
            handler=handler,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=trace,
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(
        make_tool_call(
            "secret_args",
            {"api_key": "sk-secret-value", "authorization": "Bearer token-value"},
        )
    )

    assert result.ok is False
    dumped_result = json.dumps(result.model_dump(mode="json"), ensure_ascii=False)
    assert "sk-secret-value" not in dumped_result
    assert "token-value" not in dumped_result
    trace_text = trace.path.read_text(encoding="utf-8")
    assert "sk-secret-value" not in trace_text
    assert "token-value" not in trace_text
    assert "validated_args" not in trace_text


class LongInput(BaseModel):
    model_config = ConfigDict(extra="forbid")
    value: str = Field("x")


def test_oversized_output_has_digest_and_truncation_metadata(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="long",
            description="long",
            input_model=LongInput,
            handler=lambda _args: "A" * 100 + "Z" * 100,
            max_output_chars=80,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(make_tool_call("long", {}))

    assert result.ok is True
    assert result.truncated is True
    assert result.metadata["output_digest"]
    assert result.metadata["original_chars"] == 200
    assert result.metadata["returned_chars"] <= 80
    assert result.content.startswith("A")
    assert result.content.endswith("Z")

