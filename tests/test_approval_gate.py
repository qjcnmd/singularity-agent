from pathlib import Path

import pytest

from singularity.interaction import (
    InteractionMode,
    InteractionController,
    UserDecision,
)
from singularity.policy import (
    ApprovalGate,
    ApprovalRequired,
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyConfig,
    PolicyDecision,
    PolicyRequest,
    PolicySubject,
    ResourceRef,
    PolicyComponent,
    SandboxRequired,
)
from singularity.policy.approval import _approval_grants_path
from singularity.policy.config import _default_policy_home
from singularity.policy.permissions import ApprovalPolicy, PermissionProfile
from tests.tool_executor_helpers import make_ledger_test_config


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
        component=PolicyComponent.COMMAND,
        operation=OperationKind.EXECUTE_COMMAND,
        capability=Capability.EXECUTE_COMMAND,
        subject=PolicySubject(subject_type="component", name="CommandExecutor"),
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
    interaction = InteractionController(provider=FakeProvider("approve"))

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
    interaction = InteractionController(provider=FakeProvider("reject"))

    with pytest.raises(ApprovalDenied):
        ApprovalGate(
            PolicyConfig(workspace_root=tmp_path),
            interaction=interaction,
        ).resolve(request, decision)


def test_interactive_revise_raises_policy_ask_user(tmp_path: Path) -> None:
    from singularity.policy import PolicyAskUserRequired

    request, decision = review_decision(tmp_path)
    interaction = InteractionController(provider=FakeProvider("revise"))

    with pytest.raises(PolicyAskUserRequired):
        ApprovalGate(
            PolicyConfig(workspace_root=tmp_path),
            interaction=interaction,
        ).resolve(request, decision)


def test_interactive_abort_raises_approval_denied(tmp_path: Path) -> None:
    from singularity.policy import ApprovalDenied

    request, decision = review_decision(tmp_path)
    interaction = InteractionController(provider=FakeProvider("abort"))

    with pytest.raises(ApprovalDenied):
        ApprovalGate(
            PolicyConfig(workspace_root=tmp_path),
            interaction=interaction,
        ).resolve(request, decision)

