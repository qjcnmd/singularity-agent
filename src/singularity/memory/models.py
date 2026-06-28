from __future__ import annotations

import hashlib
import json
from dataclasses import asdict, dataclass, field, is_dataclass
from datetime import UTC, datetime
from enum import Enum, StrEnum
from typing import Any
from uuid import uuid4

SCHEMA_VERSION = 1


class MemoryScope(StrEnum):
    SESSION = "session"
    WORKSPACE = "workspace"
    PROJECT = "project"
    USER_PREFERENCE = "user_preference"
    TOOL_EXECUTOR = "tool_executor"


class MemoryType(StrEnum):
    PROJECT_CONVENTION = "project_convention"
    BUILD_COMMAND = "build_command"
    TEST_COMMAND = "test_command"
    MODULE_BOUNDARY = "module_boundary"
    USER_PREFERENCE = "user_preference"
    TOOL_EXECUTOR = "tool_executor"
    LESSON = "lesson"
    CAUTION = "caution"
    FAILURE_LESSON = "failure_lesson"
    VERIFICATION_FACT = "verification_fact"


class MemorySource(StrEnum):
    TRACE = "trace"
    FINAL_REPORT = "final_report"
    REVIEW = "review"
    VERIFICATION = "verification"
    ROLLBACK = "rollback"
    USER = "user"
    MANUAL = "manual"
    HUMAN_FILE = "human_file"
    MODEL = "model"
    UNKNOWN = "unknown"


class Confidence(StrEnum):
    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"
    VERIFIED = "verified"

    @property
    def score(self) -> float:
        return {
            Confidence.LOW: 0.35,
            Confidence.MEDIUM: 0.6,
            Confidence.HIGH: 0.82,
            Confidence.VERIFIED: 0.95,
        }[self]


class ConflictStatus(StrEnum):
    NONE = "none"
    CONFLICTED = "conflicted"
    MANUAL_REVIEW_REQUIRED = "manual_review_required"
    SUPERSEDED = "superseded"


class MemoryStatus(StrEnum):
    CANDIDATE = "candidate"
    ACTIVE = "active"
    QUARANTINED = "quarantined"
    REJECTED = "rejected"
    TOMBSTONED = "tombstoned"
    EXPIRED = "expired"


class MemoryAuthorType(StrEnum):
    HUMAN = "human"
    AGENT = "agent"


@dataclass(frozen=True)
class MemoryEvidenceRef:
    source: MemorySource | str
    ref_id: str
    summary: str
    event_id: str | None = None
    artifact_ref: str | None = None
    path: str | None = None
    captured_at: str = field(default_factory=lambda: _now())
    trust_level: str = "component_evidence"
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        object.__setattr__(self, "source", _enum(MemorySource, self.source))

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> MemoryEvidenceRef:
        return cls(
            source=payload.get("source") or MemorySource.UNKNOWN,
            ref_id=str(payload.get("ref_id") or payload.get("id") or ""),
            summary=str(payload.get("summary") or ""),
            event_id=payload.get("event_id"),
            artifact_ref=payload.get("artifact_ref"),
            path=payload.get("path"),
            captured_at=str(payload.get("captured_at") or _now()),
            trust_level=str(payload.get("trust_level") or "component_evidence"),
            metadata=dict(payload.get("metadata") or {}),
        )


@dataclass(frozen=True)
class Provenance:
    evidence: list[MemoryEvidenceRef] = field(default_factory=list)
    created_by: str = "memory_pipeline"
    source_run_id: str | None = None
    source_session_id: str | None = None
    source_task_id: str | None = None
    extracted_at: str = field(default_factory=lambda: _now())
    notes: list[str] = field(default_factory=list)

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "evidence",
            [
                item if isinstance(item, MemoryEvidenceRef) else MemoryEvidenceRef.from_dict(item)
                for item in self.evidence
            ],
        )

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)

    @classmethod
    def from_dict(cls, payload: dict[str, Any] | None) -> Provenance:
        payload = payload or {}
        return cls(
            evidence=[
                MemoryEvidenceRef.from_dict(item)
                for item in list(payload.get("evidence") or [])
            ],
            created_by=str(payload.get("created_by") or "memory_pipeline"),
            source_run_id=payload.get("source_run_id"),
            source_session_id=payload.get("source_session_id"),
            source_task_id=payload.get("source_task_id"),
            extracted_at=str(payload.get("extracted_at") or _now()),
            notes=[str(item) for item in list(payload.get("notes") or [])],
        )


