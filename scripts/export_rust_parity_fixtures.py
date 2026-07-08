#!/usr/bin/env python3
"""Export stable Python oracle fixtures for the Rust migration boundary."""

from __future__ import annotations

import argparse
import json
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from singularity.agent_loop import AgentLoopResult, AgentLoopStatus
from singularity.command.models import (
    CommandDecision,
    CommandPolicyResult,
    CommandRequest,
    CommandResult,
    CommandRisk,
    ExecutionStatus,
    SemanticStatus,
)
from singularity.context.models import (
    CacheAttribution,
    CacheAttributionSource,
    ContextBudgetPlan,
    ContextBundle,
    ContextSummaryEnvelope,
    ContextSummaryPayload,
    PlannerState,
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
from singularity.planner.models import FinalReport, TaskStatus
from singularity.policy.models import ApprovalGrant, ApprovalScope, Capability
from singularity.policy.permissions import PermissionProfile, PermissionProfileName
from singularity.repair.contract import RepairActionCandidate, RepairContract
from singularity.sandbox.models import SandboxProfile, SandboxProfileName, default_sandbox_profile
from singularity.tool_protocol.models import (
    ToolProtocolRecoveryReport,
    ToolProtocolResultEnvelope,
)
from singularity.verification.contract import VerificationContract

FIXTURE_WORKSPACE_ROOT = "/workspace/repo"


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
    failed_tool_result = ToolProtocolResultEnvelope(
        tool_call_id="call_failed_1",
        tool_name="run_verification",
        ok=False,
        status="failed",
        error_code="tool_executor_failed",
        error_kind="tool_executor_failed",
        content_preview="pytest failed",
        content_digest="digest_failed_1",
        raw_result_ref="artifact_failed_1",
        artifact_refs=["artifact_failed_1"],
        observation_id="obs_failed_1",
        truncated=True,
        redacted=True,
        metadata={"exit_code": 1},
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
    planner_state = PlannerState(
        task_id="task_1",
        current_phase="running_verification",
        status="repairing_failures",
        current_plan=["run verification"],
        completion_criteria={"required_verifications_passed": False},
        open_actions=["repair failed test"],
        blocked_actions=[],
        risk_escalations=[],
        evidence_refs=["obs_1"],
    )
    context_bundle = ContextBundle(
        bundle_id="bundle_1",
        run_id="run_1",
        task_id="task_1",
        phase_id="running_verification",
        model="mimo-v2.5",
        provider="openai-compatible",
        messages=[
            {"role": "system", "content": "system"},
            {"role": "user", "content": "fix tests"},
        ],
        included_item_ids=["item_goal", "item_plan"],
        excluded_item_ids=["item_raw_tool"],
        budget=ContextBudgetPlan(
            model_context_window=128000,
            output_token_reserve=4096,
            reasoning_token_reserve=0,
            tool_schema_tokens=12,
            system_tokens=5,
            pinned_tokens=10,
            evidence_tokens=20,
            recent_dialogue_tokens=8,
            summary_tokens=7,
            message_tokens=62,
        ),
        compression_snapshot_id="snapshot_1",
        retrieval_query=None,
        created_at="2026-01-01T00:00:00+00:00",
        bundle_digest="bundle_digest_1",
        metadata={"source": "python_oracle"},
    )
    context_summary = ContextSummaryPayload(
        goal="fix tests",
        current_state="Context compacted after verification.",
        completed_actions=["ran public verification"],
        pending_actions=[],
        verified_facts=[
            {
                "fact": "public verification passed",
                "reference_ids": ["obs_1"],
            }
        ],
        failed_attempts=[],
        policy_constraints=["do not expose raw tool output"],
        workspace_changes=[],
        verification_status="passed",
        open_questions=[],
        reference_ids=["obs_1"],
        omitted_item_ids=["item_raw_tool"],
        confidence=0.91,
    )
    context_summary_envelope = ContextSummaryEnvelope(
        version=1,
        summary_id="summary_1",
        summary_payload=context_summary,
        source_item_ids=["item_raw_tool"],
        cache_attribution=CacheAttribution(
            source=CacheAttributionSource.COMPONENT_INFERRED,
            confidence=1.0,
            reasons=["deterministic compaction fixture"],
            evidence=["obs_1"],
        ),
        previous_summary_digest=None,
        rendered_summary="Context compacted after verification. | verification=passed | refs=obs_1",
        created_at="2026-01-01T00:00:00+00:00",
        metadata={"source": "python_oracle"},
    )
    repair_contract = RepairContract(
        contract_id="repair_contract_1",
        analysis_id="analysis_1",
        failure_category="unit_test_failure",
        target_files=["src/app.py"],
        evidence_refs=["obs_failed_1"],
        action_candidates=[
            RepairActionCandidate(
                candidate_id="candidate_1",
                action_type="edit",
                target_file="src/app.py",
                rationale="Fix the failing assertion and rerun pytest.",
                tool_hints=["read_file", "apply_patch", "run_verification"],
                verification_ref="pytest tests/test_app.py",
                confidence=0.82,
            )
        ],
        verification_plan=["pytest tests/test_app.py"],
        confidence=0.82,
        allowed_tool_names=["apply_patch", "read_file", "run_verification"],
        verification_contract=VerificationContract.from_plan_strings(
            ["pytest tests/test_app.py"],
            contract_id="verification_contract_1",
        ),
    )
    tool_repair = {
        "repair_id": "tool_repair_1",
        "run_id": "run_1",
        "session_id": "session_1",
        "task_id": "task_1",
        "phase_id": "repairing_failures",
        "failed_tool_call_id": failed_tool_result.tool_call_id,
        "failure_kind": str(failed_tool_result.error_kind or ""),
        "next_action": "repair_then_verify",
        "failed_result": _rust_tool_output(failed_tool_result),
        "recovery_report": ToolProtocolRecoveryReport(
            succeeded_but_not_appended_call_ids=[failed_tool_result.tool_call_id],
            warnings=["tool result failed before repair"],
            next_action="request_model",
        ).to_dict(),
        "repair_contract": repair_contract.to_dict(),
        "created_at": "2026-01-01T00:00:00+00:00",
        "metadata": {"source": "python_oracle"},
    }
    completion_assessment = {
        "status": "completed",
        "unmet": [],
        "criteria": {
            "verification": {
                "description": "Public verification passed.",
                "required": True,
                "evidence": ["verification_results"],
                "satisfied": True,
                "missing_evidence": [],
            }
        },
        "verification_contract_satisfaction": {
            "contract_id": "verification_contract_1",
            "satisfied": True,
            "completed_steps": ["vstep_0"],
            "failed_steps": [],
            "skipped_steps": [],
            "reason": None,
            "step_evidence": [
                {
                    "step_id": "vstep_0",
                    "check_id": "check_1",
                    "command_id": "command_1",
                    "status": "passed",
                    "artifact_ref": "artifact_verification_1",
                }
            ],
        },
    }
    final_report = FinalReport(
        user_goal="fix tests",
        status=TaskStatus.COMPLETED,
        files_changed=["src/app.py"],
        agent_changes=[],
        command_side_effects=[],
        verification_summary={"status": "ready", "passed_checks": ["pytest tests/test_app.py"]},
        unresolved_issues=[],
        risks=[],
        rollback_status={
            "available": True,
            "transactions": ["transaction_1"],
            "changesets": ["changeset_1"],
            "changed_files": ["src/app.py"],
            "rollback_refs": ["transaction:transaction_1"],
            "verification_state": "ready",
        },
        policy_approval_summary={"approved": 1, "denied": 0},
        artifacts=["final_report.md", "artifact_verification_1"],
        next_steps=[],
        contract_satisfaction=completion_assessment["verification_contract_satisfaction"],
    )
    agent_loop_result = AgentLoopResult(
        status=AgentLoopStatus.COMPLETED,
        final_answer="status: completed\nfiles_changed: src/app.py\nverification: ready",
        turn=7,
    )
    final_report_mapping = {
        "mapping_id": "finalization_mapping_1",
        "run_id": "run_1",
        "session_id": "session_1",
        "task_id": "task_1",
        "phase_id": "finalizing",
        "agent_loop_status": agent_loop_result.status.value,
        "run_status": agent_loop_result.status.value,
        "final_report_status": final_report.status.value,
        "completion_status": completion_assessment["status"],
        "final_answer": agent_loop_result.final_answer,
        "final_report": final_report.to_dict(),
        "completion_assessment": completion_assessment,
        "contract_satisfaction": final_report.contract_satisfaction,
        "created_at": "2026-01-01T00:00:00+00:00",
        "metadata": {"source": "python_oracle"},
    }
    return {
        "tool_result_payload": _rust_tool_result_payload(tool_result),
        "tool_output": _rust_tool_output(tool_result),
        "trace_event": trace_event.to_dict(),
        "command_request": command_request.safe_metadata(),
        "command_result": command_result.to_dict(),
        "permission_profile": _stable_permission_summary(profile),
        "approval_grant": approval.to_dict(),
        "sandbox_policy": _stable_sandbox_policy(sandbox_policy),
        "model_turn_request": model_request.to_dict(),
        "model_turn_response": model_response.to_dict(),
        "planner_state": planner_state.__dict__.copy(),
        "context_bundle": context_bundle.to_dict(),
        "context_summary_envelope": context_summary_envelope.to_dict(),
        "tool_repair": tool_repair,
        "final_report_mapping": final_report_mapping,
    }


def _stable_permission_summary(profile: PermissionProfile) -> dict[str, Any]:
    payload = profile.summary().to_dict()
    if payload["writable_roots"]:
        payload["writable_roots"] = [FIXTURE_WORKSPACE_ROOT]
    return payload


def _rust_tool_result_payload(envelope: ToolProtocolResultEnvelope) -> dict[str, Any]:
    payload = envelope.to_observation_view().to_model_payload()
    return _rename_rust_tool_result_fields(payload)


def _rust_tool_output(envelope: ToolProtocolResultEnvelope) -> dict[str, Any]:
    content: dict[str, Any] = {
        "preview": envelope.content_preview,
        "artifact_ref": envelope.raw_result_ref,
        "artifact_refs": list(envelope.artifact_refs),
        "result_id": envelope.observation_id,
        "digest": envelope.content_digest,
    }
    return {
        "ok": envelope.ok,
        "content": content,
        "error_code": envelope.error_code,
        "truncated": envelope.truncated,
        "metadata": {},
    }


def _rename_rust_tool_result_fields(payload: dict[str, Any]) -> dict[str, Any]:
    renamed = dict(payload)
    if "content_digest" in renamed:
        renamed["digest"] = renamed.pop("content_digest")
    if "result_ref" in renamed:
        renamed["artifact_ref"] = renamed.pop("result_ref")
    if "reference_ids" in renamed:
        renamed["artifact_refs"] = renamed.pop("reference_ids")
    if "observation_id" in renamed:
        renamed["result_id"] = renamed.pop("observation_id")
    if "content_preview" in renamed:
        renamed["preview"] = renamed.pop("content_preview")
    return renamed


def _stable_sandbox_policy(profile: SandboxProfile) -> dict[str, Any]:
    payload = profile.to_dict()
    filesystem = payload["filesystem"]
    filesystem["workspace_root"] = FIXTURE_WORKSPACE_ROOT
    return payload


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
