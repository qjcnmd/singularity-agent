from __future__ import annotations

from dataclasses import FrozenInstanceError
from pathlib import Path

import pytest

from singularity.policy import (
    ApprovalPolicy,
    Capability,
    DecisionOutcome,
    NetworkAccess,
    OperationKind,
    PermissionProfile,
    PermissionProfileName,
    PolicyComponent,
    PolicyConfig,
    PolicyEngine,
    PolicyRequest,
    PolicySubject,
    ProtectedPathRule,
    ResourceRef,
)


def _profile(
    workspace: Path,
    name: PermissionProfileName,
    *,
    approval_policy: ApprovalPolicy = ApprovalPolicy.ON_REQUEST,
    network_access: NetworkAccess = NetworkAccess.DENIED,
    additional_writable_directories: tuple[Path, ...] = (),
) -> PermissionProfile:
    return PermissionProfile(
        profile=name,
        workspace_roots=(workspace,),
        additional_writable_directories=additional_writable_directories,
        approval_policy=approval_policy,
        network_access=network_access,
    )


def _request(
    workspace: Path,
    *,
    operation: OperationKind,
    capability: Capability,
    resource_type: str,
    identifier: str,
    requires_network: bool = False,
) -> PolicyRequest:
    return PolicyRequest(
        session_id="session",
        task_id="task",
        phase_id="phase",
        action_id="action",
        component=(
            PolicyComponent.MUTATION
            if operation
            in {
                OperationKind.MUTATE_FILE,
                OperationKind.CREATE_FILE,
                OperationKind.DELETE_FILE,
            }
            else PolicyComponent.COMMAND
        ),
        operation=operation,
        capability=capability,
        subject=PolicySubject(subject_type="component", name="test"),
        resource=ResourceRef(resource_type=resource_type, identifier=identifier),
        reason="test",
        requires_network=requires_network,
        workspace_root=str(workspace),
    )


def _decision(
    workspace: Path,
    profile: PermissionProfile,
    *,
    operation: OperationKind,
    capability: Capability,
    resource_type: str,
    identifier: str,
    requires_network: bool = False,
):
    engine = PolicyEngine(
        PolicyConfig(workspace_root=workspace, permission_profile=profile)
    )
    return engine.evaluate(
        _request(
            workspace,
            operation=operation,
            capability=capability,
            resource_type=resource_type,
            identifier=identifier,
            requires_network=requires_network,
        )
    )


def test_permission_profile_is_immutable_and_normalizes_paths(tmp_path: Path) -> None:
    extra = tmp_path.parent / "extra"
    profile = PermissionProfile(
        profile="workspace-write",
        workspace_roots=(str(tmp_path / "."),),
        additional_writable_directories=(str(extra),),
        network_access="denied",
        approval_policy="on-request",
    )

    assert profile.profile is PermissionProfileName.WORKSPACE_WRITE
    assert profile.workspace_roots == (tmp_path.resolve(strict=False),)
    assert profile.additional_writable_directories == (extra.resolve(strict=False),)
    with pytest.raises(FrozenInstanceError):
        profile.profile = PermissionProfileName.DANGER_FULL_ACCESS  # type: ignore[misc]


def test_permission_summary_contains_only_model_safe_boundary_fields(tmp_path: Path) -> None:
    extra = tmp_path.parent / "extra"
    summary = _profile(
        tmp_path,
        PermissionProfileName.WORKSPACE_WRITE,
        additional_writable_directories=(extra,),
    ).summary()

    assert summary.to_dict() == {
        "profile": "workspace-write",
        "writable_roots": [
            str(tmp_path.resolve(strict=False)),
            str(extra.resolve(strict=False)),
        ],
        "network_access": "denied",
        "approval_policy": "on-request",
        "protected_paths_enforced": True,
    }
    assert "protected_paths" not in summary.to_dict()


def test_additional_directory_is_writable_without_full_access(tmp_path: Path) -> None:
    extra = tmp_path.parent / "extra"
    profile = _profile(
        tmp_path,
        PermissionProfileName.WORKSPACE_WRITE,
        additional_writable_directories=(extra,),
    )

    assert profile.is_writable_path(extra / "artifact.txt") is True
    assert profile.is_writable_path(tmp_path.parent / "other" / "artifact.txt") is False


@pytest.mark.parametrize(
    ("name", "mutation", "command"),
    [
        (
            PermissionProfileName.READ_ONLY,
            DecisionOutcome.REQUIRE_REVIEW,
            DecisionOutcome.REQUIRE_REVIEW,
        ),
        (
            PermissionProfileName.WORKSPACE_WRITE,
            DecisionOutcome.ALLOW,
            DecisionOutcome.SANDBOX_REQUIRED,
        ),
        (
            PermissionProfileName.DANGER_FULL_ACCESS,
            DecisionOutcome.ALLOW,
            DecisionOutcome.ALLOW,
        ),
    ],
)
def test_profile_controls_workspace_mutation_and_local_command(
    tmp_path: Path,
    name: PermissionProfileName,
    mutation: DecisionOutcome,
    command: DecisionOutcome,
) -> None:
    profile = _profile(tmp_path, name)

    read = _decision(
        tmp_path,
        profile,
        operation=OperationKind.READ_FILE,
        capability=Capability.READ_WORKSPACE,
        resource_type="file",
        identifier="README.md",
    )
    mutate = _decision(
        tmp_path,
        profile,
        operation=OperationKind.MUTATE_FILE,
        capability=Capability.MUTATE_WORKSPACE,
        resource_type="file",
        identifier="src/app.py",
    )
    execute = _decision(
        tmp_path,
        profile,
        operation=OperationKind.EXECUTE_COMMAND,
        capability=Capability.EXECUTE_COMMAND,
        resource_type="command",
        identifier="python -c print(1)",
    )

    assert read.outcome is DecisionOutcome.ALLOW
    assert mutate.outcome is mutation
    assert execute.outcome is command


