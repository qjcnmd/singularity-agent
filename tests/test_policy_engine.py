from pathlib import Path

from singularity.policy import (
    ApprovalGate,
    ApprovalGrant,
    ApprovalScope,
    ApprovalPolicy,
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyConfig,
    PolicyRequest,
    PolicyEngine,
    PolicySubject,
    ResourceRef,
    PolicyComponent,
    PermissionProfile,
    PermissionProfileName,
)
from tests.tool_executor_helpers import make_ledger_test_config


def req(
    tmp_path: Path,
    *,
    operation: OperationKind,
    capability: Capability,
    resource_type: str,
    identifier: str,
) -> PolicyRequest:
    return PolicyRequest(
        session_id="session",
        task_id="task",
        phase_id="phase",
        action_id="action",
        component=PolicyComponent.MUTATION if "FILE" in capability.name else PolicyComponent.COMMAND,
        operation=operation,
        capability=capability,
        subject=PolicySubject(subject_type="component", name="test"),
        resource=ResourceRef(resource_type=resource_type, identifier=identifier),
        reason="test",
        workspace_root=str(tmp_path),
    )


def test_workspace_write_profile_allows_workspace_read_and_mutation(tmp_path: Path) -> None:
    component = PolicyEngine(PolicyConfig(workspace_root=tmp_path))

    read = component.evaluate(
        req(
            tmp_path,
            operation=OperationKind.READ_FILE,
            capability=Capability.READ_WORKSPACE,
            resource_type="file",
            identifier="README.md",
        )
    )
    mutate = component.evaluate(
        req(
            tmp_path,
            operation=OperationKind.MUTATE_FILE,
            capability=Capability.MUTATE_WORKSPACE,
            resource_type="file",
            identifier="src/app.py",
        )
    )

    assert read.outcome == DecisionOutcome.ALLOW
    assert mutate.outcome == DecisionOutcome.ALLOW
    assert mutate.required_approval is None


def test_policy_reviews_outside_delete_and_read_only_mutation(tmp_path: Path) -> None:
    outside = tmp_path.parent / "outside.txt"
    component = PolicyEngine(PolicyConfig(workspace_root=tmp_path))
    read_only = PolicyEngine(
        PolicyConfig(
            workspace_root=tmp_path,
            permission_profile=PermissionProfile.default_for_workspace(
                tmp_path, profile=PermissionProfileName.READ_ONLY
            ),
        )
    )

    delete = component.evaluate(
        req(
            tmp_path,
            operation=OperationKind.DELETE_FILE,
            capability=Capability.DELETE_FILE,
            resource_type="file",
            identifier=str(outside),
        )
    )
    mutation = read_only.evaluate(
        req(
            tmp_path,
            operation=OperationKind.MUTATE_FILE,
            capability=Capability.MUTATE_WORKSPACE,
            resource_type="file",
            identifier="src/app.py",
        )
    )

    assert delete.outcome == DecisionOutcome.REQUIRE_REVIEW
    assert mutation.outcome == DecisionOutcome.REQUIRE_REVIEW


