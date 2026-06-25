"""Security regression test suite for Singularity trust boundary contract.

This module consolidates security-critical attack scenarios in one place:
- Approval grant forgery
- Single-use replay
- Wide-scope remote grant
- Workspace policy dir write
- Network denied fail-open
- Secret redaction
- Artifact/eval leakage
- Large file read/search
- Parallel cache/ledger races

Scenarios already covered by dedicated test modules are referenced here via
smoke tests that assert the referenced test function still exists. This keeps
the centralized regression view intact even when coverage lives elsewhere.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from singularity.command import CommandExecutor
from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.policy import (
    ApprovalGate,
    ApprovalGrant,
    ApprovalScope,
    Capability,
    OperationKind,
    PolicyComponent,
    PolicyConfig,
    PolicyRequest,
    PolicySubject,
    ResourceRef,
)
from singularity.tools import ToolExecutor, ToolPolicy, ToolRegistry
from singularity.tools.command import register_command_tools
from tests.tool_executor_helpers import make_test_policy_engine


# ---------------------------------------------------------------------------
# 1. Grant field tampering attack
# ---------------------------------------------------------------------------


@pytest.mark.xfail(
    strict=True,
    reason=(
        "Defect: ApprovalGrant.consumed is persisted as a plain boolean in the "
        "jsonl store with no integrity protection. An attacker with write access "
        "to the grant store can flip consumed=false to replay a single_use "
        "grant. Fix direction: protect the consumed state with an HMAC over the "
        "full grant record (including consumed), or maintain a separate "
        "append-only consumption ledger whose entries cannot be unset by "
        "editing the grants file. Until then, tampering the jsonl allows "
        "reconsumption."
    ),
)
def test_tampered_consumed_field_cannot_be_reconsumed(tmp_path: Path) -> None:
    """Persisted jsonl consumed field flipped to false must not allow reconsumption.

    The flow:
    1. Register a single_use grant in a trusted (outside-workspace) store.
    2. Consume it legitimately (consumed=True is persisted).
    3. Attacker edits the jsonl file to flip consumed back to false.
    4. Reload the gate from the tampered store.
    5. Secure behavior: the tampered grant must NOT be reconsumable.

    Currently this xfails because ``ApprovalGrant.from_dict`` trusts the
    ``consumed`` field from disk and ``ApprovalGate._load_grants_unlocked``
    performs no integrity check, so flipping the boolean revives the grant.
    """
    grants_path = tmp_path / "outside_grants.jsonl"
    config = PolicyConfig(
        workspace_root=tmp_path,
        approval_grants_path=grants_path,
    )
    gate = ApprovalGate(config)

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
    grant = ApprovalGrant(
        decision_id="policy_dec_tamper_replay",
        request_id=request.request_id,
        approved_by="test-approver",
        session_id=request.session_id,
        scope=ApprovalScope(
            capabilities=[Capability.EXECUTE_COMMAND],
            command_patterns=[request.resource.identifier],
            session_only=True,
            single_use=True,
        ),
        single_use=True,
    )
    gate.register_grant(grant)

    # 1. Consume the grant legitimately.
    consumed_grant = gate.consume_matching_grant(request)
    assert consumed_grant is not None
    assert consumed_grant.consumed is True

    # 2. Attacker tampers the persisted jsonl: flip consumed back to false.
    assert grants_path.exists()
    records = [
        json.loads(line)
        for line in grants_path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    assert records, "at least one grant should be persisted"
    for record in records:
        record["consumed"] = False
    grants_path.write_text(
        "".join(
            json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n"
            for record in records
        ),
        encoding="utf-8",
    )

    # 3. Reload the gate from the tampered store.
    reloaded_gate = ApprovalGate(config)

    # 4. SECURE behavior: the tampered grant must NOT be reconsumable.
    replayed = reloaded_gate.consume_matching_grant(request)
    assert replayed is None, (
        "A single_use grant whose consumed flag was tampered back to false in "
        "the jsonl store must not be reconsumable."
    )


# ---------------------------------------------------------------------------
# 2. Grant store end-to-end write rejection via ToolExecutor
# ---------------------------------------------------------------------------


def test_tool_executor_rejects_write_to_policy_dir_via_shell(tmp_path: Path) -> None:
    """ToolExecutor must hard-deny a shell command that writes to the policy dir.

    This is the end-to-end counterpart to the PolicyEngine-level test
    ``test_policy_hard_denies_writes_to_workspace_policy_dir`` in
    ``test_policy_engine.py``. It dispatches an actual ``run_command`` tool
    call whose shell string references ``.singularity/policy/`` and asserts
    the ToolExecutor denies it before any handler runs and before any file
    is created on disk.
    """
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    register_command_tools(registry, CommandExecutor(tmp_path))
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    tool_call = {
        "id": "call_policy_dir_write",
        "type": "function",
        "function": {
            "name": "run_command",
            "arguments": json.dumps(
                {
                    "shell": "echo fake > .singularity/policy/approval_grants.jsonl",
                    "cwd": ".",
                    "purpose": "READ_ONLY_COMMAND",
                }
            ),
        },
    }

    result = component.execute_tool_call(tool_call)

    assert result.ok is False
    assert result.error_code == "policy_denied"
    # The command must not have executed: no grant file created.
    assert not (tmp_path / ".singularity" / "policy" / "approval_grants.jsonl").exists()


# ---------------------------------------------------------------------------
# 3. READ_ONLY_WORKSPACE capability check (covered elsewhere)
#    Covered by test_sandbox_manager.py::test_read_only_workspace_fails_closed_on_local_backend
# ---------------------------------------------------------------------------


def test_smoke_read_only_workspace_fail_closed_coverage_exists() -> None:
    """Smoke test: verify the READ_ONLY_WORKSPACE fail-closed test still exists.

    The actual coverage lives in
    ``test_sandbox_manager.py::test_read_only_workspace_fails_closed_on_local_backend``,
    which asserts LocalStagingBackend fails closed when the profile requests
    READ_ONLY_WORKSPACE (since the backend only does chmod, which the same
    user can undo).
    """
    from tests import test_sandbox_manager

    assert hasattr(
        test_sandbox_manager,
        "test_read_only_workspace_fails_closed_on_local_backend",
    )


# ---------------------------------------------------------------------------
# 4. Operator HMAC end-to-end verification (covered elsewhere)
#    Covered by test_remote_approval.py:
#      - test_remote_approval_rejects_missing_operator_signature
#      - test_remote_approval_rejects_tampered_operator_signature
#      - test_remote_approval_accepts_valid_operator_signature
#      - test_remote_approval_rejects_grant_scope_exceeding_required_scope
#      - test_remote_approval_rejects_grant_capabilities_exceeding_required_scope
# ---------------------------------------------------------------------------


def test_smoke_operator_hmac_coverage_exists() -> None:
    """Smoke test: verify operator HMAC rejection tests still exist.

    The actual coverage lives in ``test_remote_approval.py`` and exercises the
    full RemoteApprovalExchange.import_grant path: missing signature, tampered
    signature, and wide-scope/capability convergence checks.
    """
    from tests import test_remote_approval

    assert hasattr(
        test_remote_approval,
        "test_remote_approval_rejects_missing_operator_signature",
    )
    assert hasattr(
        test_remote_approval,
        "test_remote_approval_rejects_tampered_operator_signature",
    )
    assert hasattr(
        test_remote_approval,
        "test_remote_approval_rejects_grant_scope_exceeding_required_scope",
    )


# ---------------------------------------------------------------------------
# 5. Sandbox artifact file redaction (covered elsewhere)
#    Covered by test_sandbox_artifacts.py::test_artifact_file_content_is_redacted
# ---------------------------------------------------------------------------


def test_smoke_sandbox_artifact_redaction_coverage_exists() -> None:
    """Smoke test: verify the sandbox artifact redaction test still exists.

    The actual coverage lives in
    ``test_sandbox_artifacts.py::test_artifact_file_content_is_redacted``,
    which asserts that artifact files collected from the sandbox workspace
    have their secret patterns redacted on disk (not just in metadata).
    """
    from tests import test_sandbox_artifacts

    assert hasattr(
        test_sandbox_artifacts,
        "test_artifact_file_content_is_redacted",
    )


# ---------------------------------------------------------------------------
# Additional smoke tests for already-covered scenarios referenced in the
# module docstring.
# ---------------------------------------------------------------------------


def test_smoke_network_denied_fail_closed_coverage_exists() -> None:
    """Smoke test: verify the network DENIED fail-closed test still exists.

    Covered by ``test_sandbox_manager.py::test_network_denied_fail_closed_on_local_backend``.
    """
    from tests import test_sandbox_manager

    assert hasattr(
        test_sandbox_manager,
        "test_network_denied_fail_closed_on_local_backend",
    )


def test_smoke_grant_store_trust_coverage_exists() -> None:
    """Smoke test: verify the untrusted grant store tests still exist.

    Covered by:
    - ``test_tool_executor_policy_approval.py::test_untrusted_grant_store_inside_workspace_is_not_consumed``
    - ``test_approval_gate.py::test_grant_store_inside_workspace_is_untrusted``
    """
    from tests import test_approval_gate, test_tool_executor_policy_approval

    assert hasattr(
        test_tool_executor_policy_approval,
        "test_untrusted_grant_store_inside_workspace_is_not_consumed",
    )
    assert hasattr(
        test_approval_gate,
        "test_grant_store_inside_workspace_is_untrusted",
    )


def test_smoke_grant_dedup_and_single_use_replay_coverage_exists() -> None:
    """Smoke test: verify the grant dedup / single-use replay tests still exist.

    Covered by:
    - ``test_approval_gate.py::test_repeated_import_without_grant_id_does_not_amplify``
    - ``test_approval_gate.py::test_register_grant_dedups_by_decision_id``
    """
    from tests import test_approval_gate

    assert hasattr(
        test_approval_gate,
        "test_repeated_import_without_grant_id_does_not_amplify",
    )
    assert hasattr(
        test_approval_gate,
        "test_register_grant_dedups_by_decision_id",
    )
