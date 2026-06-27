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
    GrantConsumptionLedger,
    GrantConsumptionLedgerTamperError,
    OperationKind,
    PolicyComponent,
    PolicyConfig,
    PolicyRequest,
    PolicySubject,
    ResourceRef,
)
from singularity.tools import ToolExecutor, ToolPolicy, ToolRegistry
from singularity.tools.command import register_command_tools
from tests.tool_executor_helpers import make_ledger_test_config, make_test_policy_engine


# ---------------------------------------------------------------------------
# 1. Grant field tampering attack
# ---------------------------------------------------------------------------


def test_tampered_consumed_field_cannot_be_reconsumed(tmp_path: Path) -> None:
    """Persisted jsonl consumed field flipped to false must not allow reconsumption.

    The flow:
    1. Register a single_use grant in a trusted (outside-workspace) store.
    2. Consume it legitimately (a signed consumption record is appended to the
       append-only GrantConsumptionLedger).
    3. Attacker edits the jsonl grants file: ApprovalGrant no longer carries a
       ``consumed`` field, but the attacker re-adds a forged ``consumed: false``
       entry (or deletes the grant and re-imports it) trying to revive it.
    4. Reload the gate from the tampered store.
    5. Secure behavior: the tampered grant must NOT be reconsumable, because
       the consumption truth lives in the HMAC-chained ledger, not in the
       grants file.
    """
    grants_path = tmp_path / "outside_grants.jsonl"
    ledger_path = tmp_path / "outside_ledger.jsonl"
    config = make_ledger_test_config(
        tmp_path,
        grants_path=grants_path,
        ledger_path=ledger_path,
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
    assert gate.is_grant_consumed(consumed_grant.grant_id) is True

    # 2. Attacker tampers the persisted jsonl grants file: re-add a forged
    #    ``consumed: false`` entry (legacy field) and rewrite the line. Under
    #    the new trust model this is a no-op for consumption truth, which
    #    lives in the HMAC-chained ledger.
    assert grants_path.exists()
    records = [
        json.loads(line)
        for line in grants_path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    assert records, "at least one grant should be persisted"
    for record in records:
        record["consumed"] = False  # forged legacy field; from_dict ignores it
    grants_path.write_text(
        "".join(
            json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n"
            for record in records
        ),
        encoding="utf-8",
    )

    # 3. Reload the gate from the tampered grants store. The ledger file is
    #    untouched and still chains a signed consumption record for this
    #    grant_id, so the reloaded gate must refuse to reconsume.
    reloaded_gate = ApprovalGate(config)

    # 4. SECURE behavior: the tampered grant must NOT be reconsumable.
    replayed = reloaded_gate.consume_matching_grant(request)
    assert replayed is None, (
        "A single_use grant whose jsonl record was tampered must not be "
        "reconsumable: consumption truth lives in the HMAC-chained ledger."
    )


# ---------------------------------------------------------------------------
# 1b. GrantConsumptionLedger tamper / replay / rollback / cross-session
# ---------------------------------------------------------------------------


def _make_grant_and_request(tmp_path: Path, *, session_id: str = "session"):
    request = PolicyRequest(
        session_id=session_id,
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
        decision_id=f"policy_dec_{session_id}",
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
    return grant, request


def test_ledger_tampering_breaks_hmac(tmp_path: Path) -> None:
    """Editing any field of a consumption record breaks its HMAC and fail-closes."""
    config = make_ledger_test_config(tmp_path)
    gate = ApprovalGate(config)
    grant, request = _make_grant_and_request(tmp_path)
    gate.register_grant(grant)
    gate.consume_matching_grant(request)
    ledger_path = config.consumption_ledger_path
    assert ledger_path.exists()

    # Tamper: flip a character in consumed_at.
    lines = ledger_path.read_text(encoding="utf-8").splitlines()
    assert lines, "ledger should have at least one record"
    record = json.loads(lines[0])
    record["consumed_at"] = record["consumed_at"] + "X"
    lines[0] = json.dumps(record, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    ledger_path.write_text("\n".join(lines) + "\n", encoding="utf-8")

    reloaded = GrantConsumptionLedger(config)
    with pytest.raises(GrantConsumptionLedgerTamperError):
        reloaded.is_consumed(grant.grant_id)


def test_ledger_deletion_breaks_chain(tmp_path: Path) -> None:
    """Deleting the last record breaks the head pointer; truncating mid-file breaks the chain."""
    config = make_ledger_test_config(tmp_path)
    gate = ApprovalGate(config)
    grant_a, request_a = _make_grant_and_request(tmp_path, session_id="session_a")
    grant_b, request_b = _make_grant_and_request(tmp_path, session_id="session_b")
    gate.register_grant(grant_a)
    gate.register_grant(grant_b)
    gate.consume_matching_grant(request_a)
    gate.consume_matching_grant(request_b)

    ledger_path = config.consumption_ledger_path
    lines = ledger_path.read_text(encoding="utf-8").splitlines()
    assert len(lines) >= 2
    # Delete the last line (truncate the chain).
    ledger_path.write_text("\n".join(lines[:-1]) + "\n", encoding="utf-8")

    reloaded = GrantConsumptionLedger(config)
    with pytest.raises(GrantConsumptionLedgerTamperError):
        reloaded.is_consumed(grant_a.grant_id)


def test_ledger_replay_rejected(tmp_path: Path) -> None:
    """Replaying an old record line is rejected because grant_id is already consumed."""
    config = make_ledger_test_config(tmp_path)
    gate = ApprovalGate(config)
    grant, request = _make_grant_and_request(tmp_path)
    gate.register_grant(grant)
    gate.consume_matching_grant(request)
    ledger_path = config.consumption_ledger_path

    # Replay: duplicate the consumed record line.
    lines = ledger_path.read_text(encoding="utf-8").splitlines()
    assert lines
    ledger_path.write_text(lines[0] + "\n" + lines[0] + "\n", encoding="utf-8")

    reloaded = GrantConsumptionLedger(config)
    # Chain is intact (the duplicated line still passes HMAC because the
    # record content is unchanged) but the head pointer disagrees with the
    # new file content (record_count mismatch), so tamper is detected.
    with pytest.raises(GrantConsumptionLedgerTamperError):
        reloaded.all_records()


def test_ledger_rollback_detected(tmp_path: Path) -> None:
    """Rolling back the ledger (deleting head pointer or truncating) is fail-closed."""
    config = make_ledger_test_config(tmp_path)
    gate = ApprovalGate(config)
    grant, request = _make_grant_and_request(tmp_path)
    gate.register_grant(grant)
    gate.consume_matching_grant(request)

    # Rollback attempt: delete the head pointer file but leave the ledger.
    head_path = config.consumption_ledger_path.with_suffix(
        config.consumption_ledger_path.suffix + ".head.json"
    )
    assert head_path.exists()
    head_path.unlink()

    reloaded = GrantConsumptionLedger(config)
    with pytest.raises(GrantConsumptionLedgerTamperError):
        reloaded.is_consumed(grant.grant_id)


def test_duplicate_consumption_rejected(tmp_path: Path) -> None:
    """Consuming the same grant twice must return None on the second attempt."""
    config = make_ledger_test_config(tmp_path)
    gate = ApprovalGate(config)
    grant, request = _make_grant_and_request(tmp_path)
    gate.register_grant(grant)

    first = gate.consume_matching_grant(request)
    assert first is not None
    second = gate.consume_matching_grant(request)
    assert second is None, "A single_use grant must not be consumable twice."


def test_cross_session_replay_rejected(tmp_path: Path) -> None:
    """A grant consumed in session A cannot be reconsumed in session B.

    ``ApprovalScope.session_only`` already blocks ``matches()`` across
    sessions, and ``GrantConsumptionLedger.is_consumed`` independently
    records the consuming ``session_id`` in the signed record. This test
    verifies the end-to-end behavior: even if an attacker copies the grant
    into a fresh store for session B, the ledger's record for that
    ``grant_id`` (bound to session A) still blocks reconsumption.
    """
    config = make_ledger_test_config(tmp_path)
    gate = ApprovalGate(config)
    grant, request_a = _make_grant_and_request(tmp_path, session_id="session_a")
    gate.register_grant(grant)
    gate.consume_matching_grant(request_a)
    assert gate.is_grant_consumed(grant.grant_id) is True

    # Session B attacker reuses the same grant_id.
    request_b = PolicyRequest(
        session_id="session_b",
        task_id=request_a.task_id,
        phase_id=request_a.phase_id,
        action_id=request_a.action_id,
        component=request_a.component,
        operation=request_a.operation,
        capability=request_a.capability,
        subject=request_a.subject,
        resource=request_a.resource,
        reason=request_a.reason,
        workspace_root=request_a.workspace_root,
    )
    replayed = gate.consume_matching_grant(request_b)
    assert replayed is None, (
        "Cross-session replay of a single_use grant must be refused: the "
        "ledger's consumption record for this grant_id is signed and append-only."
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
# 3. Native sandbox availability fail-closed check (covered elsewhere)
#    Covered by test_sandbox_manager.py::test_manager_fails_closed_without_available_backend_and_does_not_prepare
# ---------------------------------------------------------------------------


def test_smoke_backend_unavailable_fail_closed_coverage_exists() -> None:
    """Smoke test: verify the native sandbox fail-closed test still exists.

    The actual coverage lives in
    ``test_sandbox_manager.py::test_manager_fails_closed_without_available_backend_and_does_not_prepare``,
    which asserts an unavailable OS-native backend returns ``backend_unavailable``
    and never prepares or starts the command.
    """
    from tests import test_sandbox_manager

    assert hasattr(
        test_sandbox_manager,
        "test_manager_fails_closed_without_available_backend_and_does_not_prepare",
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

    Covered by ``test_sandbox_manager.py::test_manager_enforces_resolved_request_capabilities_before_prepare``.
    """
    from tests import test_sandbox_manager

    assert hasattr(
        test_sandbox_manager,
        "test_manager_enforces_resolved_request_capabilities_before_prepare",
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
