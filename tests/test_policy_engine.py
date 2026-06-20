from pathlib import Path

from miniharness.policy import (
    ApprovalGrant,
    ApprovalScope,
    ApprovalMode,
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyConfig,
    PolicyRequest,
    PolicyRuntime,
    PolicySubject,
    ResourceRef,
    RuntimeName,
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
        runtime=RuntimeName.MUTATION if "FILE" in capability.name else RuntimeName.COMMAND,
        operation=operation,
        capability=capability,
        subject=PolicySubject(subject_type="runtime", name="test"),
        resource=ResourceRef(resource_type=resource_type, identifier=identifier),
        reason="test",
        workspace_root=str(tmp_path),
    )


def test_policy_allows_workspace_read_and_reviews_mutation(tmp_path: Path) -> None:
    runtime = PolicyRuntime(PolicyConfig(workspace_root=tmp_path))

    read = runtime.evaluate(
        req(
            tmp_path,
            operation=OperationKind.READ_FILE,
            capability=Capability.READ_WORKSPACE,
            resource_type="file",
            identifier="README.md",
        )
    )
    mutate = runtime.evaluate(
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
    runtime = PolicyRuntime(PolicyConfig(workspace_root=tmp_path))
    read_only = PolicyRuntime(
        PolicyConfig(workspace_root=tmp_path, approval_mode=ApprovalMode.READ_ONLY)
    )

    delete = runtime.evaluate(
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
    runtime = PolicyRuntime(
        PolicyConfig(workspace_root=tmp_path, approval_mode=ApprovalMode.NON_INTERACTIVE)
    )
    command = req(
        tmp_path,
        operation=OperationKind.EXECUTE_COMMAND,
        capability=Capability.EXECUTE_COMMAND,
        resource_type="command",
        identifier="python -c print(1)",
    )

    denied = runtime.enforce(command)
    assert denied.outcome == DecisionOutcome.DENY
    assert denied.reason == "Review required but approval mode is non_interactive."

    interactive = PolicyRuntime(PolicyConfig(workspace_root=tmp_path))
    pending = interactive.evaluate(command)
    grant = ApprovalGrant(
        decision_id=pending.decision_id,
        request_id=command.request_id,
        approved_by="local-cli-user",
        scope=ApprovalScope(
            capabilities=[Capability.EXECUTE_COMMAND],
            command_patterns=["python -c print(1)"],
            single_use=True,
        ),
    )
    interactive.register_grant(grant)

    allowed = interactive.enforce(command)
    other = req(
        tmp_path,
        operation=OperationKind.EXECUTE_COMMAND,
        capability=Capability.EXECUTE_COMMAND,
        resource_type="command",
        identifier="python -c print(2)",
    )

    assert allowed.outcome == DecisionOutcome.ALLOW
    assert interactive.find_matching_grant(other) is None
    assert grant.consumed is True


def test_consume_grant_allows_without_reevaluating_policy(tmp_path: Path) -> None:
    audit_path = tmp_path / "policy.jsonl"
    runtime = PolicyRuntime(
        PolicyConfig(workspace_root=tmp_path, audit_log_path=audit_path)
    )
    request = req(
        tmp_path,
        operation=OperationKind.EXECUTE_COMMAND,
        capability=Capability.EXECUTE_COMMAND,
        resource_type="command",
        identifier="python -c print(1)",
    )
    pending = runtime.evaluate(request)
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
    audit_path.unlink()

    allowed = runtime.consume_grant(request, pending, grant)

    assert allowed.outcome == DecisionOutcome.ALLOW
    assert allowed.approval_grant_id == grant.grant_id
    assert grant.consumed is True
    assert len(audit_path.read_text(encoding="utf-8").splitlines()) == 1


def test_policy_requires_sandbox_for_verification_and_generated_code(tmp_path: Path) -> None:
    runtime = PolicyRuntime(PolicyConfig(workspace_root=tmp_path))

    verification = runtime.evaluate(
        req(
            tmp_path,
            operation=OperationKind.VERIFICATION,
            capability=Capability.EXECUTE_PROJECT_CODE,
            resource_type="command",
            identifier="python -m pytest",
        )
    )
    generated = runtime.evaluate(
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
