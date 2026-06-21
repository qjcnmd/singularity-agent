from pathlib import Path

from singularity.policy import (
    ApprovalGrant,
    ApprovalScope,
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyConstraints,
    PolicyDecision,
    PolicyRequest,
    PolicySubject,
    ResourceRef,
    RiskLevel,
    RuntimeName,
)


def test_policy_request_decision_and_grant_serialize(tmp_path: Path) -> None:
    request = PolicyRequest(
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        action_id="action_1",
        runtime=RuntimeName.COMMAND,
        operation=OperationKind.EXECUTE_COMMAND,
        capability=Capability.EXECUTE_COMMAND,
        subject=PolicySubject(subject_type="runtime", name="CommandRuntime"),
        resource=ResourceRef(
            resource_type="command",
            identifier="python -m pytest",
            workspace_relative=True,
        ),
        reason="run tests",
        proposed_by_model=True,
    )
    decision = PolicyDecision(
        request_id=request.request_id,
        outcome=DecisionOutcome.REQUIRE_REVIEW,
        risk_level=RiskLevel.MEDIUM,
        reason="commands require review",
        constraints=PolicyConstraints(max_duration_seconds=30),
    )
    grant = ApprovalGrant(
        decision_id=decision.decision_id,
        request_id=request.request_id,
        approved_by="local-cli-user",
        session_id=request.session_id,
        scope=ApprovalScope(
            capabilities=[Capability.EXECUTE_COMMAND],
            command_patterns=["python -m pytest"],
            single_use=True,
        ),
        reason="approved once",
    )

    request_payload = request.to_dict()
    decision_payload = decision.to_dict()
    grant_payload = grant.to_dict()

    assert request_payload["runtime"] == "command"
    assert request_payload["operation"] == "execute_command"
    assert decision_payload["outcome"] == "require_review"
    assert decision_payload["constraints"]["max_duration_seconds"] == 30
    assert grant_payload["scope"]["capabilities"] == ["EXECUTE_COMMAND"]
    assert grant_payload["single_use"] is True
    assert grant.matches(request, workspace_root=tmp_path) is True