@dataclass(frozen=True)
class TTL:
    expires_at: str | None = None
    stale_after: str | None = None
    reason: str | None = None

    def expired(self, *, now: datetime | None = None) -> bool:
        if not self.expires_at:
            return False
        return _parse_dt(self.expires_at) <= (now or datetime.now(UTC))

    def stale(self, *, now: datetime | None = None) -> bool:
        if not self.stale_after:
            return False
        return _parse_dt(self.stale_after) <= (now or datetime.now(UTC))

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)

    @classmethod
    def from_dict(cls, payload: dict[str, Any] | None) -> TTL:
        payload = payload or {}
        return cls(
            expires_at=payload.get("expires_at"),
            stale_after=payload.get("stale_after"),
            reason=payload.get("reason"),
        )


@dataclass
class MemoryEntry:
    id: str
    scope: MemoryScope | str
    type: MemoryType | str
    source: MemorySource | str
    title: str
    body: str
    confidence: Confidence | str = Confidence.MEDIUM
    provenance: Provenance = field(default_factory=Provenance)
    ttl: TTL = field(default_factory=TTL)
    conflict_status: ConflictStatus | str = ConflictStatus.NONE
    status: MemoryStatus | str = MemoryStatus.ACTIVE
    author_type: MemoryAuthorType | str = MemoryAuthorType.AGENT
    created_at: str = field(default_factory=lambda: _now())
    updated_at: str = field(default_factory=lambda: _now())
    last_verified_at: str | None = None
    tags: list[str] = field(default_factory=list)
    paths: list[str] = field(default_factory=list)
    tools: list[str] = field(default_factory=list)
    error_types: list[str] = field(default_factory=list)
    modules: list[str] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)
    tombstone_reason: str | None = None
    rejection_reason: str | None = None
    schema_version: int = SCHEMA_VERSION

    def __post_init__(self) -> None:
        self.scope = _enum(MemoryScope, self.scope)
        self.type = _enum(MemoryType, self.type)
        self.source = _enum(MemorySource, self.source)
        self.confidence = _enum(Confidence, self.confidence)
        self.conflict_status = _enum(ConflictStatus, self.conflict_status)
        self.status = _enum(MemoryStatus, self.status)
        self.author_type = _enum(MemoryAuthorType, self.author_type)
        if not isinstance(self.provenance, Provenance):
            self.provenance = Provenance.from_dict(self.provenance)
        if not isinstance(self.ttl, TTL):
            self.ttl = TTL.from_dict(self.ttl)
        self.tags = [str(item) for item in self.tags]
        self.paths = [str(item) for item in self.paths]
        self.tools = [str(item) for item in self.tools]
        self.error_types = [str(item) for item in self.error_types]
        self.modules = [str(item) for item in self.modules]

    @property
    def content_hash(self) -> str:
        return digest_value(
            {
                "scope": self.scope.value,
                "type": self.type.value,
                "title": self.title,
                "body": self.body,
                "paths": self.paths,
                "tools": self.tools,
                "error_types": self.error_types,
                "modules": self.modules,
            }
        )

    def is_expired(self, *, now: datetime | None = None) -> bool:
        return self.ttl.expired(now=now)

    def is_active_for_retrieval(self, *, now: datetime | None = None) -> bool:
        return (
            self.status == MemoryStatus.ACTIVE
            and self.conflict_status == ConflictStatus.NONE
            and not self.is_expired(now=now)
        )

    def to_dict(self) -> dict[str, Any]:
        payload = _to_plain(self)
        payload["schema_version"] = self.schema_version
        return payload

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> MemoryEntry:
        if int(payload.get("schema_version") or 1) != SCHEMA_VERSION:
            raise ValueError(f"Unsupported memory entry schema: {payload.get('schema_version')}")
        return cls(
            id=str(payload["id"]),
            scope=payload.get("scope") or MemoryScope.PROJECT,
            type=payload.get("type") or MemoryType.LESSON,
            source=payload.get("source") or MemorySource.UNKNOWN,
            title=str(payload.get("title") or ""),
            body=str(payload.get("body") or ""),
            confidence=payload.get("confidence") or Confidence.MEDIUM,
            provenance=Provenance.from_dict(payload.get("provenance")),
            ttl=TTL.from_dict(payload.get("ttl")),
            conflict_status=payload.get("conflict_status") or ConflictStatus.NONE,
            status=payload.get("status") or MemoryStatus.ACTIVE,
            author_type=payload.get("author_type") or MemoryAuthorType.AGENT,
            created_at=str(payload.get("created_at") or _now()),
            updated_at=str(payload.get("updated_at") or _now()),
            last_verified_at=payload.get("last_verified_at"),
            tags=list(payload.get("tags") or []),
            paths=list(payload.get("paths") or []),
            tools=list(payload.get("tools") or []),
            error_types=list(payload.get("error_types") or []),
            modules=list(payload.get("modules") or []),
            metadata=dict(payload.get("metadata") or {}),
            tombstone_reason=payload.get("tombstone_reason"),
            rejection_reason=payload.get("rejection_reason"),
            schema_version=int(payload.get("schema_version") or SCHEMA_VERSION),
        )


