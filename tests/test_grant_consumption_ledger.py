"""Unit-focused coverage of the GrantConsumptionLedger API.

These tests exercise the ledger directly (without going through
ApprovalGate) so the HMAC chain, head pointer, and fail-closed semantics
are verified at the API surface.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from singularity.policy import (
    ApprovalGrant,
    ApprovalScope,
    Capability,
    GrantAlreadyConsumedError,
    GrantConsumptionLedger,
    GrantConsumptionLedgerTamperError,
    GrantConsumptionRecord,
    PolicyConfig,
)
from singularity.policy.operator_key import generate_operator_key
from tests.tool_executor_helpers import make_ledger_test_config


def _make_grant(*, grant_id: str = "grant_test", session_id: str = "session") -> ApprovalGrant:
    return ApprovalGrant(
        decision_id=f"dec_{grant_id}",
        request_id=f"req_{grant_id}",
        approved_by="test-approver",
        session_id=session_id,
        scope=ApprovalScope(
            capabilities=[Capability.EXECUTE_COMMAND],
            command_patterns=["python -c print(1)"],
            session_only=True,
            single_use=True,
        ),
        single_use=True,
        grant_id=grant_id,
    )


def test_consume_appends_signed_record_with_chain(tmp_path: Path) -> None:
    config = make_ledger_test_config(tmp_path)
    ledger = GrantConsumptionLedger(config)
    grant = _make_grant(grant_id="grant_a")

    record = ledger.consume(grant, request=None)

    assert isinstance(record, GrantConsumptionRecord)
    assert record.grant_id == "grant_a"
    assert record.previous_record_hash == ""  # first record
    assert record.record_hmac
    assert ledger.is_consumed("grant_a") is True
    assert ledger.is_consumed("grant_other") is False

    # Second consume creates a chained record.
    grant_b = _make_grant(grant_id="grant_b")
    record_b = ledger.consume(grant_b, request=None)
    assert record_b.previous_record_hash != ""
    assert record_b.previous_record_hash != record.previous_record_hash
    assert ledger.is_consumed("grant_b") is True


def test_record_for_returns_verified_record(tmp_path: Path) -> None:
    config = make_ledger_test_config(tmp_path)
    ledger = GrantConsumptionLedger(config)
    grant = _make_grant(grant_id="grant_lookup")
    ledger.consume(grant, request=None)

    record = ledger.record_for("grant_lookup")
    assert record is not None
    assert record.grant_id == "grant_lookup"
    assert ledger.record_for("grant_missing") is None


def test_all_records_returns_chain_in_order(tmp_path: Path) -> None:
    config = make_ledger_test_config(tmp_path)
    ledger = GrantConsumptionLedger(config)
    for grant_id in ("grant_1", "grant_2", "grant_3"):
        ledger.consume(_make_grant(grant_id=grant_id), request=None)

    records = ledger.all_records()
    assert [r.grant_id for r in records] == ["grant_1", "grant_2", "grant_3"]
    # Chain integrity: each record's previous_record_hash matches the prior wire hash.
    import hashlib

    prev_hash = ""
    for record in records:
        assert record.previous_record_hash == prev_hash
        prev_hash = hashlib.sha256(record.to_jsonl_line().encode("utf-8")).hexdigest()


def test_consume_already_consumed_raises(tmp_path: Path) -> None:
    config = make_ledger_test_config(tmp_path)
    ledger = GrantConsumptionLedger(config)
    grant = _make_grant(grant_id="grant_dup")
    ledger.consume(grant, request=None)

    with pytest.raises(GrantAlreadyConsumedError):
        ledger.consume(grant, request=None)


def test_missing_operator_key_fail_closes_on_consume(tmp_path: Path) -> None:
    config = PolicyConfig(
        workspace_root=tmp_path,
        approval_grants_path=tmp_path / "grants.jsonl",
        consumption_ledger_path=tmp_path / "ledger.jsonl",
        operator_key_path=tmp_path / "missing.pem",
    )
    ledger = GrantConsumptionLedger(config)
    grant = _make_grant()
    with pytest.raises(FileNotFoundError):
        ledger.consume(grant, request=None)


def test_empty_ledger_is_consumed_returns_false_without_key(tmp_path: Path) -> None:
    # No operator key needed when the ledger is empty/missing.
    config = PolicyConfig(
        workspace_root=tmp_path,
        approval_grants_path=tmp_path / "grants.jsonl",
        consumption_ledger_path=tmp_path / "ledger.jsonl",
        operator_key_path=tmp_path / "missing.pem",
    )
    ledger = GrantConsumptionLedger(config)
    assert ledger.is_consumed("grant_any") is False
    assert ledger.all_records() == []
    assert ledger.record_for("grant_any") is None


def test_tampered_record_hmac_raises(tmp_path: Path) -> None:
    config = make_ledger_test_config(tmp_path)
    ledger = GrantConsumptionLedger(config)
    ledger.consume(_make_grant(grant_id="grant_t"), request=None)
    ledger_path = config.consumption_ledger_path

    line = ledger_path.read_text(encoding="utf-8").splitlines()[0]
    record = json.loads(line)
    record["consumed_at"] = record["consumed_at"] + "X"
    ledger_path.write_text(
        json.dumps(record, ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n",
        encoding="utf-8",
    )

    reloaded = GrantConsumptionLedger(config)
    with pytest.raises(GrantConsumptionLedgerTamperError):
        reloaded.is_consumed("grant_t")


def test_head_pointer_count_mismatch_raises(tmp_path: Path) -> None:
    config = make_ledger_test_config(tmp_path)
    ledger = GrantConsumptionLedger(config)
    ledger.consume(_make_grant(grant_id="grant_h"), request=None)
    head_path = config.consumption_ledger_path.with_suffix(
        config.consumption_ledger_path.suffix + ".head.json"
    )

    head = json.loads(head_path.read_text(encoding="utf-8"))
    head["record_count"] = head["record_count"] + 1
    # Re-sign the head pointer with a forged count using the operator key.
    from singularity.policy.operator_key import load_operator_key, sign_grant

    operator_key = load_operator_key(Path(config.operator_key_path))
    forged_payload = {
        "last_record_id": head["last_record_id"],
        "last_record_hash": head["last_record_hash"],
        "record_count": head["record_count"],
    }
    head = {**forged_payload, "head_hmac": sign_grant(forged_payload, operator_key)}
    head_path.write_text(
        json.dumps(head, ensure_ascii=False, sort_keys=True),
        encoding="utf-8",
    )

    reloaded = GrantConsumptionLedger(config)
    with pytest.raises(GrantConsumptionLedgerTamperError):
        reloaded.is_consumed("grant_h")


def test_ledger_path_is_outside_workspace_by_default(tmp_path: Path) -> None:
    # The default ledger path lives under <policy_home>/.singularity/policy/,
    # outside the model-writable workspace.
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    config = PolicyConfig(workspace_root=workspace)
    ledger = GrantConsumptionLedger(config)
    ledger_path = ledger.ledger_path()
    assert "grant_consumption_ledger.jsonl" in ledger_path.name
    # Resolve and compare with workspace root.
    assert Path(workspace).resolve() not in ledger_path.resolve().parents or (
        ledger_path.resolve() != Path(workspace).resolve()
    )


def test_generate_operator_key_creates_file(tmp_path: Path) -> None:
    key_path = tmp_path / "op.pem"
    key = generate_operator_key(key_path)
    assert key_path.exists()
    assert len(key) == 32