def test_non_interactive_review_fails_without_blocking(tmp_path: Path) -> None:
    request, decision = review_decision(tmp_path)
    interaction = InteractionController(mode=InteractionMode.NON_INTERACTIVE)

    with pytest.raises(ApprovalRequired):
        ApprovalGate(
            PolicyConfig(
                workspace_root=tmp_path,
                permission_profile=PermissionProfile.default_for_workspace(
                    tmp_path,
                    approval_policy=ApprovalPolicy.NEVER,
                ),
            ),
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


def test_default_approval_grants_path_lives_outside_workspace(tmp_path: Path) -> None:
    # Trust boundary: default grant store must live under the policy home
    # (``~/.singularity/policy/`` in production, redirected via
    # ``SINGULARITY_POLICY_HOME`` in tests) so the model cannot forge grants
    # via shell writes inside the workspace. We use a subdirectory as the
    # workspace to keep the default store outside.
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    config = PolicyConfig(workspace_root=workspace)
    grants_path = _approval_grants_path(config)

    assert grants_path == _default_policy_home() / ".singularity" / "policy" / "approval_grants.jsonl"
    # The grants path must not be inside the workspace.
    gate = ApprovalGate(config)
    assert gate.is_grant_store_trusted(workspace) is True


def test_default_audit_log_path_lives_outside_workspace(tmp_path: Path) -> None:
    # Trust boundary: default audit log path must also live outside the workspace.
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    config = PolicyConfig(workspace_root=workspace)
    assert Path(config.audit_log_path) == _default_policy_home() / ".singularity" / "policy" / "audit.jsonl"
    # Audit path must be outside the workspace.
    gate = ApprovalGate(config)
    assert gate.is_grant_store_trusted(workspace) is True


def test_grant_store_inside_workspace_is_untrusted(tmp_path: Path) -> None:
    # Trust boundary: grant stores inside the workspace are untrusted.
    inside_config = PolicyConfig(
        workspace_root=tmp_path,
        approval_grants_path=tmp_path / ".singularity" / "policy" / "approval_grants.jsonl",
    )
    gate_inside = ApprovalGate(inside_config)
    assert gate_inside.is_grant_store_trusted(tmp_path) is False

    outside_config = PolicyConfig(
        workspace_root=tmp_path,
        approval_grants_path=tmp_path.parent / "outside_grants.jsonl",
    )
    gate_outside = ApprovalGate(outside_config)
    assert gate_outside.is_grant_store_trusted(tmp_path) is True


def test_repeated_import_without_grant_id_does_not_amplify(tmp_path: Path) -> None:
    # Grant identity: repeated import of the same grant payload (without
    # grant_id) must not amplify a single approval into multiple consumable
    # grants. ``ApprovalGrant.from_dict`` generates a deterministic grant_id
    # from ``decision_id`` + ``request_id`` + ``approved_by``, and
    # ``register_grant`` dedups by grant_id OR decision_id, so the second
    # import replaces the first instead of appending a new consumable grant.
    from singularity.policy import ApprovalGrant, ApprovalScope
    from singularity.policy.approval import _approval_grants_path

    grants_path = tmp_path / "outside_grants.jsonl"
    ledger_path = tmp_path / "outside_ledger.jsonl"
    config = make_ledger_test_config(
        tmp_path,
        grants_path=grants_path,
        ledger_path=ledger_path,
    )
    gate = ApprovalGate(config)

    request, _decision = review_decision(tmp_path)
    grant_payload = {
        "decision_id": "policy_dec_test_amplify",
        "request_id": request.request_id,
        "approved_by": "test-approver",
        "session_id": request.session_id,
        "scope": {
            "capabilities": [Capability.EXECUTE_COMMAND.value],
            "command_patterns": [request.resource.identifier],
            "session_only": True,
            "single_use": True,
        },
        "single_use": True,
        "reason": "approved once",
    }

    # Parse and register the same payload twice. Both calls must resolve to
    # the same deterministic grant_id and the second registration must
    # replace the first rather than create a second consumable entry.
    grant_first = ApprovalGrant.from_dict(grant_payload)
    grant_second = ApprovalGrant.from_dict(grant_payload)
    assert grant_first.grant_id == grant_second.grant_id, (
        "from_dict must produce a deterministic grant_id for the same "
        "decision_id + request_id + approved_by triple."
    )

    gate.register_grant(grant_first)
    gate.register_grant(grant_second)

    # Only one grant should be persisted.
    persisted = _approval_grants_path(config).read_text(encoding="utf-8").splitlines()
    persisted = [line for line in persisted if line.strip()]
    assert len(persisted) == 1, "Repeated import must not duplicate grants."

    # The single persisted grant must be consumable exactly once. The first
    # consumption succeeds and the second must return None (no amplification).
    first_consume = gate.consume_matching_grant(request)
    assert first_consume is not None
    assert gate.is_grant_consumed(first_consume.grant_id) is True

    second_consume = gate.consume_matching_grant(request)
    assert second_consume is None, (
        "A single approval must not be amplified into multiple consumable "
        "grants via repeated import."
    )


def test_register_grant_dedups_by_decision_id(tmp_path: Path) -> None:
    # Grant identity: even when two grants carry different grant_ids,
    # registering a grant with the same decision_id as an existing one must
    # replace the prior entry instead of appending a second consumable grant.
    # This prevents a reviewer from inflating a single decision into many
    # grants.
    from singularity.policy import ApprovalGrant, ApprovalScope

    grants_path = tmp_path / "outside_grants.jsonl"
    config = PolicyConfig(
        workspace_root=tmp_path,
        approval_grants_path=grants_path,
    )
    gate = ApprovalGate(config)

    request, _decision = review_decision(tmp_path)
    scope = ApprovalScope(
        capabilities=[Capability.EXECUTE_COMMAND],
        command_patterns=[request.resource.identifier],
        session_only=True,
        single_use=True,
    )
    grant_a = ApprovalGrant(
        decision_id="policy_dec_dedup",
        request_id=request.request_id,
        approved_by="approver-a",
        session_id=request.session_id,
        scope=scope,
        grant_id="grant_dedup_a",
        single_use=True,
    )
    grant_b = ApprovalGrant(
        decision_id="policy_dec_dedup",
        request_id=request.request_id,
        approved_by="approver-a",
        session_id=request.session_id,
        scope=scope,
        grant_id="grant_dedup_b",
        single_use=True,
    )

    gate.register_grant(grant_a)
    gate.register_grant(grant_b)

    persisted = grants_path.read_text(encoding="utf-8").splitlines()
    persisted = [line for line in persisted if line.strip()]
    assert len(persisted) == 1, (
        "Two grants sharing a decision_id must collapse to one entry."
    )

    import json as _json
    stored = _json.loads(persisted[0])
    assert stored["grant_id"] == "grant_dedup_b"
    assert stored["decision_id"] == "policy_dec_dedup"
