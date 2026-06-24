from __future__ import annotations

import hashlib
import json
from dataclasses import asdict, dataclass, field, is_dataclass
from datetime import UTC, datetime
from enum import Enum
from typing import Any
from uuid import uuid4


class ContextSource(str, Enum):
    MODEL = "model"
    TOOL = "tool"
    TOOL_PROTOCOL = "tool_protocol"
    PLANNER = "planner"
    EDIT = "edit"
    MUTATION = "mutation"
    COMMAND = "command"
    VERIFICATION = "verification"
    POLICY = "policy"
    WORKSPACE_STATE = "workspace_state"
    PROJECT_INDEX = "project_index"
    MEMORY = "memory"
    USER = "user"
    SYSTEM = "system"
    SUMMARY = "summary"


class ContextItemType(str, Enum):
    SYSTEM_INSTRUCTION = "system_instruction"
    USER_GOAL = "user_goal"
    USER_MESSAGE = "user_message"
    ASSISTANT_MESSAGE = "assistant_message"
    TOOL_OBSERVATION = "tool_observation"
    PLANNER_STATE = "planner_state"
    POLICY_OBSERVATION = "policy_observation"
    EDIT_EVIDENCE = "edit_evidence"
    MUTATION_EVIDENCE = "mutation_evidence"
    COMMAND_OBSERVATION = "command_observation"
    VERIFICATION_EVIDENCE = "verification_evidence"
    WORKSPACE_STATE = "workspace_state"
    PROJECT_INDEX = "project_index"
    MEMORY_CONTEXT = "memory_context"
    FAILURE = "failure"
    SUMMARY = "summary"
    REFERENCE = "reference"


class ContextLayer(str, Enum):
    SYSTEM = "system"
    USER_GOAL = "user_goal"
    TASK_STATE = "task_state"
    PLANNER_STATE = "planner_state"
    POLICY_STATE = "policy_state"
    WORKSPACE_STATE = "workspace_state"
    EVIDENCE = "evidence"
    TOOL_OBSERVATIONS = "tool_observations"
    VERIFICATION = "verification"
    RECENT_DIALOGUE = "recent_dialogue"
    COMPRESSED_HISTORY = "compressed_history"
    FAILURE_MEMORY = "failure_memory"
    REFERENCES = "references"
    SCRATCHPAD = "scratchpad"


class ContextAuthority(str, Enum):
    USER = "user"
    SYSTEM = "system"
    COMPONENT = "component"
    TOOL = "tool"
    MODEL = "model"
    SUMMARY = "summary"


class ContextFreshness(str, Enum):
    CURRENT = "current"
    STALE = "stale"
    OBSOLETE = "obsolete"


class ContextSensitivity(str, Enum):
    PUBLIC = "public"
    WORKSPACE = "workspace"
    SENSITIVE = "sensitive"
    SECRET = "secret"


class CacheAttributionSource(str, Enum):
    PROVIDER_NATIVE = "provider_native"
    COMPONENT_INFERRED = "component_inferred"
    UNKNOWN = "unknown"


@dataclass(frozen=True)
class PartialCompactionRange:
    start_turn: int | None = None
    end_turn: int | None = None
    checkpoint_id: str | None = None

    def __post_init__(self) -> None:
        if self.start_turn is None and self.end_turn is None and not self.checkpoint_id:
            raise ValueError("PartialCompactionRange requires a turn range or checkpoint_id.")
        if (
            self.start_turn is not None
            and self.end_turn is not None
            and self.start_turn > self.end_turn
        ):
            raise ValueError("PartialCompactionRange start_turn cannot exceed end_turn.")

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)


@dataclass
class ContextReference:
    ref_id: str
    ref_type: str
    target: str | None = None
    path: str | None = None
    line_start: int | None = None
    line_end: int | None = None
    digest: str | None = None
    observed_at: str = field(default_factory=lambda: _now())
    freshness: ContextFreshness = ContextFreshness.CURRENT
    source_item_id: str = ""
    metadata: dict[str, Any] = field(default_factory=dict)
    observation_id: str = ""

    def __post_init__(self) -> None:
        self.freshness = _enum(ContextFreshness, self.freshness)
        if not self.target:
            self.target = self.path or self.observation_id or self.source_item_id or self.ref_id
        if not self.observation_id:
            self.observation_id = self.source_item_id

    @property
    def id(self) -> str:
        return self.ref_id

    @property
    def type(self) -> str:
        return self.ref_type

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ContextReference":
        return cls(
            ref_id=str(payload.get("ref_id") or payload.get("id") or ""),
            ref_type=str(payload.get("ref_type") or payload.get("type") or ""),
            target=payload.get("target"),
            path=payload.get("path"),
            line_start=payload.get("line_start"),
            line_end=payload.get("line_end"),
            digest=payload.get("digest"),
            observed_at=str(payload.get("observed_at") or _now()),
            freshness=payload.get("freshness") or ContextFreshness.CURRENT,
            source_item_id=str(payload.get("source_item_id") or ""),
            metadata=dict(payload.get("metadata") or {}),
            observation_id=str(payload.get("observation_id") or ""),
        )