def test_approval_never_fails_closed_and_on_request_grant_allows_exact_action(tmp_path: Path) -> None:
    grants_path = tmp_path / "policy" / "grants.jsonl"
    ledger_path = tmp_path / "policy" / "ledger.jsonl"
    never_config = make_ledger_test_config(
        tmp_path,
        grants_path=grants_path,
        ledger_path=ledger_path,
        permission_profile=PermissionProfile.default_for_workspace(
            tmp_path,
            profile=PermissionProfileName.READ_ONLY,
            approval_policy=ApprovalPolicy.NEVER,
        ),
    )
    component = PolicyEngine(never_config)
    command = req(
        tmp_path,
        operation=OperationKind.MUTATE_FILE,
        capability=Capability.MUTATE_WORKSPACE,
        resource_type="file",
        identifier="src/app.py",
    )

    denied = component.enforce(command)
    assert denied.outcome == DecisionOutcome.DENY
    assert "Approval policy is never" in denied.reason

    config = make_ledger_test_config(
        tmp_path,
        grants_path=grants_path,
        ledger_path=ledger_path,
        permission_profile=PermissionProfile.default_for_workspace(
            tmp_path, profile=PermissionProfileName.READ_ONLY
        ),
    )
    interactive = PolicyEngine(config)
    gate = ApprovalGate(config)
    pending = interactive.evaluate(command)
    grant = ApprovalGrant(
        decision_id=pending.decision_id,
        request_id=command.request_id,
        approved_by="local-cli-user",
        session_id=command.session_id,
        scope=ApprovalScope(
            capabilities=[Capability.MUTATE_WORKSPACE],
            path_globs=["src/app.py"],
            single_use=True,
        ),
    )
    gate.register_grant(grant)

    consumed = gate.consume_matching_grant(command)
    other = req(
        tmp_path,
        operation=OperationKind.MUTATE_FILE,
        capability=Capability.MUTATE_WORKSPACE,
        resource_type="file",
        identifier="src/other.py",
    )

    assert consumed is not None
    assert consumed.grant_id == grant.grant_id
    assert gate.find_matching_grant(other) is None
    assert gate.find_matching_grant(command) is None


def test_policy_grants_persist_across_process_restarts(tmp_path: Path) -> None:
    grant_path = tmp_path / "policy" / "grants.jsonl"
    ledger_path = tmp_path / "policy" / "ledger.jsonl"
    config = make_ledger_test_config(
        tmp_path,
        grants_path=grant_path,
        ledger_path=ledger_path,
    )
    first = ApprovalGate(config)
    request = req(
        tmp_path,
        operation=OperationKind.MUTATE_FILE,
        capability=Capability.MUTATE_WORKSPACE,
        resource_type="file",
        identifier="src/app.py",
    )
    pending = PolicyEngine(config).evaluate(request)
    grant = ApprovalGrant(
        decision_id=pending.decision_id,
        request_id=request.request_id,
        approved_by="local-cli-user",
        session_id=request.session_id,
        scope=ApprovalScope(
            capabilities=[Capability.MUTATE_WORKSPACE],
            path_globs=[str((tmp_path / "src" / "app.py").resolve(strict=False))],
            single_use=True,
        ),
    )

    first.register_grant(grant)
    restarted = ApprovalGate(config)
    consumed = restarted.consume_matching_grant(request)

    assert consumed is not None
    assert consumed.grant_id == grant.grant_id
    assert ApprovalGate(config).find_matching_grant(request) is None


def test_single_use_grant_cannot_be_consumed_twice_by_stale_process(tmp_path: Path) -> None:
    config = make_ledger_test_config(
        tmp_path,
        grants_path=tmp_path / "policy" / "grants.jsonl",
        ledger_path=tmp_path / "policy" / "ledger.jsonl",
    )
    request = req(
        tmp_path,
        operation=OperationKind.MUTATE_FILE,
        capability=Capability.MUTATE_WORKSPACE,
        resource_type="file",
        identifier="src/app.py",
    )
    pending = PolicyEngine(config).evaluate(request)
    grant = ApprovalGrant(
        decision_id=pending.decision_id,
        request_id=request.request_id,
        approved_by="local-cli-user",
        session_id=request.session_id,
        scope=ApprovalScope(
            capabilities=[Capability.MUTATE_WORKSPACE],
            path_globs=["src/app.py"],
            single_use=True,
        ),
    )
    writer = ApprovalGate(config)
    writer.register_grant(grant)
    stale = ApprovalGate(config)
    first = ApprovalGate(config)

    assert first.consume_matching_grant(request) is not None
    second = stale.consume_matching_grant(request)

    assert second is None
    assert ApprovalGate(config).find_matching_grant(request) is None


