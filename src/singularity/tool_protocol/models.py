from __future__ import annotations

import hashlib
import json
from dataclasses import asdict, dataclass, field, is_dataclass
from datetime import UTC, datetime
from enum import Enum
from typing import Any

from singularity.model.models import (
    ModelToolParseStatus,
)
from singularity.tools.models import ToolResult


def _now() -> str:
    return datetime.now(UTC).isoformat()


def _digest(value: Any) -> str:
    text = json.dumps(_to_plain(value), ensure_ascii=False, sort_keys=True, default=str)
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _to_plain(value: Any) -> Any:
    if isinstance(value, Enum):
        return value.value
    if is_dataclass(value):
        return {key: _to_plain(item) for key, item in asdict(value).items()}
    if isinstance(value, list):
        return [_to_plain(item) for item in value]
    if isinstance(value, tuple):
        return [_to_plain(item) for item in value]
    if isinstance(value, set):
        return sorted(_to_plain(item) for item in value)
    if isinstance(value, dict):
        return {str(key): _to_plain(item) for key, item in value.items()}
    return value


def _enum(enum_cls: type[Enum], value: Any) -> Any:
    if value is None:
        return None
    if isinstance(value, enum_cls):
        return value
    return enum_cls(str(value))


class SerializableDataclass:
    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)


@dataclass
class ToolProtocolVersion(SerializableDataclass):
    version: str = "1.0"

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ToolProtocolVersion":
        return cls(version=str(payload.get("version") or "1.0"))


class ToolCallPhase(str, Enum):
    PROPOSED = "proposed"
    VALIDATED = "validated"
    REJECTED = "rejected"
    WAITING_APPROVAL = "waiting_approval"
    APPROVED = "approved"
    SCHEDULED = "scheduled"
    RUNNING = "running"
    SUCCEEDED = "succeeded"
    FAILED = "failed"
    CANCELLED = "cancelled"
    RECOVERED = "recovered"
    RESULT_APPENDED = "result_appended"


class ToolCallFailureKind(str, Enum):
    missing_tool_call_id = "missing_tool_call_id"
    duplicate_tool_call_id = "duplicate_tool_call_id"
    unknown_tool = "unknown_tool"
    disallowed_tool = "disallowed_tool"
    invalid_json = "invalid_json"
    arguments_not_object = "arguments_not_object"
    schema_mismatch = "schema_mismatch"
    protocol_violation = "protocol_violation"
    policy_denied = "policy_denied"
    approval_required = "approval_required"
    approval_denied = "approval_denied"
    sandbox_required = "sandbox_required"
    tool_executor_failed = "tool_executor_failed"
    result_binding_failed = "result_binding_failed"
    replay_detected = "replay_detected"
    conflicting_replay = "conflicting_replay"
    context_append_failed = "context_append_failed"


class ToolExecutionMode(str, Enum):
    SEQUENTIAL = "sequential"
    PARALLEL_READONLY = "parallel_readonly"
    BLOCKED = "blocked"


class ToolProtocolTurnStatus(str, Enum):
    NO_TOOL_CALLS = "no_tool_calls"
    PROCESSED = "processed"
    REJECTED = "rejected"
    PENDING_APPROVAL = "pending_approval"
    RECOVERED = "recovered"
    FAILED = "failed"
    INVALID_ASSISTANT = "invalid_assistant"