@dataclass
class ContextItem:
    item_id: str
    run_id: str
    session_id: str
    task_id: str
    phase_id: str
    layer: ContextLayer
    source_component: ContextSource
    item_type: ContextItemType
    content: Any
    content_digest: str = ""
    created_at: str = field(default_factory=lambda: _now())
    updated_at: str = field(default_factory=lambda: _now())
    importance: float = 0.5
    relevance_score: float | None = None
    authority: ContextAuthority = ContextAuthority.COMPONENT
    freshness: ContextFreshness = ContextFreshness.CURRENT
    sensitivity: ContextSensitivity = ContextSensitivity.WORKSPACE
    token_count: int = 0
    references: list[ContextReference] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)
    pinned: bool = False
    expires_at: str | None = None

    def __post_init__(self) -> None:
        self.layer = _enum(ContextLayer, self.layer)
        self.source_component = _enum(ContextSource, self.source_component)
        self.item_type = _enum(ContextItemType, self.item_type)
        self.authority = _enum(ContextAuthority, self.authority)
        self.freshness = _enum(ContextFreshness, self.freshness)
        self.sensitivity = _enum(ContextSensitivity, self.sensitivity)
        self.references = [
            ref if isinstance(ref, ContextReference) else ContextReference.from_dict(ref)
            for ref in self.references
        ]
        if not self.content_digest:
            self.content_digest = digest_value(self.content)

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ContextItem":
        return cls(
            item_id=str(payload["item_id"]),
            run_id=str(payload["run_id"]),
            session_id=str(payload.get("session_id") or payload["run_id"]),
            task_id=str(payload.get("task_id") or payload["run_id"]),
            phase_id=str(payload.get("phase_id") or "context"),
            layer=payload.get("layer") or ContextLayer.SCRATCHPAD,
            source_component=payload.get("source_component") or ContextSource.SYSTEM,
            item_type=payload.get("item_type") or ContextItemType.SUMMARY,
            content=payload.get("content"),
            content_digest=str(payload.get("content_digest") or ""),
            created_at=str(payload.get("created_at") or _now()),
            updated_at=str(payload.get("updated_at") or _now()),
            importance=float(payload.get("importance") or 0.5),
            relevance_score=payload.get("relevance_score"),
            authority=payload.get("authority") or ContextAuthority.COMPONENT,
            freshness=payload.get("freshness") or ContextFreshness.CURRENT,
            sensitivity=payload.get("sensitivity") or ContextSensitivity.WORKSPACE,
            token_count=int(payload.get("token_count") or 0),
            references=[
                ContextReference.from_dict(ref)
                for ref in (payload.get("references") or [])
            ],
            metadata=dict(payload.get("metadata") or {}),
            pinned=bool(payload.get("pinned")),
            expires_at=payload.get("expires_at"),
        )


@dataclass
class ContextBudgetPlan:
    model_context_window: int
    output_token_reserve: int
    reasoning_token_reserve: int = 0
    tool_schema_tokens: int = 0
    system_tokens: int = 0
    pinned_tokens: int = 0
    evidence_tokens: int = 0
    recent_dialogue_tokens: int = 0
    summary_tokens: int = 0
    available_tokens: int = 0
    used_tokens: int = 0
    overflow_tokens: int = 0
    soft_limit: int = 0
    hard_limit: int = 0
    message_tokens: int = 0

    def __post_init__(self) -> None:
        if not self.hard_limit:
            self.hard_limit = self.model_context_window
        if not self.soft_limit:
            self.soft_limit = max(0, int(self.model_context_window * 0.9))
        if not self.used_tokens:
            self.used_tokens = (
                self.message_tokens
                + self.tool_schema_tokens
                + self.output_token_reserve
                + self.reasoning_token_reserve
            )
        if not self.available_tokens:
            self.available_tokens = max(
                0,
                self.model_context_window
                - self.output_token_reserve
                - self.reasoning_token_reserve
                - self.tool_schema_tokens,
            )
        self.overflow_tokens = max(0, self.used_tokens - self.model_context_window)

    @property
    def tool_tokens(self) -> int:
        return self.tool_schema_tokens

    @property
    def total_tokens(self) -> int:
        return self.used_tokens

    @property
    def remaining_tokens(self) -> int:
        return self.model_context_window - self.used_tokens