def test_session_only_grant_does_not_match_other_session_after_restart(tmp_path: Path) -> None:
    config = make_ledger_test_config(
        tmp_path,
        grants_path=tmp_path / "policy" / "grants.jsonl",
        ledger_path=tmp_path / "policy" / "ledger.jsonl",
    )
    request = req(
        tmp_path,
        operation=OperationKind.MUTATE_FILE,
        capability=Capability.MUTATE_WORKSPACE,
        resource_type="file",
        identifier="src/app.py",
    )
    pending = PolicyEngine(config).evaluate(request)
    grant = ApprovalGrant(
        decision_id=pending.decision_id,
        request_id=request.request_id,
        approved_by="local-cli-user",
        session_id=request.session_id,
        scope=ApprovalScope(
            capabilities=[Capability.MUTATE_WORKSPACE],
            path_globs=["src/app.py"],
            session_only=True,
            single_use=True,
        ),
    )
    component = ApprovalGate(config)
    component.register_grant(grant)
    other_session = PolicyRequest(
        session_id="other_session",
        task_id=request.task_id,
        phase_id=request.phase_id,
        action_id=request.action_id,
        component=request.component,
        operation=request.operation,
        capability=request.capability,
        subject=request.subject,
        resource=request.resource,
        reason=request.reason,
        workspace_root=request.workspace_root,
    )

    assert ApprovalGate(config).find_matching_grant(other_session) is None


def test_session_only_grant_without_session_id_fails_closed(tmp_path: Path) -> None:
    config = make_ledger_test_config(
        tmp_path,
        grants_path=tmp_path / "policy" / "grants.jsonl",
        ledger_path=tmp_path / "policy" / "ledger.jsonl",
    )
    request = req(
        tmp_path,
        operation=OperationKind.MUTATE_FILE,
        capability=Capability.MUTATE_WORKSPACE,
        resource_type="file",
        identifier="src/app.py",
    )
    pending = PolicyEngine(config).evaluate(request)
    grant = ApprovalGrant(
        decision_id=pending.decision_id,
        request_id=request.request_id,
        approved_by="legacy-local-cli-user",
        scope=ApprovalScope(
            capabilities=[Capability.MUTATE_WORKSPACE],
            path_globs=["src/app.py"],
            session_only=True,
            single_use=True,
        ),
    )
    component = ApprovalGate(config)
    component.register_grant(grant)

    assert ApprovalGate(config).find_matching_grant(request) is None


def test_policy_config_uses_one_session_permission_profile(tmp_path: Path) -> None:
    profile = PermissionProfile.default_for_workspace(
        tmp_path,
        profile=PermissionProfileName.READ_ONLY,
        approval_policy=ApprovalPolicy.NEVER,
    )
    component = PolicyEngine(
        PolicyConfig(workspace_root=tmp_path, permission_profile=profile)
    )

    read = component.evaluate(
        req(
            tmp_path,
            operation=OperationKind.READ_FILE,
            capability=Capability.READ_WORKSPACE,
            resource_type="file",
            identifier="README.md",
        )
    )
    command = component.evaluate(
        req(
            tmp_path,
            operation=OperationKind.EXECUTE_COMMAND,
            capability=Capability.EXECUTE_COMMAND,
            resource_type="command",
            identifier="python -c print(1)",
        )
    )
    network = component.evaluate(
        req(
            tmp_path,
            operation=OperationKind.NETWORK_ACCESS,
            capability=Capability.NETWORK_ACCESS,
            resource_type="network",
            identifier="https://example.test",
        )
    )

    assert component.config.permission_profile is profile
    assert read.outcome == DecisionOutcome.ALLOW
    assert command.outcome == DecisionOutcome.DENY
    assert network.outcome == DecisionOutcome.DENY