@dataclass
class MemoryCandidate:
    id: str
    scope: MemoryScope | str
    type: MemoryType | str
    source: MemorySource | str
    title: str
    body: str
    confidence: Confidence | str = Confidence.MEDIUM
    provenance: Provenance = field(default_factory=Provenance)
    ttl: TTL = field(default_factory=TTL)
    status: MemoryStatus | str = MemoryStatus.CANDIDATE
    author_type: MemoryAuthorType | str = MemoryAuthorType.AGENT
    created_at: str = field(default_factory=lambda: _now())
    updated_at: str = field(default_factory=lambda: _now())
    last_verified_at: str | None = None
    tags: list[str] = field(default_factory=list)
    paths: list[str] = field(default_factory=list)
    tools: list[str] = field(default_factory=list)
    error_types: list[str] = field(default_factory=list)
    modules: list[str] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)
    decision_reason: str | None = None
    schema_version: int = SCHEMA_VERSION

    def __post_init__(self) -> None:
        self.scope = _enum(MemoryScope, self.scope)
        self.type = _enum(MemoryType, self.type)
        self.source = _enum(MemorySource, self.source)
        self.confidence = _enum(Confidence, self.confidence)
        self.status = _enum(MemoryStatus, self.status)
        self.author_type = _enum(MemoryAuthorType, self.author_type)
        if not isinstance(self.provenance, Provenance):
            self.provenance = Provenance.from_dict(self.provenance)
        if not isinstance(self.ttl, TTL):
            self.ttl = TTL.from_dict(self.ttl)

    @classmethod
    def from_entry(cls, entry: MemoryEntry) -> MemoryCandidate:
        return cls(
            id=f"cand_{entry.id}",
            scope=entry.scope,
            type=entry.type,
            source=entry.source,
            title=entry.title,
            body=entry.body,
            confidence=entry.confidence,
            provenance=entry.provenance,
            ttl=entry.ttl,
            last_verified_at=entry.last_verified_at,
            author_type=entry.author_type,
            tags=list(entry.tags),
            paths=list(entry.paths),
            tools=list(entry.tools),
            error_types=list(entry.error_types),
            modules=list(entry.modules),
            metadata=dict(entry.metadata),
        )

    def with_status(self, status: MemoryStatus, *, reason: str | None = None) -> MemoryCandidate:
        payload = self.to_dict()
        payload["status"] = status.value
        payload["decision_reason"] = reason
        payload["updated_at"] = _now()
        return MemoryCandidate.from_dict(payload)

    def to_entry(self, *, entry_id: str | None = None) -> MemoryEntry:
        resolved_id = entry_id or (self.id if self.id.startswith("mem_") else f"mem_{self.id}")
        return MemoryEntry(
            id=resolved_id,
            scope=self.scope,
            type=self.type,
            source=self.source,
            title=self.title,
            body=self.body,
            confidence=self.confidence,
            provenance=self.provenance,
            ttl=self.ttl,
            status=MemoryStatus.ACTIVE,
            author_type=self.author_type,
            created_at=self.created_at,
            updated_at=_now(),
            last_verified_at=self.last_verified_at,
            tags=list(self.tags),
            paths=list(self.paths),
            tools=list(self.tools),
            error_types=list(self.error_types),
            modules=list(self.modules),
            metadata=dict(self.metadata),
        )

    def to_dict(self) -> dict[str, Any]:
        payload = _to_plain(self)
        payload["schema_version"] = self.schema_version
        return payload

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> MemoryCandidate:
        if int(payload.get("schema_version") or 1) != SCHEMA_VERSION:
            raise ValueError(f"Unsupported memory candidate schema: {payload.get('schema_version')}")
        return cls(
            id=str(payload["id"]),
            scope=payload.get("scope") or MemoryScope.PROJECT,
            type=payload.get("type") or MemoryType.LESSON,
            source=payload.get("source") or MemorySource.UNKNOWN,
            title=str(payload.get("title") or ""),
            body=str(payload.get("body") or ""),
            confidence=payload.get("confidence") or Confidence.MEDIUM,
            provenance=Provenance.from_dict(payload.get("provenance")),
            ttl=TTL.from_dict(payload.get("ttl")),
            status=payload.get("status") or MemoryStatus.CANDIDATE,
            author_type=payload.get("author_type") or MemoryAuthorType.AGENT,
            created_at=str(payload.get("created_at") or _now()),
            updated_at=str(payload.get("updated_at") or _now()),
            last_verified_at=payload.get("last_verified_at"),
            tags=list(payload.get("tags") or []),
            paths=list(payload.get("paths") or []),
            tools=list(payload.get("tools") or []),
            error_types=list(payload.get("error_types") or []),
            modules=list(payload.get("modules") or []),
            metadata=dict(payload.get("metadata") or {}),
            decision_reason=payload.get("decision_reason"),
            schema_version=int(payload.get("schema_version") or SCHEMA_VERSION),
        )


