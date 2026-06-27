from __future__ import annotations

import json

from singularity.tool_protocol.models import ToolCallEnvelope
from singularity.tool_protocol.result import ToolProtocolResultBuilder
from singularity.tools.models import ToolResult


def test_policy_failure_model_payload_omits_internal_authorization_objects() -> None:
    result = ToolResult.failure(
        code="approval_required",
        message="This action requires approval.",
        details={
            "policy": {
                "decision_id": "policy_dec_secret",
                "constraints": {"sandbox_required": True},
                "required_approval": {"scope": {"path_globs": [".git/**"]}},
            },
            "request": {"request_id": "policy_req_secret"},
        },
        metadata={"approval_grant_id": "grant_secret"},
    )
    envelope = ToolProtocolResultBuilder().build(
        tool_call=ToolCallEnvelope(
            protocol_version="1.0",
            run_id="run",
            session_id="session",
            task_id="task",
            phase_id="phase",
            model_request_id="request",
            model_response_id="response",
            assistant_message_id="assistant",
            tool_call_id="call_permission",
            tool_name="run_command",
            raw_arguments="{}",
            parsed_arguments={},
            normalized_arguments={},
        ),
        result=result,
        policy_decision_id="policy_dec_secret",
        approval_grant_id="grant_secret",
    )

    payload = envelope.to_observation_view().to_model_payload()
    serialized = json.dumps(payload, ensure_ascii=False, sort_keys=True)

    assert payload["error_code"] == "approval_required"
    assert "This action requires approval." in serialized
    for internal_name in (
        "decision_id",
        "request_id",
        "constraints",
        "required_approval",
        "approval_grant_id",
        "path_globs",
        "policy_dec_secret",
        "grant_secret",
    ):
        assert internal_name not in serialized