def test_approval_gate_consumes_grant_without_reevaluating_policy(tmp_path: Path) -> None:
    grants_path = tmp_path / "policy" / "grants.jsonl"
    ledger_path = tmp_path / "policy" / "ledger.jsonl"
    config = make_ledger_test_config(
        tmp_path,
        grants_path=grants_path,
        ledger_path=ledger_path,
    )
    component = PolicyEngine(config)
    gate = ApprovalGate(config)
    request = req(
        tmp_path,
        operation=OperationKind.EXECUTE_COMMAND,
        capability=Capability.EXECUTE_COMMAND,
        resource_type="command",
        identifier="python -c print(1)",
    )
    pending = component.evaluate(request)
    grant = ApprovalGrant(
        decision_id=pending.decision_id,
        request_id=request.request_id,
        approved_by="local-cli-user",
        scope=ApprovalScope(
            capabilities=[Capability.EXECUTE_COMMAND],
            command_patterns=["python -c print(1)"],
            single_use=True,
        ),
    )
    gate.register_grant(grant)
    consumed = gate.consume_grant(grant)

    assert consumed is not None
    assert consumed.grant_id == grant.grant_id
    assert gate.is_grant_consumed(consumed.grant_id) is True


def test_policy_requires_sandbox_for_verification_and_generated_code(tmp_path: Path) -> None:
    component = PolicyEngine(PolicyConfig(workspace_root=tmp_path))

    verification = component.evaluate(
        req(
            tmp_path,
            operation=OperationKind.VERIFICATION,
            capability=Capability.EXECUTE_PROJECT_CODE,
            resource_type="command",
            identifier="python -m pytest",
        )
    )
    generated = component.evaluate(
        req(
            tmp_path,
            operation=OperationKind.EXECUTE_PROJECT_CODE,
            capability=Capability.EXECUTE_GENERATED_CODE,
            resource_type="command",
            identifier="python generated.py",
        )
    )

    assert verification.outcome == DecisionOutcome.SANDBOX_REQUIRED
    assert verification.constraints.sandbox_required is True
    assert verification.constraints.hard_isolation_required is True
    assert verification.constraints.filesystem_mode == "workspace-write"
    assert generated.outcome == DecisionOutcome.SANDBOX_REQUIRED
    assert generated.constraints.hard_isolation_required is True


def test_policy_hard_denies_writes_to_workspace_policy_dir(tmp_path: Path) -> None:
    # Trust boundary: writes to <workspace>/.singularity/policy/ must be
    # hard-denied so the model cannot forge approval grants or audit entries
    # via shell writes.
    component = PolicyEngine(PolicyConfig(workspace_root=tmp_path))

    policy_grants_path = tmp_path / ".singularity" / "policy" / "approval_grants.jsonl"
    mutate = component.evaluate(
        req(
            tmp_path,
            operation=OperationKind.CREATE_FILE,
            capability=Capability.CREATE_FILE,
            resource_type="file",
            identifier=str(policy_grants_path),
        )
    )
    assert mutate.outcome == DecisionOutcome.DENY
    assert "hard_deny_protected_path" in mutate.rule_ids

    # Command strings referencing the policy dir are also denied.
    command = component.evaluate(
        req(
            tmp_path,
            operation=OperationKind.EXECUTE_COMMAND,
            capability=Capability.EXECUTE_COMMAND,
            resource_type="command",
            identifier='bash -c "echo fake > .singularity/policy/approval_grants.jsonl"',
        )
    )
    assert command.outcome == DecisionOutcome.DENY
    assert "hard_deny_protected_path" in command.rule_ids


def test_policy_allows_reads_outside_policy_dir(tmp_path: Path) -> None:
    # Trust boundary: reads of normal workspace files must still be allowed.
    component = PolicyEngine(PolicyConfig(workspace_root=tmp_path))
    read = component.evaluate(
        req(
            tmp_path,
            operation=OperationKind.READ_FILE,
            capability=Capability.READ_WORKSPACE,
            resource_type="file",
            identifier="src/app.py",
        )
    )
    assert read.outcome == DecisionOutcome.ALLOW
