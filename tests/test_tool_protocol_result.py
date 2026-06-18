from __future__ import annotations

from pathlib import Path

from miniharness.tool_protocol.models import ToolCallEnvelope
from miniharness.tool_protocol.result import ToolProtocolResultBuilder
from miniharness.tools.models import ToolResult


def test_result_builder_redacts_and_references_raw_result(tmp_path) -> None:
    builder = ToolProtocolResultBuilder(tmp_path / "results")
    tool_call = ToolCallEnvelope(
        protocol_version="1.0",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message_id="msg_1",
        tool_call_id="call_1",
        tool_name="read_file",
        raw_arguments="{}",
        parsed_arguments={},
        normalized_arguments={},
    )
    result = ToolResult.success(content={"secret": "value"}, metadata={"artifact_refs": ["artifact_1"]})

    envelope = builder.build(
        tool_call=tool_call,
        result=result,
        redact=True,
        raw_result_ref="raw_1",
    )

    assert envelope.redacted is True
    assert envelope.raw_result_ref == "raw_1"
    assert envelope.content_preview
    assert envelope.content_digest


def test_result_builder_redacts_api_keys_and_keeps_large_output_as_preview(tmp_path) -> None:
    builder = ToolProtocolResultBuilder(tmp_path / "results", max_preview_chars=80)
    tool_call = ToolCallEnvelope(
        protocol_version="1.0",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message_id="msg_1",
        tool_call_id="call_secret",
        tool_name="read_file",
        raw_arguments='{"api_key":"sk-secret-value","text":"x"}',
        parsed_arguments={"api_key": "sk-secret-value", "text": "x"},
        normalized_arguments={"api_key": "sk-secret-value", "text": "x"},
    )
    result = ToolResult.success(
        content={
            "text": "A" * 200,
            "api_key": "sk-secret-value",
            "nested": {"token": "ghp_secretvalue"},
        }
    )

    envelope = builder.build(tool_call=tool_call, result=result)

    assert envelope.truncated is True
    assert envelope.redacted is True
    assert "sk-secret-value" not in envelope.content_preview
    assert "ghp_secretvalue" not in envelope.content_preview
    assert envelope.raw_result_ref is not None
    artifact_text = Path(envelope.raw_result_ref).read_text(encoding="utf-8")
    assert "sk-secret-value" not in artifact_text
    assert "ghp_secretvalue" not in artifact_text
    assert "raw_result" not in envelope.to_context_message()["content"]
