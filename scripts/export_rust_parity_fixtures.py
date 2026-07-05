#!/usr/bin/env python3
"""Export stable Python oracle fixtures for the Rust migration boundary."""

from __future__ import annotations

import argparse
import json
from datetime import UTC, datetime
from pathlib import Path

from singularity.command.models import (
    CommandDecision,
    CommandPolicyResult,
    CommandRequest,
    CommandResult,
    CommandRisk,
    ExecutionStatus,
    SemanticStatus,
)
from singularity.model.models import (
    ContentBlock,
    ModelMessage,
    ModelRole,
    ModelTurnRequest,
    ModelTurnResult,
    ModelTurnStatus,
)
from singularity.observability.models import TraceEvent, TraceEventType, TraceSeverity
from singularity.policy.models import ApprovalGrant, ApprovalScope, Capability
from singularity.policy.permissions import PermissionProfile, PermissionProfileName
from singularity.sandbox.models import SandboxProfileName, default_sandbox_profile
from singularity.tool_protocol.models import ToolProtocolResultEnvelope


def build_fixtures() -> dict[str, object]:
    command_policy = CommandPolicyResult(
        decision=CommandDecision.ALLOW,
        reasons=["parity fixture"],
        risk_tags=[CommandRisk.READ_ONLY_COMMAND],
    )
    command_request = CommandRequest(
        argv=["python", "-m", "pytest"],
        cwd=".",
        purpose="PROJECT_VERIFICATION",
        command_id="command_1",
    )
    command_result = CommandResult(
        command_id=command_request.command_id,
        execution_status=ExecutionStatus.COMPLETED,
        semantic_status=SemanticStatus.SUCCEEDED,
        exit_code=0,
        signal=None,
        duration_ms=10,
        timed_out=False,
        idle_timed_out=False,
        stdout_preview="passed",
        stderr_preview="",
        combined_output_preview="passed",
        output_truncated=False,
        output_digest="digest",
        artifact_path=None,
        changed_files=[],
        policy_decision=command_policy,
        risk_tags=[CommandRisk.READ_ONLY_COMMAND],
        error_code=None,
        isolation_report={"sandbox_backend": "windows_unelevated"},
    )
    tool_result = ToolProtocolResultEnvelope(
        tool_call_id="call_1",
        tool_name="read_file",
        ok=True,
        status="ok",
        content_preview="safe preview",
        content_digest="digest_1",
        raw_result_ref="artifact_1",
        artifact_refs=["artifact_1"],
        observation_id="obs_1",
        policy_decision_id="policy_1",
        approval_grant_id="grant_1",
        metadata={"raw_arguments": {"path": ".env"}},
    )
    trace_event = TraceEvent(
        event_id="event_1",
        event_type=TraceEventType.TOOL_PROTOCOL_CALL_COMPLETED,
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        action_id="action_1",
        parent_event_id=None,
        timestamp=datetime(2026, 1, 1, tzinfo=UTC),
        monotonic_ms=1,
        component="tool_protocol",
        severity=TraceSeverity.INFO,
        summary="Tool completed.",
        payload={"tool_call_id": "call_1"},
    )
    profile = PermissionProfile.default_for_workspace(
        "C:/repo",
        profile=PermissionProfileName.WORKSPACE_WRITE,
    )
    approval = ApprovalGrant(
        decision_id="decision_1",
        request_id="request_1",
        approved_by="operator",
        session_id="session_1",
        approved_at="2026-01-01T00:00:00+00:00",
        grant_id="grant_1",
        scope=ApprovalScope(
            capabilities=[Capability.EXECUTE_COMMAND],
            command_patterns=["pytest"],
            session_only=True,
            single_use=True,
        ),
        reason="fixture approval",
    )
    model_request = ModelTurnRequest(
        request_id="model_req_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="model",
        action_id="action_1",
        purpose="plan_next_action",
        messages=[
            ModelMessage(role=ModelRole.USER, content=[ContentBlock.from_text("hello")])
        ],
    )
    model_response = ModelTurnResult(
        request_id=model_request.request_id,
        response_id="response_1",
        status=ModelTurnStatus.SUCCESS,
        assistant_message=ModelMessage.assistant_text("done"),
    )
    sandbox_policy = default_sandbox_profile(
        SandboxProfileName.ISOLATED_VERIFICATION,
        workspace_root=Path("C:/repo"),
    )
    return {
        "tool_observation_model_payload": tool_result.to_observation_view().to_model_payload(),
        "tool_protocol_result_envelope": tool_result.to_dict(),
        "trace_event": trace_event.to_dict(),
        "command_request": command_request.safe_metadata(),
        "command_result": command_result.to_dict(),
        "permission_profile": profile.summary().to_dict(),
        "approval_grant": approval.to_dict(),
        "sandbox_policy": sandbox_policy.to_dict(),
        "model_turn_request": model_request.to_dict(),
        "model_turn_response": model_response.to_dict(),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--out",
        default="tests/fixtures/rust_parity/python_oracle.json",
        help="Output JSON fixture path.",
    )
    args = parser.parse_args()
    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(
        json.dumps(build_fixtures(), ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print(out_path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