ContextBudget = ContextBudgetPlan


@dataclass
class ContextRenderPolicy:
    include_raw_tool_outputs: bool = False
    include_policy_details: bool = True
    include_secret_content: bool = False
    include_full_diff: bool = False
    include_failed_attempts: bool = True
    max_tool_preview_tokens: int = 1000
    max_evidence_items: int = 12
    max_recent_turns: int = 8
    require_references_for_claims: bool = True
    redact_sensitive: bool = True
    phase_aware: bool = True


@dataclass
class ContextBundle:
    bundle_id: str
    run_id: str
    task_id: str
    phase_id: str
    model: str
    provider: str
    messages: list[dict[str, Any]]
    included_item_ids: list[str]
    excluded_item_ids: list[str]
    budget: ContextBudgetPlan
    compression_snapshot_id: str | None = None
    retrieval_query: str | None = None
    render_policy: ContextRenderPolicy = field(default_factory=ContextRenderPolicy)
    created_at: str = field(default_factory=lambda: _now())
    bundle_digest: str = ""
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if not self.bundle_digest:
            self.bundle_digest = digest_value(
                {
                    "messages": self.messages,
                    "included_item_ids": self.included_item_ids,
                    "excluded_item_ids": self.excluded_item_ids,
                    "budget": self.budget,
                }
            )

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ContextBundle":
        return cls(
            bundle_id=str(payload["bundle_id"]),
            run_id=str(payload["run_id"]),
            task_id=str(payload.get("task_id") or payload["run_id"]),
            phase_id=str(payload.get("phase_id") or "context"),
            model=str(payload.get("model") or ""),
            provider=str(payload.get("provider") or ""),
            messages=list(payload.get("messages") or []),
            included_item_ids=list(payload.get("included_item_ids") or []),
            excluded_item_ids=list(payload.get("excluded_item_ids") or []),
            budget=ContextBudgetPlan(**dict(payload.get("budget") or {})),
            compression_snapshot_id=payload.get("compression_snapshot_id"),
            retrieval_query=payload.get("retrieval_query"),
            render_policy=ContextRenderPolicy(**dict(payload.get("render_policy") or {})),
            created_at=str(payload.get("created_at") or _now()),
            bundle_digest=str(payload.get("bundle_digest") or ""),
            metadata=dict(payload.get("metadata") or {}),
        )


@dataclass
class ContextUsageReport:
    layer_token_usage: dict[str, int] = field(default_factory=dict)
    included_item_ids: list[str] = field(default_factory=list)
    excluded_item_ids: list[str] = field(default_factory=list)
    stale_item_ids: list[str] = field(default_factory=list)
    summary_item_ids: list[str] = field(default_factory=list)
    recent_tail_item_ids: list[str] = field(default_factory=list)
    input_tokens: int = 0
    cached_input_tokens: int = 0
    cache_hit_ratio: float = 0.0
    cache_miss_reasons: list[str] = field(default_factory=list)
    cache_attribution: "CacheAttribution" = field(default_factory=lambda: CacheAttribution())
    recommendations: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)


@dataclass
class ContextSnapshot:
    snapshot_id: str
    run_id: str
    session_id: str = ""
    task_id: str = ""
    goal: str = ""
    summary: str = ""
    retained_item_ids: list[str] = field(default_factory=list)
    known_observation_ids: list[str] = field(default_factory=list)
    version: int = 0
    created_at: str = field(default_factory=lambda: _now())
    retained_messages: list[dict[str, Any]] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)

    @property
    def id(self) -> str:
        return self.snapshot_id

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ContextSnapshot":
        return cls(
            snapshot_id=str(payload.get("snapshot_id") or payload.get("id") or ""),
            run_id=str(payload.get("run_id") or ""),
            session_id=str(payload.get("session_id") or payload.get("run_id") or ""),
            task_id=str(payload.get("task_id") or payload.get("run_id") or ""),
            goal=str(payload.get("goal") or ""),
            summary=str(payload.get("summary") or ""),
            retained_item_ids=list(payload.get("retained_item_ids") or []),
            known_observation_ids=list(payload.get("known_observation_ids") or []),
            version=int(payload.get("version") or 0),
            created_at=str(payload.get("created_at") or _now()),
            retained_messages=list(payload.get("retained_messages") or []),
            metadata=dict(payload.get("metadata") or {}),
        )


