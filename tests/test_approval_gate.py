from pathlib import Path

import pytest

from singularity.interaction import (
    InteractionMode,
    InteractionRuntime,
    UserDecision,
)
from singularity.policy import (
    ApprovalGate,
    ApprovalMode,
    ApprovalRequired,
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyConfig,
    PolicyDecision,
    PolicyRequest,
    PolicySubject,
    ResourceRef,
    RuntimeName,
    SandboxRequired,
)


class FakeProvider:
    def __init__(self, decision: str) -> None:
        self.decision = decision

    def request_decision(self, prompt):
        return UserDecision(
            prompt_id=prompt.prompt_id,
            decision=self.decision,
            reason=f"{self.decision} in test",
            decided_by="test-user",
        )

    def request_clarification(self, request):
        raise AssertionError("not used")


def review_decision(tmp_path: Path) -> tuple[PolicyRequest, PolicyDecision]:
    request = PolicyRequest(
        session_id="session",
        task_id="task",
        phase_id="phase",
        action_id="action",
        runtime=RuntimeName.COMMAND,
        operation=OperationKind.EXECUTE_COMMAND,
        capability=Capability.EXECUTE_COMMAND,
        subject=PolicySubject(subject_type="runtime", name="CommandRuntime"),
        resource=ResourceRef(resource_type="command", identifier="python -c print(1)"),
        reason="test",
        workspace_root=str(tmp_path),
    )
    decision = PolicyDecision.review(
        request=request,
        reason="command requires review",
        message="Approve command?",
    )
    return request, decision


def test_interactive_approve_once_generates_single_use_grant(tmp_path: Path) -> None:
    request, decision = review_decision(tmp_path)
    interaction = InteractionRuntime(provider=FakeProvider("approve"))

    grant = ApprovalGate(
        PolicyConfig(workspace_root=tmp_path),
        interaction=interaction,
    ).resolve(request, decision)

    assert grant.single_use is True
    assert grant.scope.single_use is True
    assert grant.matches(request, workspace_root=tmp_path) is True
    assert interaction.decisions[0].decision == "approve"


def test_interactive_reject_raises_approval_denied(tmp_path: Path) -> None:
    from singularity.policy import ApprovalDenied

    request, decision = review_decision(tmp_path)
    interaction = InteractionRuntime(provider=FakeProvider("reject"))

    with pytest.raises(ApprovalDenied):
        ApprovalGate(
            PolicyConfig(workspace_root=tmp_path),
            interaction=interaction,
        ).resolve(request, decision)


def test_interactive_revise_raises_policy_ask_user(tmp_path: Path) -> None:
    from singularity.policy import PolicyAskUserRequired

    request, decision = review_decision(tmp_path)
    interaction = InteractionRuntime(provider=FakeProvider("revise"))

    with pytest.raises(PolicyAskUserRequired):
        ApprovalGate(
            PolicyConfig(workspace_root=tmp_path),
            interaction=interaction,
        ).resolve(request, decision)


def test_interactive_abort_raises_approval_denied(tmp_path: Path) -> None:
    from singularity.policy import ApprovalDenied

    request, decision = review_decision(tmp_path)
    interaction = InteractionRuntime(provider=FakeProvider("abort"))

    with pytest.raises(ApprovalDenied):
        ApprovalGate(
            PolicyConfig(workspace_root=tmp_path),
            interaction=interaction,
        ).resolve(request, decision)

def test_non_interactive_review_fails_without_blocking(tmp_path: Path) -> None:
    request, decision = review_decision(tmp_path)
    interaction = InteractionRuntime(mode=InteractionMode.NON_INTERACTIVE)

    with pytest.raises(ApprovalRequired):
        ApprovalGate(
            PolicyConfig(workspace_root=tmp_path, approval_mode=ApprovalMode.NON_INTERACTIVE),
            interaction=interaction,
        ).resolve(request, decision)
    assert interaction.decisions[0].metadata["fail_closed"] is True


def test_sandbox_required_without_backend_does_not_execute(tmp_path: Path) -> None:
    request, _decision = review_decision(tmp_path)
    decision = PolicyDecision(
        request_id=request.request_id,
        outcome=DecisionOutcome.SANDBOX_REQUIRED,
        reason="sandbox needed",
    )

    with pytest.raises(SandboxRequired):
        ApprovalGate(PolicyConfig(workspace_root=tmp_path)).resolve(request, decision)
