from __future__ import annotations

import json
from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import Enum
from typing import Any
from uuid import uuid4


class ControlCommand(str, Enum):
    CANCEL = "cancel"
    CONTINUE = "continue"
    REVISE = "revise"
    ABORT = "abort"


class InteractionMode(str, Enum):
    INTERACTIVE = "interactive"
    NON_INTERACTIVE = "non_interactive"


class OutcomeStatus(str, Enum):
    SUCCESS = "success"
    PARTIAL_SUCCESS = "partial_success"
    FAILED = "failed"
    CANCELLED = "cancelled"
    BLOCKED = "blocked"
    UNVERIFIED = "unverified"


@dataclass(frozen=True)
class InteractionEvent:
    event_type: str
    summary: str
    component: str = "interaction"
    payload: dict[str, Any] = field(default_factory=dict)
    severity: str = "info"
    event_id: str = field(default_factory=lambda: f"interaction_evt_{uuid4().hex[:12]}")
    run_id: str | None = None
    session_id: str | None = None
    task_id: str | None = None
    phase_id: str | None = None
    action_id: str | None = None
    trace_event_id: str | None = None
    timestamp: datetime = field(default_factory=lambda: datetime.now(UTC))

    def to_dict(self) -> dict[str, Any]:
        return {
            "event_id": self.event_id,
            "event_type": self.event_type,
            "summary": self.summary,
            "component": self.component,
            "payload": self.payload,
            "severity": self.severity,
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "phase_id": self.phase_id,
            "action_id": self.action_id,
            "trace_event_id": self.trace_event_id,
            "timestamp": self.timestamp.isoformat(),
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "InteractionEvent":
        return cls(
            event_id=str(payload.get("event_id") or f"interaction_evt_{uuid4().hex[:12]}"),
            event_type=str(payload["event_type"]),
            summary=str(payload.get("summary") or ""),
            component=str(payload.get("component") or "interaction"),
            payload=dict(payload.get("payload") or {}),
            severity=str(payload.get("severity") or "info"),
            run_id=payload.get("run_id"),
            session_id=payload.get("session_id"),
            task_id=payload.get("task_id"),
            phase_id=payload.get("phase_id"),
            action_id=payload.get("action_id"),
            trace_event_id=payload.get("trace_event_id"),
            timestamp=_datetime(payload.get("timestamp") or datetime.now(UTC)),
        )

    def to_json(self) -> str:
        return json.dumps(self.to_dict(), ensure_ascii=False, sort_keys=True, default=str)

    @classmethod
    def from_json(cls, text: str) -> "InteractionEvent":
        return cls.from_dict(json.loads(text))


@dataclass(frozen=True)
class ProgressEvent:
    phase: str
    status: str
    summary: str
    current: int | None = None
    total: int | None = None
    payload: dict[str, Any] = field(default_factory=dict)
    event_id: str = field(default_factory=lambda: f"progress_evt_{uuid4().hex[:12]}")
    run_id: str | None = None
    session_id: str | None = None
    task_id: str | None = None
    action_id: str | None = None
    timestamp: datetime = field(default_factory=lambda: datetime.now(UTC))

    def to_interaction_event(self) -> InteractionEvent:
        payload = {
            **self.payload,
            "phase": self.phase,
            "status": self.status,
            "current": self.current,
            "total": self.total,
        }
        return InteractionEvent(
            event_id=self.event_id,
            event_type=f"progress.{self.status}",
            summary=self.summary,
            component="progress",
            payload=payload,
            run_id=self.run_id,
            session_id=self.session_id,
            task_id=self.task_id,
            phase_id=self.phase,
            action_id=self.action_id,
            timestamp=self.timestamp,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "event_id": self.event_id,
            "phase": self.phase,
            "status": self.status,
            "summary": self.summary,
            "current": self.current,
            "total": self.total,
            "payload": self.payload,
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "action_id": self.action_id,
            "timestamp": self.timestamp.isoformat(),
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ProgressEvent":
        return cls(
            event_id=str(payload.get("event_id") or f"progress_evt_{uuid4().hex[:12]}"),
            phase=str(payload["phase"]),
            status=str(payload["status"]),
            summary=str(payload.get("summary") or ""),
            current=payload.get("current"),
            total=payload.get("total"),
            payload=dict(payload.get("payload") or {}),
            run_id=payload.get("run_id"),
            session_id=payload.get("session_id"),
            task_id=payload.get("task_id"),
            action_id=payload.get("action_id"),
            timestamp=_datetime(payload.get("timestamp") or datetime.now(UTC)),
        )


@dataclass(frozen=True)
class DecisionPrompt:
    title: str
    message: str
    choices: list[str]
    prompt_id: str = field(default_factory=lambda: f"decision_prompt_{uuid4().hex[:12]}")
    recommended: str | None = None
    default_decision: str | None = None
    risk_level: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)
    allow_freeform: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "prompt_id": self.prompt_id,
            "title": self.title,
            "message": self.message,
            "choices": self.choices,
            "recommended": self.recommended,
            "default_decision": self.default_decision,
            "risk_level": self.risk_level,
            "metadata": self.metadata,
            "allow_freeform": self.allow_freeform,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "DecisionPrompt":
        return cls(
            prompt_id=str(payload.get("prompt_id") or f"decision_prompt_{uuid4().hex[:12]}"),
            title=str(payload.get("title") or ""),
            message=str(payload.get("message") or ""),
            choices=[str(choice) for choice in payload.get("choices") or []],
            recommended=payload.get("recommended"),
            default_decision=payload.get("default_decision"),
            risk_level=payload.get("risk_level"),
            metadata=dict(payload.get("metadata") or {}),
            allow_freeform=bool(payload.get("allow_freeform", False)),
        )


@dataclass(frozen=True)
class UserDecision:
    prompt_id: str
    decision: str
    reason: str = ""
    revised_goal: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)
    decided_by: str = "local-user"
    timestamp: datetime = field(default_factory=lambda: datetime.now(UTC))

    def to_dict(self) -> dict[str, Any]:
        return {
            "prompt_id": self.prompt_id,
            "decision": self.decision,
            "reason": self.reason,
            "revised_goal": self.revised_goal,
            "metadata": self.metadata,
            "decided_by": self.decided_by,
            "timestamp": self.timestamp.isoformat(),
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "UserDecision":
        return cls(
            prompt_id=str(payload["prompt_id"]),
            decision=str(payload["decision"]),
            reason=str(payload.get("reason") or ""),
            revised_goal=payload.get("revised_goal"),
            metadata=dict(payload.get("metadata") or {}),
            decided_by=str(payload.get("decided_by") or "local-user"),
            timestamp=_datetime(payload.get("timestamp") or datetime.now(UTC)),
        )


@dataclass(frozen=True)
class ClarificationRequest:
    question: str
    reason: str
    current_goal: str
    request_id: str = field(default_factory=lambda: f"clarification_{uuid4().hex[:12]}")
    required: bool = True
    options: list[str] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "request_id": self.request_id,
            "question": self.question,
            "reason": self.reason,
            "current_goal": self.current_goal,
            "required": self.required,
            "options": self.options,
            "metadata": self.metadata,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ClarificationRequest":
        return cls(
            request_id=str(payload.get("request_id") or f"clarification_{uuid4().hex[:12]}"),
            question=str(payload.get("question") or ""),
            reason=str(payload.get("reason") or ""),
            current_goal=str(payload.get("current_goal") or ""),
            required=bool(payload.get("required", True)),
            options=[str(option) for option in payload.get("options") or []],
            metadata=dict(payload.get("metadata") or {}),
        )


@dataclass(frozen=True)
class ClarificationAnswer:
    request_id: str
    answer: str
    revised_goal: str | None = None
    answered_by: str = "local-user"
    metadata: dict[str, Any] = field(default_factory=dict)
    timestamp: datetime = field(default_factory=lambda: datetime.now(UTC))

    def to_dict(self) -> dict[str, Any]:
        return {
            "request_id": self.request_id,
            "answer": self.answer,
            "revised_goal": self.revised_goal,
            "answered_by": self.answered_by,
            "metadata": self.metadata,
            "timestamp": self.timestamp.isoformat(),
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ClarificationAnswer":
        return cls(
            request_id=str(payload["request_id"]),
            answer=str(payload.get("answer") or ""),
            revised_goal=payload.get("revised_goal"),
            answered_by=str(payload.get("answered_by") or "local-user"),
            metadata=dict(payload.get("metadata") or {}),
            timestamp=_datetime(payload.get("timestamp") or datetime.now(UTC)),
        )


@dataclass(frozen=True)
class FinalReport:
    outcome: OutcomeStatus | str
    summary: str
    title: str = "Final Report"
    completed_items: list[str] = field(default_factory=list)
    partial_items: list[str] = field(default_factory=list)
    failed_items: list[str] = field(default_factory=list)
    blocked_reasons: list[str] = field(default_factory=list)
    cancelled_reason: str | None = None
    verification_status: str | None = None
    review_findings: list[dict[str, Any]] = field(default_factory=list)
    files_changed: list[str] = field(default_factory=list)
    risks: list[Any] = field(default_factory=list)
    next_steps: list[str] = field(default_factory=list)
    trace_summary: dict[str, Any] = field(default_factory=dict)
    technical_summary: dict[str, Any] = field(default_factory=dict)
    generated_at: datetime = field(default_factory=lambda: datetime.now(UTC))

    def __post_init__(self) -> None:
        object.__setattr__(self, "outcome", _enum(OutcomeStatus, self.outcome))
        object.__setattr__(self, "generated_at", _datetime(self.generated_at))

    def to_dict(self) -> dict[str, Any]:
        return {
            "title": self.title,
            "outcome": self.outcome.value,
            "summary": self.summary,
            "completed_items": self.completed_items,
            "partial_items": self.partial_items,
            "failed_items": self.failed_items,
            "blocked_reasons": self.blocked_reasons,
            "cancelled_reason": self.cancelled_reason,
            "verification_status": self.verification_status,
            "review_findings": self.review_findings,
            "files_changed": self.files_changed,
            "risks": self.risks,
            "next_steps": self.next_steps,
            "trace_summary": self.trace_summary,
            "technical_summary": self.technical_summary,
            "generated_at": self.generated_at.isoformat(),
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "FinalReport":
        return cls(
            title=str(payload.get("title") or "Final Report"),
            outcome=payload["outcome"],
            summary=str(payload.get("summary") or ""),
            completed_items=[str(item) for item in payload.get("completed_items") or []],
            partial_items=[str(item) for item in payload.get("partial_items") or []],
            failed_items=[str(item) for item in payload.get("failed_items") or []],
            blocked_reasons=[str(item) for item in payload.get("blocked_reasons") or []],
            cancelled_reason=payload.get("cancelled_reason"),
            verification_status=payload.get("verification_status"),
            review_findings=list(payload.get("review_findings") or []),
            files_changed=[str(item) for item in payload.get("files_changed") or []],
            risks=list(payload.get("risks") or []),
            next_steps=[str(item) for item in payload.get("next_steps") or []],
            trace_summary=dict(payload.get("trace_summary") or {}),
            technical_summary=dict(payload.get("technical_summary") or {}),
            generated_at=_datetime(payload.get("generated_at") or datetime.now(UTC)),
        )


def _enum(enum_type: type[Enum], value: Enum | str) -> Enum:
    if isinstance(value, enum_type):
        return value
    text = str(value)
    if text in enum_type.__members__:
        return enum_type[text]
    return enum_type(text)


def _datetime(value: datetime | str) -> datetime:
    if isinstance(value, datetime):
        return value if value.tzinfo is not None else value.replace(tzinfo=UTC)
    parsed = datetime.fromisoformat(str(value))
    return parsed if parsed.tzinfo is not None else parsed.replace(tzinfo=UTC)
