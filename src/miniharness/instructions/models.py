from __future__ import annotations

import hashlib
import json
from dataclasses import asdict, dataclass, field, is_dataclass
from datetime import UTC, datetime
from enum import Enum
from typing import Any
from uuid import uuid4

from miniharness.model.models import ModelMessage


class SerializableDataclass:
    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> Any:
        return _from_payload(cls, payload)


class InstructionPriority(str, Enum):
    SYSTEM_INVARIANT = "system_invariant"
    HARNESS_DEVELOPER = "harness_developer"
    USER_SESSION = "user_session"
    USER_TASK = "user_task"
    PROJECT_INSTRUCTION = "project_instruction"
    RUNTIME_OBSERVATION = "runtime_observation"
    RETRIEVED_CONTENT = "retrieved_content"
    MODEL_GENERATED = "model_generated"

    def __lt__(self, other: object) -> bool:
        if not isinstance(other, InstructionPriority):
            return NotImplemented
        return _priority_rank(self) < _priority_rank(other)

    def __gt__(self, other: object) -> bool:
        if not isinstance(other, InstructionPriority):
            return NotImplemented
        return _priority_rank(self) > _priority_rank(other)

    def __le__(self, other: object) -> bool:
        if not isinstance(other, InstructionPriority):
            return NotImplemented
        return _priority_rank(self) <= _priority_rank(other)

    def __ge__(self, other: object) -> bool:
        if not isinstance(other, InstructionPriority):
            return NotImplemented
        return _priority_rank(self) >= _priority_rank(other)


class TrustLevel(str, Enum):
    TRUSTED_SYSTEM = "trusted_system"
    TRUSTED_HARNESS = "trusted_harness"
    TRUSTED_USER = "trusted_user"
    PROJECT_DECLARED = "project_declared"
    RUNTIME_OBSERVATION = "runtime_observation"
    UNTRUSTED_CONTENT = "untrusted_content"
    MODEL_GENERATED = "model_generated"


class InstructionSourceType(str, Enum):
    SYSTEM = "system"
    HARNESS = "harness"
    USER_MESSAGE = "user_message"
    USER_SESSION_CONFIG = "user_session_config"
    PROJECT_FILE = "project_file"
    PROJECT_INSTRUCTION_FILE = "project_instruction_file"
    README = "readme"
    TOOL_OUTPUT = "tool_output"
    COMMAND_OUTPUT = "command_output"
    VERIFICATION_EVIDENCE = "verification_evidence"
    POLICY_OBSERVATION = "policy_observation"
    SANDBOX_OBSERVATION = "sandbox_observation"
    TRACE_SUMMARY = "trace_summary"
    MODEL_OUTPUT = "model_output"
    CONTEXT_SUMMARY = "context_summary"


@dataclass
class InstructionScope(SerializableDataclass):
    applies_to_runtime: list[str] = field(default_factory=list)
    applies_to_purpose: list[str] = field(default_factory=list)
    applies_to_paths: list[str] = field(default_factory=list)
    applies_to_tools: list[str] = field(default_factory=list)
    session_only: bool = False
    task_only: bool = False

    def matches(self, *, purpose: str | None = None, runtime: str | None = None) -> bool:
        if purpose and self.applies_to_purpose and purpose not in self.applies_to_purpose:
            return False
        if runtime and self.applies_to_runtime and runtime not in self.applies_to_runtime:
            return False
        return True


@dataclass
class InstructionSource(SerializableDataclass):
    source_id: str
    source_type: InstructionSourceType
    origin: str
    priority: InstructionPriority
    trust_level: TrustLevel
    scope: InstructionScope
    content: str
    metadata: dict[str, Any] = field(default_factory=dict)
    created_at: str = field(default_factory=lambda: datetime.now(UTC).isoformat())
    redaction_applied: bool = False
    source_hash: str = ""

    def __post_init__(self) -> None:
        if not self.source_hash:
            self.source_hash = _hash_payload(
                {
                    "source_type": self.source_type.value,
                    "origin": self.origin,
                    "content": self.content,
                    "metadata": self.metadata,
                }
            )