@pytest.mark.parametrize("name", list(PermissionProfileName))
def test_outside_workspace_write_requires_review(
    tmp_path: Path, name: PermissionProfileName
) -> None:
    outside = tmp_path.parent / "outside" / "artifact.txt"

    decision = _decision(
        tmp_path,
        _profile(tmp_path, name),
        operation=OperationKind.CREATE_FILE,
        capability=Capability.CREATE_FILE,
        resource_type="file",
        identifier=str(outside),
    )

    assert decision.outcome is DecisionOutcome.REQUIRE_REVIEW


@pytest.mark.parametrize(
    ("name", "expected"),
    [
        (PermissionProfileName.READ_ONLY, DecisionOutcome.SANDBOX_REQUIRED),
        (PermissionProfileName.WORKSPACE_WRITE, DecisionOutcome.SANDBOX_REQUIRED),
        (PermissionProfileName.DANGER_FULL_ACCESS, DecisionOutcome.ALLOW),
    ],
)
def test_explicit_network_access_still_uses_sandbox_except_full_access(
    tmp_path: Path, name: PermissionProfileName, expected: DecisionOutcome
) -> None:
    decision = _decision(
        tmp_path,
        _profile(tmp_path, name, network_access=NetworkAccess.ALLOWED),
        operation=OperationKind.NETWORK_ACCESS,
        capability=Capability.NETWORK_ACCESS,
        resource_type="network",
        identifier="https://example.test",
        requires_network=True,
    )

    assert decision.outcome is expected


def test_denied_network_and_package_install_require_review(tmp_path: Path) -> None:
    profile = _profile(tmp_path, PermissionProfileName.DANGER_FULL_ACCESS)

    network = _decision(
        tmp_path,
        profile,
        operation=OperationKind.NETWORK_ACCESS,
        capability=Capability.NETWORK_ACCESS,
        resource_type="network",
        identifier="https://example.test",
        requires_network=True,
    )
    package = _decision(
        tmp_path,
        profile,
        operation=OperationKind.PACKAGE_INSTALL,
        capability=Capability.PACKAGE_INSTALL,
        resource_type="command",
        identifier="python -m pip install sample",
        requires_network=True,
    )

    assert network.outcome is DecisionOutcome.REQUIRE_REVIEW
    assert package.outcome is DecisionOutcome.REQUIRE_REVIEW


def test_never_approval_policy_turns_review_into_deny(tmp_path: Path) -> None:
    decision = _decision(
        tmp_path,
        _profile(
            tmp_path,
            PermissionProfileName.READ_ONLY,
            approval_policy=ApprovalPolicy.NEVER,
        ),
        operation=OperationKind.MUTATE_FILE,
        capability=Capability.MUTATE_WORKSPACE,
        resource_type="file",
        identifier="src/app.py",
    )

    assert decision.outcome is DecisionOutcome.DENY
    assert decision.required_approval is None


@pytest.mark.parametrize(
    ("identifier", "operation", "capability"),
    [
        (".git/config", OperationKind.MUTATE_FILE, Capability.MUTATE_WORKSPACE),
        (".singularity/state.json", OperationKind.READ_FILE, Capability.READ_WORKSPACE),
        (".env", OperationKind.READ_FILE, Capability.READ_WORKSPACE),
        (".ssh/id_ed25519", OperationKind.READ_FILE, Capability.READ_WORKSPACE),
        ("credentials.json", OperationKind.READ_FILE, Capability.READ_WORKSPACE),
        ("deploy.key", OperationKind.READ_FILE, Capability.READ_WORKSPACE),
    ],
)
def test_builtin_protected_paths_are_hard_denied(
    tmp_path: Path,
    identifier: str,
    operation: OperationKind,
    capability: Capability,
) -> None:
    decision = _decision(
        tmp_path,
        _profile(tmp_path, PermissionProfileName.DANGER_FULL_ACCESS),
        operation=operation,
        capability=capability,
        resource_type="file",
        identifier=identifier,
    )

    assert decision.outcome is DecisionOutcome.DENY
    assert "protected_path" in decision.rule_ids[0]


def test_env_examples_are_not_treated_as_secrets(tmp_path: Path) -> None:
    profile = _profile(tmp_path, PermissionProfileName.WORKSPACE_WRITE)

    assert profile.matching_protected_rule(tmp_path / ".env.example") is None


def test_user_protected_rule_can_only_add_a_deny_boundary(tmp_path: Path) -> None:
    profile = PermissionProfile(
        profile=PermissionProfileName.DANGER_FULL_ACCESS,
        workspace_roots=(tmp_path,),
        protected_paths=(ProtectedPathRule("private/**"),),
    )

    decision = _decision(
        tmp_path,
        profile,
        operation=OperationKind.READ_FILE,
        capability=Capability.READ_WORKSPACE,
        resource_type="file",
        identifier="private/data.txt",
    )

    assert decision.outcome is DecisionOutcome.DENY


def test_symlink_to_protected_path_is_denied(tmp_path: Path) -> None:
    protected = tmp_path / ".singularity"
    protected.mkdir()
    link = tmp_path / "state-link"
    try:
        link.symlink_to(protected, target_is_directory=True)
    except OSError:
        pytest.skip("symlink creation is unavailable")

    decision = _decision(
        tmp_path,
        _profile(tmp_path, PermissionProfileName.DANGER_FULL_ACCESS),
        operation=OperationKind.READ_FILE,
        capability=Capability.READ_WORKSPACE,
        resource_type="file",
        identifier="state-link/state.json",
    )

    assert decision.outcome is DecisionOutcome.DENY