@dataclass
class ToolObservation:
    id: str
    tool_name: str
    tool_call_id: str | None
    ok: bool
    raw_result: dict[str, Any]
    preview: str
    truncated: bool
    metadata: dict[str, Any] = field(default_factory=dict)
    run_id: str = ""
    turn: int = 0
    created_at: str = ""
    input_tokens: int = 0
    preview_tokens: int = 0
    raw_digest: str = ""
    source_refs: list[ContextReference] = field(default_factory=list)
    cache_hit: bool = False
    duration_seconds: float | None = None
    error_code: str | None = None
    tool_version: str | None = None
    truncation_reason: str | None = None
    sensitivity: ContextSensitivity = ContextSensitivity.WORKSPACE

    def __post_init__(self) -> None:
        self.sensitivity = _enum(ContextSensitivity, self.sensitivity)
        self.source_refs = [
            ref if isinstance(ref, ContextReference) else ContextReference.from_dict(ref)
            for ref in self.source_refs
        ]


@dataclass
class PlannerState:
    task_id: str
    current_phase: str
    status: str
    current_plan: list[Any]
    completion_criteria: dict[str, Any]
    open_actions: list[Any]
    blocked_actions: list[Any]
    risk_escalations: list[Any]
    evidence_refs: list[str]


@dataclass
class PolicyObservation:
    decision_id: str
    request_id: str
    outcome: str
    risk_level: str
    reason: str
    constraints_summary: list[str]
    user_decision: str | None
    approval_grant_id: str | None
    component: str
    operation: str
    resource: str
    reference: str | None = None


@dataclass
class VerificationEvidence:
    check_id: str
    command: str
    status: str
    failure_summary: str | None
    parsed_failures: list[Any]
    repair_hints: list[Any]
    logs_ref: str | None
    confidence: float


@dataclass
class MutationEvidence:
    transaction_id: str
    files_changed: list[str]
    diff_summary: str
    rollback_ref: str | None
    status: str


@dataclass
class CommandObservation:
    command_id: str
    command_preview: str
    exit_code: int | None
    status: str
    stdout_preview: str
    stderr_preview: str
    output_ref: str | None
    resource_limits: dict[str, Any]
    policy_decision_id: str | None


@dataclass
class ContextSummaryPayload:
    goal: str
    current_state: str
    completed_actions: list[Any]
    pending_actions: list[Any]
    verified_facts: list[Any]
    failed_attempts: list[Any]
    policy_constraints: list[str]
    workspace_changes: list[Any]
    verification_status: str
    open_questions: list[Any]
    reference_ids: list[str]
    omitted_item_ids: list[str]
    confidence: float

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ContextSummaryPayload":
        return cls(
            goal=str(payload.get("goal") or ""),
            current_state=str(payload.get("current_state") or ""),
            completed_actions=list(payload.get("completed_actions") or []),
            pending_actions=list(payload.get("pending_actions") or []),
            verified_facts=list(payload.get("verified_facts") or []),
            failed_attempts=list(payload.get("failed_attempts") or []),
            policy_constraints=[str(item) for item in payload.get("policy_constraints") or []],
            workspace_changes=list(payload.get("workspace_changes") or []),
            verification_status=str(payload.get("verification_status") or "unknown"),
            open_questions=list(payload.get("open_questions") or []),
            reference_ids=[str(item) for item in payload.get("reference_ids") or []],
            omitted_item_ids=[str(item) for item in payload.get("omitted_item_ids") or []],
            confidence=float(payload.get("confidence") or 0.5),
        )