@dataclass
class InstructionConflict(SerializableDataclass):
    conflict_id: str
    higher_source_id: str
    lower_source_id: str
    description: str
    resolution: str
    severity: str
    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass
class InjectionWarning(SerializableDataclass):
    warning_id: str
    source_id: str
    pattern: str
    message: str
    severity: str
    evidence_excerpt: str
    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass
class InstructionFrame(SerializableDataclass):
    frame_id: str
    source: InstructionSource
    normalized_content: str
    effective_priority: InstructionPriority
    effective_trust_level: TrustLevel
    injection_warnings: list[InjectionWarning] = field(default_factory=list)
    conflicts: list[InstructionConflict] = field(default_factory=list)
    active: bool = True
    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass
class PromptSection(SerializableDataclass):
    section_id: str
    title: str
    priority: InstructionPriority
    trust_level: TrustLevel
    source_refs: list[str]
    content: str
    fenced: bool = False
    redaction_applied: bool = False
    token_estimate: int = 0
    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass
class PromptManifest(SerializableDataclass):
    manifest_id: str
    bundle_id: str
    purpose: str
    source_count: int
    section_count: int
    trust_summary: dict[str, int]
    priority_summary: dict[str, int]
    conflict_count: int = 0
    injection_warning_count: int = 0
    redaction_applied: bool = True
    prompt_hash: str = ""
    token_estimate: int = 0
    folded_developer_into_system: bool = False
    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass
class PromptBundle(SerializableDataclass):
    bundle_id: str
    purpose: str
    messages: list[ModelMessage]
    sections: list[PromptSection]
    manifest: PromptManifest
    token_estimate: int
    prompt_hash: str
    created_at: str = field(default_factory=lambda: datetime.now(UTC).isoformat())
    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass
class ResolvedInstructions(SerializableDataclass):
    frames: list[InstructionFrame]
    conflicts: list[InstructionConflict] = field(default_factory=list)
    warnings: list[InjectionWarning] = field(default_factory=list)


@dataclass
class InstructionCompilerInput:
    purpose: str
    frames: list[InstructionFrame]
    conflicts: list[InstructionConflict] = field(default_factory=list)
    warnings: list[InjectionWarning] = field(default_factory=list)
    supports_developer_message: bool = True
    metadata: dict[str, Any] = field(default_factory=dict)


PRIORITY_ORDER = [
    InstructionPriority.MODEL_GENERATED,
    InstructionPriority.RETRIEVED_CONTENT,
    InstructionPriority.RUNTIME_OBSERVATION,
    InstructionPriority.PROJECT_INSTRUCTION,
    InstructionPriority.USER_TASK,
    InstructionPriority.USER_SESSION,
    InstructionPriority.HARNESS_DEVELOPER,
    InstructionPriority.SYSTEM_INVARIANT,
]


def _priority_rank(priority: InstructionPriority) -> int:
    return PRIORITY_ORDER.index(priority)


def _new_id(prefix: str) -> str:
    return f"{prefix}_{uuid4().hex[:12]}"


def _hash_payload(payload: Any) -> str:
    text = json.dumps(_to_plain(payload), ensure_ascii=False, sort_keys=True, default=str)
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _to_plain(value: Any) -> Any:
    if isinstance(value, Enum):
        return value.value
    if is_dataclass(value):
        return {key: _to_plain(item) for key, item in asdict(value).items()}
    if isinstance(value, list):
        return [_to_plain(item) for item in value]
    if isinstance(value, dict):
        return {str(key): _to_plain(item) for key, item in value.items()}
    return value


