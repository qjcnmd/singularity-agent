from pathlib import Path

from singularity.policy import (
    ApprovalGate,
    ApprovalGrant,
    ApprovalScope,
    ApprovalMode,
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyConfig,
    PolicyRequest,
    PolicyEngine,
    PolicySubject,
    ResourceRef,
    PolicyComponent,
)


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


def test_policy_allows_workspace_read_and_reviews_mutation(tmp_path: Path) -> None:
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
    assert mutate.outcome == DecisionOutcome.REQUIRE_REVIEW
    assert mutate.required_approval is not None


def test_policy_denies_outside_delete_and_read_only_mutation(tmp_path: Path) -> None:
    outside = tmp_path.parent / "outside.txt"
    component = PolicyEngine(PolicyConfig(workspace_root=tmp_path))
    read_only = PolicyEngine(
        PolicyConfig(workspace_root=tmp_path, approval_mode=ApprovalMode.READ_ONLY)
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

    assert delete.outcome == DecisionOutcome.DENY
    assert mutation.outcome == DecisionOutcome.DENY


def test_non_interactive_review_fails_closed_and_grant_allows_exact_action(tmp_path: Path) -> None:
    component = PolicyEngine(
        PolicyConfig(
            workspace_root=tmp_path,
            approval_mode=ApprovalMode.NON_INTERACTIVE,
            security_mode="compat",
        )
    )
    command = req(
        tmp_path,
        operation=OperationKind.MUTATE_FILE,
        capability=Capability.MUTATE_WORKSPACE,
        resource_type="file",
        identifier="src/app.py",
    )

    denied = component.enforce(command)
    assert denied.outcome == DecisionOutcome.DENY
    assert denied.reason == "Review required but approval mode is non_interactive."

    interactive = PolicyEngine(PolicyConfig(workspace_root=tmp_path, security_mode="compat"))
    gate = ApprovalGate(PolicyConfig(workspace_root=tmp_path, security_mode="compat"))
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
    config = PolicyConfig(
        workspace_root=tmp_path,
        approval_grants_path=grant_path,
        security_mode="compat",
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
    config = PolicyConfig(
        workspace_root=tmp_path,
        approval_grants_path=tmp_path / "policy" / "grants.jsonl",
        security_mode="compat",
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
    config = PolicyConfig(
        workspace_root=tmp_path,
        approval_grants_path=tmp_path / "policy" / "grants.jsonl",
        security_mode="compat",
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
    config = PolicyConfig(
        workspace_root=tmp_path,
        approval_grants_path=tmp_path / "policy" / "grants.jsonl",
        security_mode="compat",
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


def test_policy_config_switches_are_enforced(tmp_path: Path) -> None:
    component = PolicyEngine(
        PolicyConfig(
            workspace_root=tmp_path,
            allow_workspace_reads=False,
            allow_command_with_review=False,
            allow_network_with_review=False,
        )
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

    assert read.outcome == DecisionOutcome.DENY
    assert command.outcome == DecisionOutcome.DENY
    assert network.outcome == DecisionOutcome.DENY


def test_approval_gate_consumes_grant_without_reevaluating_policy(tmp_path: Path) -> None:
    component = PolicyEngine(PolicyConfig(workspace_root=tmp_path))
    gate = ApprovalGate(PolicyConfig(workspace_root=tmp_path))
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
    assert consumed.consumed is True


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
    assert verification.constraints.filesystem_mode == "copy_on_write_workspace"
    assert generated.outcome == DecisionOutcome.SANDBOX_REQUIRED
    assert generated.constraints.hard_isolation_required is True