@dataclass
class CacheAttribution:
    source: CacheAttributionSource = CacheAttributionSource.UNKNOWN
    confidence: float = 0.0
    reasons: list[str] = field(default_factory=list)
    evidence: list[str] = field(default_factory=list)
    provider_name: str | None = None
    model_name: str | None = None

    def __post_init__(self) -> None:
        self.source = _enum(CacheAttributionSource, self.source)

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)

    @classmethod
    def from_dict(cls, payload: dict[str, Any] | None) -> "CacheAttribution":
        payload = payload or {}
        return cls(
            source=payload.get("source") or CacheAttributionSource.UNKNOWN,
            confidence=float(payload.get("confidence") or 0.0),
            reasons=[str(item) for item in payload.get("reasons") or []],
            evidence=[str(item) for item in payload.get("evidence") or []],
            provider_name=payload.get("provider_name"),
            model_name=payload.get("model_name"),
        )


@dataclass
class ContextSummaryEnvelope:
    version: int = 1
    summary_id: str = ""
    summary_payload: ContextSummaryPayload | None = None
    source_item_ids: list[str] = field(default_factory=list)
    cache_attribution: CacheAttribution = field(default_factory=CacheAttribution)
    previous_summary_digest: str | None = None
    summary_digest: str = ""
    rendered_summary: str = ""
    created_at: str = field(default_factory=lambda: _now())
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        self.version = int(self.version or 1)
        self.cache_attribution = (
            self.cache_attribution
            if isinstance(self.cache_attribution, CacheAttribution)
            else CacheAttribution.from_dict(self.cache_attribution)
        )
        if self.summary_payload is not None and not isinstance(self.summary_payload, ContextSummaryPayload):
            self.summary_payload = ContextSummaryPayload.from_dict(self.summary_payload)
        if not self.summary_digest and self.summary_payload is not None:
            self.summary_digest = digest_value(self.summary_payload)

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "ContextSummaryEnvelope":
        raw_payload = payload.get("summary_payload")
        if raw_payload is None and isinstance(payload.get("payload"), dict):
            raw_payload = payload.get("payload")
        if raw_payload is None:
            raw_payload = payload
        summary_payload = (
            ContextSummaryPayload.from_dict(raw_payload)
            if isinstance(raw_payload, dict)
            else None
        )
        return cls(
            version=int(payload.get("version") or 1),
            summary_id=str(payload.get("summary_id") or payload.get("id") or ""),
            summary_payload=summary_payload,
            source_item_ids=[str(item) for item in payload.get("source_item_ids") or []],
            cache_attribution=CacheAttribution.from_dict(
                payload.get("cache_attribution")
                or payload.get("cache")
                or {},
            ),
            previous_summary_digest=payload.get("previous_summary_digest"),
            summary_digest=str(payload.get("summary_digest") or ""),
            rendered_summary=str(payload.get("rendered_summary") or payload.get("summary_text") or ""),
            created_at=str(payload.get("created_at") or _now()),
            metadata=dict(payload.get("metadata") or {}),
        )


@dataclass
class RecoveredContext:
    run_id: str
    messages: list[dict[str, Any]]
    context_items: list[ContextItem] = field(default_factory=list)
    last_bundle: ContextBundle | None = None
    planner_state: dict[str, Any] | None = None
    pending_tool_calls: list[dict[str, Any]] = field(default_factory=list)
    completed_tool_call_ids: set[str] = field(default_factory=set)
    pending_policy_approval: dict[str, Any] | None = None
    active_process_sessions: list[str] = field(default_factory=list)
    open_mutation_transactions: list[str] = field(default_factory=list)
    last_verification_status: str | None = None
    last_safe_checkpoint: dict[str, Any] | None = None
    recommended_next_action: str = "request_model"
    recovery_warnings: list[str] = field(default_factory=list)
    trace_last_event: str | None = None

    @property
    def last_completed_tool_call_ids(self) -> set[str]:
        return self.completed_tool_call_ids

    @property
    def next_action(self) -> str:
        return self.recommended_next_action


def new_item_id(prefix: str) -> str:
    return f"{prefix}_{uuid4().hex[:12]}"


def digest_value(value: Any) -> str:
    payload = json.dumps(_to_plain(value), ensure_ascii=False, sort_keys=True, default=str)
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()


def _to_plain(value: Any) -> Any:
    if isinstance(value, Enum):
        return value.value
    if is_dataclass(value) and not isinstance(value, type):
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
    if isinstance(value, enum_cls):
        return value
    return enum_cls(str(value))


def _now() -> str:
    return datetime.now(UTC).isoformat()