@dataclass
class ToolCallEnvelope(SerializableDataclass):
    protocol_version: str
    run_id: str
    session_id: str
    task_id: str
    phase_id: str
    model_request_id: str
    model_response_id: str
    assistant_message_id: str
    tool_call_id: str
    tool_name: str
    raw_arguments: str
    parsed_arguments: dict[str, Any]
    normalized_arguments: dict[str, Any]
    argument_digest: str = ""
    tool_schema_hash: str = ""
    allowed_tool_names: list[str] = field(default_factory=list)
    proposed_at: str = field(default_factory=_now)
    proposed_by_model: bool = True
    parse_status: ModelToolParseStatus = ModelToolParseStatus.VALID
    validation_errors: list[str] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)
    phase: ToolCallPhase = ToolCallPhase.PROPOSED

    def __post_init__(self) -> None:
        self.parse_status = _enum(ModelToolParseStatus, self.parse_status)
        self.phase = _enum(ToolCallPhase, self.phase)
        self.allowed_tool_names = list(self.allowed_tool_names)
        self.validation_errors = list(self.validation_errors)
        self.metadata = dict(self.metadata)
        if not self.argument_digest:
            self.argument_digest = _digest(self.normalized_arguments or self.parsed_arguments or self.raw_arguments)
        if not self.tool_schema_hash:
            self.tool_schema_hash = str(self.metadata.get("tool_schema_hash") or "")

    def to_provider_tool_call(self) -> dict[str, Any]:
        return {
            "id": self.tool_call_id,
            "type": "function",
            "function": {
                "name": self.tool_name,
                "arguments": self.raw_arguments,
            },
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ToolCallEnvelope":
        return cls(
            protocol_version=str(payload.get("protocol_version") or "1.0"),
            run_id=str(payload.get("run_id") or ""),
            session_id=str(payload.get("session_id") or ""),
            task_id=str(payload.get("task_id") or ""),
            phase_id=str(payload.get("phase_id") or ""),
            model_request_id=str(payload.get("model_request_id") or ""),
            model_response_id=str(payload.get("model_response_id") or ""),
            assistant_message_id=str(payload.get("assistant_message_id") or ""),
            tool_call_id=str(payload.get("tool_call_id") or ""),
            tool_name=str(payload.get("tool_name") or ""),
            raw_arguments=str(payload.get("raw_arguments") or "{}"),
            parsed_arguments=dict(payload.get("parsed_arguments") or {}),
            normalized_arguments=dict(payload.get("normalized_arguments") or {}),
            argument_digest=str(payload.get("argument_digest") or ""),
            tool_schema_hash=str(payload.get("tool_schema_hash") or ""),
            allowed_tool_names=list(payload.get("allowed_tool_names") or []),
            proposed_at=str(payload.get("proposed_at") or _now()),
            proposed_by_model=bool(payload.get("proposed_by_model", True)),
            parse_status=payload.get("parse_status") or ModelToolParseStatus.VALID,
            validation_errors=list(payload.get("validation_errors") or []),
            metadata=dict(payload.get("metadata") or {}),
            phase=payload.get("phase") or ToolCallPhase.PROPOSED,
        )


@dataclass
class ToolCallBatch(SerializableDataclass):
    batch_id: str
    run_id: str
    session_id: str
    task_id: str
    phase_id: str
    model_request_id: str
    model_response_id: str
    assistant_message: dict[str, Any]
    tool_calls: list[ToolCallEnvelope] = field(default_factory=list)
    supports_parallel_execution: bool = False
    max_tool_calls: int = 0
    created_at: str = field(default_factory=_now)
    batch_digest: str = ""

    def __post_init__(self) -> None:
        self.tool_calls = [
            call if isinstance(call, ToolCallEnvelope) else ToolCallEnvelope.from_dict(call)
            for call in self.tool_calls
        ]
        if not self.batch_digest:
            self.batch_digest = _digest(
                {
                    "assistant_message": self.assistant_message,
                    "tool_calls": [call.to_dict() for call in self.tool_calls],
                    "supports_parallel_execution": self.supports_parallel_execution,
                    "max_tool_calls": self.max_tool_calls,
                }
            )

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ToolCallBatch":
        return cls(
            batch_id=str(payload.get("batch_id") or ""),
            run_id=str(payload.get("run_id") or ""),
            session_id=str(payload.get("session_id") or ""),
            task_id=str(payload.get("task_id") or ""),
            phase_id=str(payload.get("phase_id") or ""),
            model_request_id=str(payload.get("model_request_id") or ""),
            model_response_id=str(payload.get("model_response_id") or ""),
            assistant_message=dict(payload.get("assistant_message") or {}),
            tool_calls=[ToolCallEnvelope.from_dict(item) for item in payload.get("tool_calls") or []],
            supports_parallel_execution=bool(payload.get("supports_parallel_execution")),
            max_tool_calls=int(payload.get("max_tool_calls") or 0),
            created_at=str(payload.get("created_at") or _now()),
            batch_digest=str(payload.get("batch_digest") or ""),
        )


@dataclass
class ToolExecutionPlan(SerializableDataclass):
    plan_id: str
    batch_id: str
    execution_mode: ToolExecutionMode
    ordered_calls: list[ToolCallEnvelope] = field(default_factory=list)
    parallel_groups: list[list[ToolCallEnvelope]] = field(default_factory=list)
    blocked_calls: list[ToolCallEnvelope] = field(default_factory=list)
    reasons: list[str] = field(default_factory=list)
    requires_approval_count: int = 0
    side_effect_count: int = 0

    def __post_init__(self) -> None:
        self.execution_mode = _enum(ToolExecutionMode, self.execution_mode)
        self.ordered_calls = [
            call if isinstance(call, ToolCallEnvelope) else ToolCallEnvelope.from_dict(call)
            for call in self.ordered_calls
        ]
        self.parallel_groups = [
            [
                call if isinstance(call, ToolCallEnvelope) else ToolCallEnvelope.from_dict(call)
                for call in group
            ]
            for group in self.parallel_groups
        ]
        self.blocked_calls = [
            call if isinstance(call, ToolCallEnvelope) else ToolCallEnvelope.from_dict(call)
            for call in self.blocked_calls
        ]
        self.reasons = list(self.reasons)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ToolExecutionPlan":
        return cls(
            plan_id=str(payload.get("plan_id") or ""),
            batch_id=str(payload.get("batch_id") or ""),
            execution_mode=payload.get("execution_mode") or ToolExecutionMode.SEQUENTIAL,
            ordered_calls=[
                ToolCallEnvelope.from_dict(item)
                for item in payload.get("ordered_calls") or []
            ],
            parallel_groups=[
                [ToolCallEnvelope.from_dict(item) for item in group]
                for group in payload.get("parallel_groups") or []
            ],
            blocked_calls=[
                ToolCallEnvelope.from_dict(item)
                for item in payload.get("blocked_calls") or []
            ],
            reasons=list(payload.get("reasons") or []),
            requires_approval_count=int(payload.get("requires_approval_count") or 0),
            side_effect_count=int(payload.get("side_effect_count") or 0),
        )


@dataclass
class ToolCallRecord(SerializableDataclass):
    record_id: str
    envelope: ToolCallEnvelope
    phase: ToolCallPhase
    previous_phase: ToolCallPhase | None = None
    policy_decision_id: str | None = None
    approval_grant_id: str | None = None
    execution_started_at: str | None = None
    execution_finished_at: str | None = None
    tool_result_digest: str | None = None
    context_message_id: str | None = None
    error_kind: ToolCallFailureKind | None = None
    error_message: str | None = None
    attempts: int = 1
    created_at: str = field(default_factory=_now)
    updated_at: str = field(default_factory=_now)

    def __post_init__(self) -> None:
        self.phase = _enum(ToolCallPhase, self.phase)
        self.previous_phase = _enum(ToolCallPhase, self.previous_phase) if self.previous_phase is not None else None
        self.error_kind = _enum(ToolCallFailureKind, self.error_kind) if self.error_kind is not None else None
        self.envelope = self.envelope if isinstance(self.envelope, ToolCallEnvelope) else ToolCallEnvelope.from_dict(self.envelope)
        self.attempts = max(1, int(self.attempts))

    @property
    def tool_call_id(self) -> str:
        return self.envelope.tool_call_id

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ToolCallRecord":
        return cls(
            record_id=str(payload.get("record_id") or ""),
            envelope=ToolCallEnvelope.from_dict(payload.get("envelope") or {}),
            phase=payload.get("phase") or ToolCallPhase.PROPOSED,
            previous_phase=payload.get("previous_phase"),
            policy_decision_id=payload.get("policy_decision_id"),
            approval_grant_id=payload.get("approval_grant_id"),
            execution_started_at=payload.get("execution_started_at"),
            execution_finished_at=payload.get("execution_finished_at"),
            tool_result_digest=payload.get("tool_result_digest"),
            context_message_id=payload.get("context_message_id"),
            error_kind=payload.get("error_kind"),
            error_message=payload.get("error_message"),
            attempts=int(payload.get("attempts") or 1),
            created_at=str(payload.get("created_at") or _now()),
            updated_at=str(payload.get("updated_at") or _now()),
        )


class ToolObservationVisibility(str, Enum):
    FULL = "full"
    SUMMARY = "summary"
    REFERENCE_ONLY = "reference_only"


@dataclass
class ToolObservationView(SerializableDataclass):
    tool_call_id: str
    tool_name: str
    ok: bool
    status: str
    visibility: ToolObservationVisibility = ToolObservationVisibility.SUMMARY
    content_preview: str = ""
    content_digest: str = ""
    result_ref: str | None = None
    error_code: str | None = None
    error_kind: ToolCallFailureKind | None = None
    reference_ids: list[str] = field(default_factory=list)
    observation_id: str | None = None
    truncated: bool = False
    redacted: bool = False

    def __post_init__(self) -> None:
        self.visibility = _enum(ToolObservationVisibility, self.visibility)
        if isinstance(self.error_kind, str):
            self.error_kind = ToolCallFailureKind(self.error_kind)
        self.reference_ids = list(self.reference_ids)

    @classmethod
    def from_protocol_result(
        cls,
        envelope: "ToolProtocolResultEnvelope",
        *,
        visibility: ToolObservationVisibility | str = ToolObservationVisibility.SUMMARY,
    ) -> "ToolObservationView":
        return cls(
            tool_call_id=envelope.tool_call_id,
            tool_name=envelope.tool_name,
            ok=envelope.ok,
            status=envelope.status,
            visibility=_enum(ToolObservationVisibility, visibility),
            content_preview=envelope.content_preview,
            content_digest=envelope.content_digest,
            result_ref=envelope.raw_result_ref,
            error_code=envelope.error_code,
            error_kind=envelope.error_kind,
            reference_ids=envelope.artifact_refs,
            observation_id=envelope.observation_id,
            truncated=envelope.truncated,
            redacted=envelope.redacted,
        )

    def to_model_payload(self) -> dict[str, Any]:
        payload = {
            "ok": self.ok,
            "tool_name": self.tool_name,
            "tool_call_id": self.tool_call_id,
            "status": self.status,
            "content_digest": self.content_digest,
            "result_ref": self.result_ref,
            "error_code": self.error_code,
            "error_kind": self.error_kind.value if self.error_kind else None,
            "reference_ids": self.reference_ids,
            "observation_id": self.observation_id,
            "truncated": self.truncated,
            "redacted": self.redacted,
        }
        if self.visibility is not ToolObservationVisibility.REFERENCE_ONLY:
            payload["content"] = self.content_preview
            payload["content_preview"] = self.content_preview
        return payload


@dataclass
class ToolProtocolResultEnvelope(SerializableDataclass):
    tool_call_id: str
    tool_name: str
    ok: bool
    status: str
    error_code: str | None = None
    error_kind: ToolCallFailureKind | None = None
    content_preview: str = ""
    content_digest: str = ""
    raw_result_ref: str | None = None
    artifact_refs: list[str] = field(default_factory=list)
    observation_id: str | None = None
    policy_decision_id: str | None = None
    approval_grant_id: str | None = None
    truncated: bool = False
    redacted: bool = False
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        self.artifact_refs = list(self.artifact_refs)
        self.metadata = dict(self.metadata)
        if isinstance(self.error_kind, str):
            self.error_kind = ToolCallFailureKind(self.error_kind)

    def to_observation_view(
        self,
        *,
        visibility: ToolObservationVisibility | str = ToolObservationVisibility.SUMMARY,
    ) -> ToolObservationView:
        return ToolObservationView.from_protocol_result(self, visibility=visibility)

    def to_context_message(self) -> dict[str, Any]:
        return {
            "role": "tool",
            "tool_call_id": self.tool_call_id,
            "name": self.tool_name,
            "content": json.dumps(self.to_observation_view().to_model_payload(), ensure_ascii=False),
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ToolProtocolResultEnvelope":
        error_kind = payload.get("error_kind")
        return cls(
            tool_call_id=str(payload.get("tool_call_id") or ""),
            tool_name=str(payload.get("tool_name") or ""),
            ok=bool(payload.get("ok")),
            status=str(payload.get("status") or "ok"),
            error_code=payload.get("error_code"),
            error_kind=error_kind,
            content_preview=str(payload.get("content_preview") or ""),
            content_digest=str(payload.get("content_digest") or ""),
            raw_result_ref=payload.get("raw_result_ref"),
            artifact_refs=list(payload.get("artifact_refs") or []),
            observation_id=payload.get("observation_id"),
            policy_decision_id=payload.get("policy_decision_id"),
            approval_grant_id=payload.get("approval_grant_id"),
            truncated=bool(payload.get("truncated")),
            redacted=bool(payload.get("redacted")),
            metadata=dict(payload.get("metadata") or {}),
        )


@dataclass
class ToolProtocolTurnResult(SerializableDataclass):
    status: ToolProtocolTurnStatus
    batch_id: str | None = None
    executed_count: int = 0
    failed_count: int = 0
    rejected_count: int = 0
    pending_approval_count: int = 0
    appended_tool_message_count: int = 0
    next_action: str = "continue"
    recovery_report: dict[str, Any] = field(default_factory=dict)
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        self.status = _enum(ToolProtocolTurnStatus, self.status)
        self.metadata = dict(self.metadata)
        self.recovery_report = dict(self.recovery_report)


@dataclass
class ToolProtocolValidationResult(SerializableDataclass):
    valid: bool
    batch: ToolCallBatch | None = None
    errors: list[str] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    assistant_message_valid: bool = True
    protocol_version: str = "1.0"

    def __post_init__(self) -> None:
        if self.batch is not None and not isinstance(self.batch, ToolCallBatch):
            self.batch = ToolCallBatch.from_dict(self.batch)
        self.errors = list(self.errors)
        self.warnings = list(self.warnings)


@dataclass
class ToolProtocolRecoveryReport(SerializableDataclass):
    pending_call_ids: list[str] = field(default_factory=list)
    running_call_ids: list[str] = field(default_factory=list)
    succeeded_but_not_appended_call_ids: list[str] = field(default_factory=list)
    assistant_message_ids_missing_tool_messages: list[str] = field(default_factory=list)
    recovered_call_ids: list[str] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    next_action: str = "request_model"

    def __post_init__(self) -> None:
        self.pending_call_ids = list(self.pending_call_ids)
        self.running_call_ids = list(self.running_call_ids)
        self.succeeded_but_not_appended_call_ids = list(self.succeeded_but_not_appended_call_ids)
        self.assistant_message_ids_missing_tool_messages = list(self.assistant_message_ids_missing_tool_messages)
        self.recovered_call_ids = list(self.recovered_call_ids)
        self.warnings = list(self.warnings)


@dataclass
class ToolProtocolEvent(SerializableDataclass):
    event_id: str
    run_id: str
    batch_id: str | None
    tool_call_id: str | None
    event_type: str
    payload: dict[str, Any] = field(default_factory=dict)
    created_at: str = field(default_factory=_now)

    def __post_init__(self) -> None:
        self.payload = dict(self.payload)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ToolProtocolEvent":
        return cls(
            event_id=str(payload.get("event_id") or ""),
            run_id=str(payload.get("run_id") or ""),
            batch_id=payload.get("batch_id"),
            tool_call_id=payload.get("tool_call_id"),
            event_type=str(payload.get("event_type") or ""),
            payload=dict(payload.get("payload") or {}),
            created_at=str(payload.get("created_at") or _now()),
        )


@dataclass
class ToolProtocolResultBinding(SerializableDataclass):
    binding_id: str
    record_id: str
    tool_call_id: str
    result_id: str
    result: ToolProtocolResultEnvelope | None = None
    raw_result_ref: str | None = None
    context_message_id: str | None = None
    result_digest: str | None = None
    appended: bool = False
    created_at: str = field(default_factory=_now)
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if self.result is not None and not isinstance(self.result, ToolProtocolResultEnvelope):
            self.result = ToolProtocolResultEnvelope.from_dict(self.result)
        self.metadata = dict(self.metadata)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ToolProtocolResultBinding":
        return cls(
            binding_id=str(payload.get("binding_id") or ""),
            record_id=str(payload.get("record_id") or ""),
            tool_call_id=str(payload.get("tool_call_id") or ""),
            result_id=str(payload.get("result_id") or ""),
            result=payload.get("result"),
            raw_result_ref=payload.get("raw_result_ref"),
            context_message_id=payload.get("context_message_id"),
            result_digest=payload.get("result_digest"),
            appended=bool(payload.get("appended")),
            created_at=str(payload.get("created_at") or _now()),
            metadata=dict(payload.get("metadata") or {}),
        )


def envelope_from_tool_result(
    *,
    tool_call: ToolCallEnvelope,
    result: ToolResult,
    status: str,
    content_preview: str,
    content_digest: str,
    raw_result_ref: str | None = None,
    observation_id: str | None = None,
    redacted: bool = False,
    truncated: bool = False,
    error_kind: ToolCallFailureKind | None = None,
    policy_decision_id: str | None = None,
    approval_grant_id: str | None = None,
    metadata: dict[str, Any] | None = None,
) -> ToolProtocolResultEnvelope:
    return ToolProtocolResultEnvelope(
        tool_call_id=tool_call.tool_call_id,
        tool_name=tool_call.tool_name,
        ok=bool(result.ok),
        status=status,
        error_code=result.error_code,
        error_kind=error_kind,
        content_preview=content_preview,
        content_digest=content_digest,
        raw_result_ref=raw_result_ref,
        artifact_refs=list(result.metadata.get("artifact_refs") or []),
        observation_id=observation_id,
        policy_decision_id=policy_decision_id,
        approval_grant_id=approval_grant_id,
        truncated=truncated,
        redacted=redacted,
        metadata=metadata or {},
    )