def _from_payload(cls: Any, payload: dict[str, Any]) -> Any:
    if cls is InstructionScope:
        return InstructionScope(
            applies_to_runtime=list(payload.get("applies_to_runtime") or []),
            applies_to_purpose=list(payload.get("applies_to_purpose") or []),
            applies_to_paths=list(payload.get("applies_to_paths") or []),
            applies_to_tools=list(payload.get("applies_to_tools") or []),
            session_only=bool(payload.get("session_only")),
            task_only=bool(payload.get("task_only")),
        )
    if cls is InstructionSource:
        return InstructionSource(
            source_id=str(payload["source_id"]),
            source_type=InstructionSourceType(payload["source_type"]),
            origin=str(payload.get("origin") or ""),
            priority=InstructionPriority(payload["priority"]),
            trust_level=TrustLevel(payload["trust_level"]),
            scope=InstructionScope.from_dict(payload.get("scope") or {}),
            content=str(payload.get("content") or ""),
            metadata=dict(payload.get("metadata") or {}),
            created_at=str(payload.get("created_at") or datetime.now(UTC).isoformat()),
            redaction_applied=bool(payload.get("redaction_applied")),
            source_hash=str(payload.get("source_hash") or ""),
        )
    if cls is InjectionWarning:
        return InjectionWarning(**payload)
    if cls is InstructionConflict:
        return InstructionConflict(**payload)
    if cls is InstructionFrame:
        return InstructionFrame(
            frame_id=str(payload["frame_id"]),
            source=InstructionSource.from_dict(payload["source"]),
            normalized_content=str(payload.get("normalized_content") or ""),
            effective_priority=InstructionPriority(payload["effective_priority"]),
            effective_trust_level=TrustLevel(payload["effective_trust_level"]),
            injection_warnings=[
                InjectionWarning.from_dict(item)
                for item in payload.get("injection_warnings") or []
            ],
            conflicts=[
                InstructionConflict.from_dict(item)
                for item in payload.get("conflicts") or []
            ],
            active=bool(payload.get("active", True)),
            metadata=dict(payload.get("metadata") or {}),
        )
    if cls is PromptSection:
        return PromptSection(
            section_id=str(payload["section_id"]),
            title=str(payload.get("title") or ""),
            priority=InstructionPriority(payload["priority"]),
            trust_level=TrustLevel(payload["trust_level"]),
            source_refs=list(payload.get("source_refs") or []),
            content=str(payload.get("content") or ""),
            fenced=bool(payload.get("fenced")),
            redaction_applied=bool(payload.get("redaction_applied")),
            token_estimate=int(payload.get("token_estimate") or 0),
            metadata=dict(payload.get("metadata") or {}),
        )
    if cls is PromptManifest:
        return PromptManifest(
            manifest_id=str(payload["manifest_id"]),
            bundle_id=str(payload["bundle_id"]),
            purpose=str(payload["purpose"]),
            source_count=int(payload.get("source_count") or 0),
            section_count=int(payload.get("section_count") or 0),
            trust_summary=dict(payload.get("trust_summary") or {}),
            priority_summary=dict(payload.get("priority_summary") or {}),
            conflict_count=int(payload.get("conflict_count") or 0),
            injection_warning_count=int(payload.get("injection_warning_count") or 0),
            redaction_applied=bool(payload.get("redaction_applied", True)),
            prompt_hash=str(payload.get("prompt_hash") or ""),
            token_estimate=int(payload.get("token_estimate") or 0),
            folded_developer_into_system=bool(payload.get("folded_developer_into_system")),
            metadata=dict(payload.get("metadata") or {}),
        )
    if cls is PromptBundle:
        return PromptBundle(
            bundle_id=str(payload["bundle_id"]),
            purpose=str(payload["purpose"]),
            messages=[ModelMessage.from_dict(item) for item in payload.get("messages") or []],
            sections=[PromptSection.from_dict(item) for item in payload.get("sections") or []],
            manifest=PromptManifest.from_dict(payload["manifest"]),
            token_estimate=int(payload.get("token_estimate") or 0),
            prompt_hash=str(payload.get("prompt_hash") or ""),
            created_at=str(payload.get("created_at") or datetime.now(UTC).isoformat()),
            metadata=dict(payload.get("metadata") or {}),
        )
    if cls is ResolvedInstructions:
        return ResolvedInstructions(
            frames=[InstructionFrame.from_dict(item) for item in payload.get("frames") or []],
            conflicts=[InstructionConflict.from_dict(item) for item in payload.get("conflicts") or []],
            warnings=[InjectionWarning.from_dict(item) for item in payload.get("warnings") or []],
        )
    return cls(**payload)
