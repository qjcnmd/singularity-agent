from pathlib import Path

import pytest

from miniharness.policy import (
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


def test_interactive_approve_once_generates_single_use_grant(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    request, decision = review_decision(tmp_path)
    monkeypatch.setattr("builtins.input", lambda _prompt: "a")

    grant = ApprovalGate(PolicyConfig(workspace_root=tmp_path)).resolve(request, decision)

    assert grant.single_use is True
    assert grant.scope.single_use is True
    assert grant.matches(request, workspace_root=tmp_path) is True


def test_interactive_deny_raises_approval_denied(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from miniharness.policy import ApprovalDenied

    request, decision = review_decision(tmp_path)
    monkeypatch.setattr("builtins.input", lambda _prompt: "d")

    with pytest.raises(ApprovalDenied):
        ApprovalGate(PolicyConfig(workspace_root=tmp_path)).resolve(request, decision)

def test_non_interactive_review_fails_without_blocking(tmp_path: Path) -> None:
    request, decision = review_decision(tmp_path)

    with pytest.raises(ApprovalRequired):
        ApprovalGate(
            PolicyConfig(workspace_root=tmp_path, approval_mode=ApprovalMode.NON_INTERACTIVE)
        ).resolve(request, decision)


def test_sandbox_required_without_backend_does_not_execute(tmp_path: Path) -> None:
    request, _decision = review_decision(tmp_path)
    decision = PolicyDecision(
        request_id=request.request_id,
        outcome=DecisionOutcome.SANDBOX_REQUIRED,
        reason="sandbox needed",
    )

    with pytest.raises(SandboxRequired):
        ApprovalGate(PolicyConfig(workspace_root=tmp_path)).resolve(request, decision)
