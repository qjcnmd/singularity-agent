"""Append-only, HMAC-chained ledger for approval grant consumption.

Trust boundary: ``ApprovalGrant`` is an authorization declaration (who
approved what scope, when, with operator signature). It carries no
consumption state. ``GrantConsumptionLedger`` is the consumption fact:
an append-only JSONL file whose records are chained by SHA-256 links and
signed with the operator key (HMAC-SHA256). Both live under
``<policy_home>/.singularity/policy/`` outside the model-writable
workspace and are considered control-plane state.

Tamper model:

* Editing any field of a record breaks that record's ``record_hmac``.
* Deleting a record breaks the ``previous_record_hash`` link of the next
  record (or, if the last record was deleted, breaks the head pointer).
* Replaying a record duplicates ``grant_id`` (caught by ``consume``) or
  breaks the chain order (``previous_record_hash`` mismatch).
* Rolling back the last record breaks the head pointer (``record_count``
  and ``last_record_hash`` disagree with the file content).
* Any HMAC mismatch raises ``GrantConsumptionLedgerTamperError`` and the
  gate refuses to consume or reconsume grants (fail-closed).
"""
from __future__ import annotations

import hashlib
import json
import os
from contextlib import contextmanager
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Protocol, cast
from uuid import uuid4

from singularity.policy.config import PolicyConfig, _default_policy_home
from singularity.policy.models import ApprovalGrant, PolicyRequest, stable_hash
from singularity.policy.operator_key import (
    load_operator_key,
    sign_grant,
    verify_grant_signature,
)


class _FcntlModule(Protocol):
    LOCK_EX: int
    LOCK_UN: int

    def flock(self, file_descriptor: int, operation: int) -> None:
        ...


class GrantConsumptionLedgerTamperError(RuntimeError):
    """Raised when the consumption ledger HMAC chain or head pointer is broken."""


class GrantAlreadyConsumedError(RuntimeError):
    """Raised when attempting to consume a grant that is already recorded as consumed."""


@dataclass(frozen=True)
class GrantConsumptionRecord:
    """A single signed consumption fact in the append-only ledger.

    Fields:
        record_id: uuid4 hex, unique per consumption event.
        grant_id: the consumed grant's id.
        decision_id: the decision that authorized the grant.
        request_id: the policy request that triggered the consumption.
        request_digest: stable_hash of the request that consumed the
            grant. Binds the consumption to a specific request so a
            stolen record cannot be replayed against a different
            request/session.
        session_id: the session that consumed the grant.
        consumed_at: ISO8601 UTC timestamp.
        previous_record_hash: SHA-256 of the previous record's wire
            format (empty string for the first record). Chains records
            so deletion/reordering is detectable.
        record_hmac: HMAC-SHA256 over ``canonical_payload()`` (excludes
            ``record_hmac``) using the operator key. Authenticates the
            record and prevents tampering with any field.
    """

    record_id: str
    grant_id: str
    decision_id: str
    request_id: str
    request_digest: str
    session_id: str
    consumed_at: str
    previous_record_hash: str
    record_hmac: str

    def canonical_payload(self) -> dict[str, Any]:
        """Return the signed payload (excludes ``record_hmac``)."""
        return {
            "record_id": self.record_id,
            "grant_id": self.grant_id,
            "decision_id": self.decision_id,
            "request_id": self.request_id,
            "request_digest": self.request_digest,
            "session_id": self.session_id,
            "consumed_at": self.consumed_at,
            "previous_record_hash": self.previous_record_hash,
        }

    def to_dict(self) -> dict[str, Any]:
        return {
            **self.canonical_payload(),
            "record_hmac": self.record_hmac,
        }

    def to_jsonl_line(self) -> str:
        """Return the canonical wire format used for hashing and persistence."""
        return json.dumps(self.to_dict(), ensure_ascii=False, sort_keys=True, separators=(",", ":"))

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "GrantConsumptionRecord":
        return cls(
            record_id=str(payload["record_id"]),
            grant_id=str(payload["grant_id"]),
            decision_id=str(payload.get("decision_id") or ""),
            request_id=str(payload.get("request_id") or ""),
            request_digest=str(payload.get("request_digest") or ""),
            session_id=str(payload.get("session_id") or ""),
            consumed_at=str(payload.get("consumed_at") or ""),
            previous_record_hash=str(payload.get("previous_record_hash") or ""),
            record_hmac=str(payload.get("record_hmac") or ""),
        )