@dataclass(frozen=True)
class MemoryQuery:
    goal: str = ""
    paths: list[str] = field(default_factory=list)
    tools: list[str] = field(default_factory=list)
    error_types: list[str] = field(default_factory=list)
    modules: list[str] = field(default_factory=list)
    limit: int = 8
    min_confidence: Confidence | str | None = None

    def __post_init__(self) -> None:
        if self.min_confidence is not None:
            object.__setattr__(self, "min_confidence", _enum(Confidence, self.min_confidence))


@dataclass(frozen=True)
class MemorySearchResult:
    entry: MemoryEntry
    score: float
    matched_fields: list[str]

    @property
    def confidence(self) -> Confidence:
        return self.entry.confidence

    @property
    def source(self) -> MemorySource:
        return self.entry.source

    @property
    def provenance(self) -> list[MemoryEvidenceRef]:
        return list(self.entry.provenance.evidence)

    @property
    def last_verified_at(self) -> str | None:
        return self.entry.last_verified_at

    def to_dict(self) -> dict[str, Any]:
        return {
            "entry": self.entry.to_dict(),
            "score": self.score,
            "matched_fields": self.matched_fields,
            "confidence": self.confidence.value,
            "source": self.source.value,
            "last_verified_at": self.last_verified_at,
            "provenance": [item.to_dict() for item in self.provenance],
        }


@dataclass(frozen=True)
class MemoryContextBlock:
    items: list[dict[str, Any]]
    token_count: int
    budget: int
    component: str = "memory"
    priority: float = 0.65
    pollution_risk: str = "bounded"
    generated_at: str = field(default_factory=lambda: _now())

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)


def new_memory_id(prefix: str = "mem") -> str:
    return f"{prefix}_{uuid4().hex[:12]}"


def digest_value(value: Any) -> str:
    return hashlib.sha256(
        json.dumps(_to_plain(value), ensure_ascii=False, sort_keys=True, default=str).encode("utf-8")
    ).hexdigest()


def _to_plain(value: Any) -> Any:
    if isinstance(value, Enum):
        return value.value
    if is_dataclass(value):
        return {key: _to_plain(item) for key, item in asdict(value).items()}
    if isinstance(value, dict):
        return {str(key): _to_plain(item) for key, item in value.items()}
    if isinstance(value, list):
        return [_to_plain(item) for item in value]
    if isinstance(value, tuple):
        return [_to_plain(item) for item in value]
    if isinstance(value, set):
        return sorted(_to_plain(item) for item in value)
    return value


def _enum(enum_cls: type[Enum], value: Any) -> Any:
    if isinstance(value, enum_cls):
        return value
    return enum_cls(str(value))


def _parse_dt(value: str) -> datetime:
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        return parsed.replace(tzinfo=UTC)
    return parsed.astimezone(UTC)


def _now() -> str:
    return datetime.now(UTC).isoformat()