@dataclass(frozen=True)
class _LedgerHead:
    """Head pointer persisted alongside the JSONL to detect truncation/rollback."""

    last_record_id: str
    last_record_hash: str
    record_count: int
    head_hmac: str

    def canonical_payload(self) -> dict[str, Any]:
        return {
            "last_record_id": self.last_record_id,
            "last_record_hash": self.last_record_hash,
            "record_count": self.record_count,
        }

    def to_dict(self) -> dict[str, Any]:
        return {**self.canonical_payload(), "head_hmac": self.head_hmac}

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "_LedgerHead":
        return cls(
            last_record_id=str(payload.get("last_record_id") or ""),
            last_record_hash=str(payload.get("last_record_hash") or ""),
            record_count=int(payload.get("record_count") or 0),
            head_hmac=str(payload.get("head_hmac") or ""),
        )


class GrantConsumptionLedger:
    """Append-only HMAC-chained ledger of consumed approval grants."""

    def __init__(self, config: PolicyConfig, *, trace: Any | None = None) -> None:
        self._config = config
        self._trace = trace
        self._ledger_path = _ledger_path(config)
        self._ledger_path.parent.mkdir(parents=True, exist_ok=True)
        self._lock_path = self._ledger_path.with_suffix(self._ledger_path.suffix + ".lock")
        self._head_path = self._ledger_path.with_suffix(self._ledger_path.suffix + ".head.json")
        self._operator_key: bytes | None = None

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------
    def is_consumed(self, grant_id: str) -> bool:
        """Return True iff ``grant_id`` has a verified consumption record.

        Fail-closed: if the ledger file exists but the HMAC chain or head
        pointer is broken, raises ``GrantConsumptionLedgerTamperError``.
        An empty/missing ledger returns False without requiring the
        operator key.
        """
        with _file_lock(self._lock_path):
            records = self._load_records_unlocked()
        return any(record.grant_id == grant_id for record in records)

    def consume(self, grant: ApprovalGrant, *, request: PolicyRequest | None = None) -> GrantConsumptionRecord:
        """Append a signed consumption record for ``grant``.

        Raises ``GrantAlreadyConsumedError`` if ``grant.grant_id`` is
        already recorded as consumed. Raises
        ``GrantConsumptionLedgerTamperError`` if the existing chain is
        broken. Requires the operator key to be loadable (fail-closed if
        missing).
        """
        with _file_lock(self._lock_path):
            records = self._load_records_unlocked()
            if any(record.grant_id == grant.grant_id for record in records):
                raise GrantAlreadyConsumedError(
                    f"Grant {grant.grant_id} is already recorded as consumed."
                )
            record = self._build_record(grant, records=records, request=request)
            self._append_record_unlocked(record, count=len(records) + 1)
            return record

    def record_for(self, grant_id: str) -> GrantConsumptionRecord | None:
        """Return the verified consumption record for ``grant_id`` or None."""
        with _file_lock(self._lock_path):
            records = self._load_records_unlocked()
        for record in records:
            if record.grant_id == grant_id:
                return record
        return None

    def all_records(self) -> list[GrantConsumptionRecord]:
        """Return all verified consumption records in append order."""
        with _file_lock(self._lock_path):
            return self._load_records_unlocked()

    def ledger_path(self) -> Path:
        return self._ledger_path

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------
    def _load_operator_key(self) -> bytes:
        if self._operator_key is None:
            key_path = self._config.operator_key_path
            # PolicyConfig.__post_init__ guarantees operator_key_path is set
            # to a Path at runtime, but the field annotation is
            # ``Path | str | None``. Coerce to Path when present; otherwise
            # let load_operator_key fall back to the default path.
            self._operator_key = load_operator_key(
                Path(key_path) if key_path is not None else None
            )
        return self._operator_key

    def _load_records_unlocked(self) -> list[GrantConsumptionRecord]:
        if not self._ledger_path.exists():
            # Fresh ledger: head pointer must also be absent for a clean state.
            if self._head_path.exists():
                raise GrantConsumptionLedgerTamperError(
                    "Consumption ledger is missing but head pointer exists — possible rollback."
                )
            return []
        raw_text = self._ledger_path.read_text(encoding="utf-8")
        records: list[GrantConsumptionRecord] = []
        for line in raw_text.splitlines():
            if not line.strip():
                continue
            try:
                payload = json.loads(line)
            except json.JSONDecodeError as exc:
                raise GrantConsumptionLedgerTamperError(
                    f"Consumption ledger line is not valid JSON: {exc}"
                ) from exc
            records.append(GrantConsumptionRecord.from_dict(payload))
        self._verify_chain_unlocked(records)
        return records

    def _verify_chain_unlocked(self, records: list[GrantConsumptionRecord]) -> None:
        if not records:
            # No records: head pointer must also be absent.
            if self._head_path.exists():
                raise GrantConsumptionLedgerTamperError(
                    "Consumption ledger is empty but head pointer exists — possible rollback."
                )
            return
        operator_key = self._load_operator_key()
        previous_wire_hash = ""
        for index, record in enumerate(records):
            if record.previous_record_hash != previous_wire_hash:
                raise GrantConsumptionLedgerTamperError(
                    f"Consumption ledger record {record.record_id} has a broken "
                    f"previous_record_hash link (expected {previous_wire_hash[:12]}…, "
                    f"got {record.previous_record_hash[:12]}…)."
                )
            expected_hmac = sign_grant(record.canonical_payload(), operator_key)
            if not verify_grant_signature(record.canonical_payload(), record.record_hmac, operator_key):
                raise GrantConsumptionLedgerTamperError(
                    f"Consumption ledger record {record.record_id} has an invalid HMAC "
                    f"(expected {expected_hmac[:12]}…, got {record.record_hmac[:12]}…)."
                )
            previous_wire_hash = hashlib.sha256(record.to_jsonl_line().encode("utf-8")).hexdigest()
        # Head pointer check: must agree with the last record's wire hash and record count.
        head = self._read_head_unlocked()
        if head is None:
            raise GrantConsumptionLedgerTamperError(
                "Consumption ledger has records but head pointer is missing — possible rollback."
            )
        if head.record_count != len(records):
            raise GrantConsumptionLedgerTamperError(
                f"Consumption ledger head pointer count ({head.record_count}) does not match "
                f"file record count ({len(records)})."
            )
        if head.last_record_hash != previous_wire_hash:
            raise GrantConsumptionLedgerTamperError(
                "Consumption ledger head pointer hash does not match the last record."
            )
        if records and head.last_record_id != records[-1].record_id:
            raise GrantConsumptionLedgerTamperError(
                "Consumption ledger head pointer id does not match the last record."
            )

    def _build_record(
        self,
        grant: ApprovalGrant,
        *,
        records: list[GrantConsumptionRecord],
        request: PolicyRequest | None,
    ) -> GrantConsumptionRecord:
        operator_key = self._load_operator_key()
        if records:
            previous_wire_hash = hashlib.sha256(
                records[-1].to_jsonl_line().encode("utf-8")
            ).hexdigest()
        else:
            previous_wire_hash = ""
        if request is not None:
            request_digest = stable_hash(
                {
                    "request_id": request.request_id,
                    "capability": request.capability.value,
                    "resource_identifier": request.resource.identifier,
                    "session_id": request.session_id,
                }
            )
            session_id = request.session_id
            request_id = request.request_id
        else:
            request_digest = stable_hash(
                {
                    "grant_id": grant.grant_id,
                    "decision_id": grant.decision_id,
                    "request_id": grant.request_id,
                    "session_id": grant.session_id or "",
                }
            )
            session_id = grant.session_id or ""
            request_id = grant.request_id
        record_id = uuid4().hex
        consumed_at = datetime.now(UTC).isoformat()
        canonical = {
            "record_id": record_id,
            "grant_id": grant.grant_id,
            "decision_id": grant.decision_id,
            "request_id": request_id,
            "request_digest": request_digest,
            "session_id": session_id,
            "consumed_at": consumed_at,
            "previous_record_hash": previous_wire_hash,
        }
        record_hmac = sign_grant(canonical, operator_key)
        return GrantConsumptionRecord(
            record_id=record_id,
            grant_id=grant.grant_id,
            decision_id=grant.decision_id,
            request_id=request_id,
            request_digest=request_digest,
            session_id=session_id,
            consumed_at=consumed_at,
            previous_record_hash=previous_wire_hash,
            record_hmac=record_hmac,
        )

    def _append_record_unlocked(self, record: GrantConsumptionRecord, *, count: int) -> None:
        line = record.to_jsonl_line() + "\n"
        # Append-only: never rewrite existing lines.
        with self._ledger_path.open("a", encoding="utf-8") as handle:
            handle.write(line)
        self._write_head_unlocked(record, count=count)

    def _read_head_unlocked(self) -> _LedgerHead | None:
        if not self._head_path.exists():
            return None
        try:
            payload = json.loads(self._head_path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as exc:
            raise GrantConsumptionLedgerTamperError(
                f"Consumption ledger head pointer is not valid JSON: {exc}"
            ) from exc
        head = _LedgerHead.from_dict(payload)
        operator_key = self._load_operator_key()
        if not verify_grant_signature(head.canonical_payload(), head.head_hmac, operator_key):
            raise GrantConsumptionLedgerTamperError(
                "Consumption ledger head pointer HMAC is invalid."
            )
        return head

    def _write_head_unlocked(self, record: GrantConsumptionRecord, *, count: int) -> None:
        operator_key = self._load_operator_key()
        last_hash = hashlib.sha256(record.to_jsonl_line().encode("utf-8")).hexdigest()
        head = _LedgerHead(
            last_record_id=record.record_id,
            last_record_hash=last_hash,
            record_count=count,
            head_hmac="",
        )
        head_hmac = sign_grant(head.canonical_payload(), operator_key)
        signed = _LedgerHead(
            last_record_id=head.last_record_id,
            last_record_hash=head.last_record_hash,
            record_count=head.record_count,
            head_hmac=head_hmac,
        )
        self._head_path.parent.mkdir(parents=True, exist_ok=True)
        tmp_path = self._head_path.with_suffix(self._head_path.suffix + ".tmp")
        tmp_path.write_text(
            json.dumps(signed.to_dict(), ensure_ascii=False, sort_keys=True),
            encoding="utf-8",
        )
        tmp_path.replace(self._head_path)


def _ledger_path(config: PolicyConfig) -> Path:
    """Return the resolved consumption ledger path.

    Trust boundary: defaults to
    ``<policy_home>/.singularity/policy/grant_consumption_ledger.jsonl``
    outside the model-writable workspace. Explicit configuration overrides
    the default for backward compatibility and test isolation.
    """
    if config.consumption_ledger_path is None:
        return (
            _default_policy_home()
            / ".singularity"
            / "policy"
            / "grant_consumption_ledger.jsonl"
        )
    return Path(config.consumption_ledger_path)


@contextmanager
def _file_lock(path: Path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+b") as handle:
        _lock_file(handle)
        try:
            yield
        finally:
            _unlock_file(handle)


def _lock_file(handle: Any) -> None:
    if os.name == "nt":
        import msvcrt

        handle.seek(0)
        msvcrt.locking(handle.fileno(), msvcrt.LK_LOCK, 1)
        return
    import fcntl

    posix_lock = cast(_FcntlModule, fcntl)
    posix_lock.flock(handle.fileno(), posix_lock.LOCK_EX)


def _unlock_file(handle: Any) -> None:
    if os.name == "nt":
        import msvcrt

        handle.seek(0)
        msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
        return
    import fcntl

    posix_lock = cast(_FcntlModule, fcntl)
    posix_lock.flock(handle.fileno(), posix_lock.LOCK_UN)
