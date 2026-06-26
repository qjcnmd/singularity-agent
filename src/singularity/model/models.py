from __future__ import annotations

from dataclasses import asdict, dataclass, field, is_dataclass
from enum import Enum
from typing import Any
from uuid import uuid4


class SerializableDataclass:
    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> Any:
        return _from_payload(cls, payload)


class ModelPurpose(str, Enum):
    PLAN_NEXT_ACTION = "plan_next_action"
    FAILURE_ANALYSIS = "failure_analysis"
    REPAIR_PLANNING = "repair_planning"
    REPAIR_AFTER_FAILURE = "repair_after_failure"
    SUMMARIZE_CONTEXT = "summarize_context"
    FINAL_ANSWER = "final_answer"
    CLASSIFY_ERROR = "classify_error"
    VALIDATE_TOOL_CALL = "validate_tool_call"
    COMPACT_CONTEXT = "compact_context"
    TASK_CONTRACT_EXTRACTION = "task_contract_extraction"
    SEMANTIC_PLANNING = "semantic_planning"
    PLANNER_DECISION = "planner_decision"
    FINAL_REVIEW = "final_review"


class ModelRole(str, Enum):
    SYSTEM = "system"
    DEVELOPER = "developer"
    USER = "user"
    ASSISTANT = "assistant"
    TOOL = "tool"


class ContentBlockType(str, Enum):
    TEXT = "text"
    TOOL_RESULT = "tool_result"
    ERROR_SUMMARY = "error_summary"
    ARTIFACT_REF = "artifact_ref"


class ToolChoiceMode(str, Enum):
    NONE = "none"
    AUTO = "auto"
    REQUIRED = "required"
    SPECIFIC_TOOL = "specific_tool"
    ALLOWED_TOOLS = "allowed_tools"


class ModelToolParseStatus(str, Enum):
    VALID = "valid"
    INVALID_JSON = "invalid_json"
    SCHEMA_MISMATCH = "schema_mismatch"
    UNKNOWN_TOOL = "unknown_tool"


class ModelErrorKind(str, Enum):
    NETWORK_ERROR = "network_error"
    TIMEOUT = "timeout"
    RATE_LIMITED = "rate_limited"
    PROVIDER_OVERLOADED = "provider_overloaded"
    AUTH_ERROR = "auth_error"
    INVALID_REQUEST = "invalid_request"
    CONTEXT_LENGTH_EXCEEDED = "context_length_exceeded"
    BUDGET_EXCEEDED = "budget_exceeded"
    TOOL_CALL_PARSE_ERROR = "tool_call_parse_error"
    JSON_SCHEMA_VIOLATION = "json_schema_violation"
    CONTENT_FILTER = "content_filter"
    UNSUPPORTED_CAPABILITY = "unsupported_capability"
    UNKNOWN_PROVIDER_ERROR = "unknown_provider_error"


class ModelTurnStatus(str, Enum):
    SUCCESS = "success"
    FAILED = "failed"
    INVALID = "invalid"
    TIMEOUT = "timeout"
    CANCELLED = "cancelled"
    BUDGET_EXCEEDED = "budget_exceeded"


@dataclass
class ContentBlock(SerializableDataclass):
    type: ContentBlockType
    text: str | None = None
    artifact_ref: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_text(cls, text: str, **metadata: Any) -> "ContentBlock":
        return cls(type=ContentBlockType.TEXT, text=text, metadata=metadata)


@dataclass
class ModelMessage(SerializableDataclass):
    role: ModelRole
    content: list[ContentBlock]
    name: str | None = None
    tool_call_id: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def assistant_text(cls, text: str) -> "ModelMessage":
        return cls(role=ModelRole.ASSISTANT, content=[ContentBlock.from_text(text)])

    @property
    def text(self) -> str:
        return "".join(block.text or "" for block in self.content)


@dataclass
class ModelToolSchema(SerializableDataclass):
    name: str
    description: str
    parameters_schema: dict[str, Any]
    capability_tags: list[str] = field(default_factory=list)
    risk_tags: list[str] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass
class ToolChoicePolicy(SerializableDataclass):
    mode: ToolChoiceMode = ToolChoiceMode.AUTO
    tool_name: str | None = None
    allowed_tool_names: list[str] = field(default_factory=list)
    max_tool_calls: int = 8


@dataclass
class ModelToolCall(SerializableDataclass):
    tool_call_id: str
    tool_name: str
    arguments: dict[str, Any]
    raw_arguments: str
    parse_status: ModelToolParseStatus
    validation_errors: list[str] = field(default_factory=list)
    provider_metadata: dict[str, Any] = field(default_factory=dict)

    def to_provider_tool_call(self) -> dict[str, Any]:
        return {
            "id": self.tool_call_id,
            "type": "function",
            "function": {
                "name": self.tool_name,
                "arguments": self.raw_arguments,
            },
        }


@dataclass
class ModelCapabilities(SerializableDataclass):
    supports_tools: bool = True
    supports_parallel_tool_calls: bool = False
    supports_streaming: bool = False
    supports_json_mode: bool = False
    supports_system_message: bool = True
    supports_developer_message: bool = False
    max_context_tokens: int = 128000
    max_output_tokens: int = 4096
    input_modalities: list[str] = field(default_factory=lambda: ["text"])
    output_modalities: list[str] = field(default_factory=lambda: ["text"])


@dataclass
class ModelPreferences(SerializableDataclass):
    provider_name: str | None = None
    model_name: str | None = None
    temperature: float | None = None
    top_p: float | None = None
    max_output_tokens: int | None = None
    json_mode: bool = False
    stream: bool = False
    fallback_models: list[str] = field(default_factory=list)


@dataclass
class ModelBudget(SerializableDataclass):
    max_input_tokens: int | None = None
    max_output_tokens: int | None = None
    max_total_tokens: int | None = None
    max_retries: int = 2
    max_latency_ms: int | None = None
    max_cost_estimate: float | None = None


@dataclass
class ModelUsage(SerializableDataclass):
    input_tokens: int = 0
    output_tokens: int = 0
    total_tokens: int = 0
    cached_input_tokens: int = 0
    reasoning_tokens: int = 0
    cost_estimate: float | None = None

    def __post_init__(self) -> None:
        if not self.total_tokens:
            self.total_tokens = self.input_tokens + self.output_tokens


@dataclass
class ModelTurnRequest(SerializableDataclass):
    request_id: str
    run_id: str
    session_id: str
    task_id: str
    phase_id: str
    action_id: str
    purpose: ModelPurpose
    messages: list[ModelMessage]
    tools: list[ModelToolSchema] = field(default_factory=list)
    tool_choice: ToolChoicePolicy = field(default_factory=ToolChoicePolicy)
    model_preferences: ModelPreferences = field(default_factory=ModelPreferences)
    budget: ModelBudget = field(default_factory=ModelBudget)
    context_metadata: dict[str, Any] = field(default_factory=dict)
    policy_metadata: dict[str, Any] = field(default_factory=dict)
    trace_metadata: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def simple(
        cls,
        *,
        messages: list[ModelMessage | dict[str, Any]],
        purpose: ModelPurpose = ModelPurpose.PLAN_NEXT_ACTION,
    ) -> "ModelTurnRequest":
        request_id = f"model_req_{uuid4().hex[:12]}"
        return cls(
            request_id=request_id,
            run_id=request_id,
            session_id=request_id,
            task_id=request_id,
            phase_id="model",
            action_id=request_id,
            purpose=purpose,
            messages=[
                ModelMessage.from_dict(message) if isinstance(message, dict) else message
                for message in messages
            ],
        )

    def __post_init__(self) -> None:
        self.messages = [_model_message_from_payload(message) for message in self.messages]


@dataclass
class ModelValidationResult(SerializableDataclass):
    valid: bool
    errors: list[str] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    repaired: bool = False
    repair_message: str | None = None


@dataclass
class ModelError(Exception, SerializableDataclass):
    kind: ModelErrorKind
    message: str
    retryable: bool = False
    provider_name: str | None = None
    model_name: str | None = None
    raw_error_ref: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        Exception.__init__(self, self.message)


@dataclass
class ModelTurnResult(SerializableDataclass):
    request_id: str
    response_id: str
    status: ModelTurnStatus
    assistant_message: ModelMessage | None = None
    tool_calls: list[ModelToolCall] = field(default_factory=list)
    usage: ModelUsage = field(default_factory=ModelUsage)
    finish_reason: str | None = None
    validation: ModelValidationResult | None = None
    error: ModelError | None = None
    provider_name: str | None = None
    model_name: str | None = None
    latency_ms: int | None = None
    trace_event_ids: list[str] = field(default_factory=list)
    raw_response_ref: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)


def _to_plain(value: Any) -> Any:
    if isinstance(value, Enum):
        return value.value
    if is_dataclass(value) and not isinstance(value, type):
        return {key: _to_plain(item) for key, item in asdict(value).items()}
    if isinstance(value, list):
        return [_to_plain(item) for item in value]
    if isinstance(value, dict):
        return {key: _to_plain(item) for key, item in value.items()}
    return value


def _content_blocks_from_payload(content: Any) -> list[ContentBlock]:
    if content is None:
        return []
    if isinstance(content, str):
        return [ContentBlock.from_text(content)]
    if isinstance(content, ContentBlock):
        return [content]
    if not isinstance(content, list):
        return [ContentBlock.from_text(str(content))]

    blocks: list[ContentBlock] = []
    for item in content:
        if isinstance(item, ContentBlock):
            blocks.append(item)
        elif isinstance(item, str):
            blocks.append(ContentBlock.from_text(item))
        elif isinstance(item, dict):
            block_payload = dict(item)
            block_payload.setdefault("type", ContentBlockType.TEXT.value)
            blocks.append(ContentBlock.from_dict(block_payload))
        else:
            blocks.append(ContentBlock.from_text(str(item)))
    return blocks


def _model_message_from_payload(payload: ModelMessage | dict[str, Any]) -> ModelMessage:
    if isinstance(payload, ModelMessage):
        return payload
    return ModelMessage(
        role=ModelRole(payload["role"]),
        content=_content_blocks_from_payload(payload.get("content")),
        name=payload.get("name"),
        tool_call_id=payload.get("tool_call_id"),
        metadata=dict(payload.get("metadata") or {}),
    )


def _from_payload(cls: Any, payload: dict[str, Any]) -> Any:
    if cls is ContentBlock:
        return ContentBlock(
            type=ContentBlockType(payload["type"]),
            text=payload.get("text"),
            artifact_ref=payload.get("artifact_ref"),
            metadata=dict(payload.get("metadata") or {}),
        )
    if cls is ModelMessage:
        return _model_message_from_payload(payload)
    if cls is ModelToolSchema:
        return ModelToolSchema(
            name=str(payload["name"]),
            description=str(payload.get("description") or ""),
            parameters_schema=dict(payload.get("parameters_schema") or {}),
            capability_tags=list(payload.get("capability_tags") or []),
            risk_tags=list(payload.get("risk_tags") or []),
            metadata=dict(payload.get("metadata") or {}),
        )
    if cls is ToolChoicePolicy:
        return ToolChoicePolicy(
            mode=ToolChoiceMode(payload.get("mode") or ToolChoiceMode.AUTO.value),
            tool_name=payload.get("tool_name"),
            allowed_tool_names=list(payload.get("allowed_tool_names") or []),
            max_tool_calls=int(payload.get("max_tool_calls") or 8),
        )
    if cls is ModelToolCall:
        return ModelToolCall(
            tool_call_id=str(payload["tool_call_id"]),
            tool_name=str(payload["tool_name"]),
            arguments=dict(payload.get("arguments") or {}),
            raw_arguments=str(payload.get("raw_arguments") or "{}"),
            parse_status=ModelToolParseStatus(payload.get("parse_status") or ModelToolParseStatus.VALID.value),
            validation_errors=list(payload.get("validation_errors") or []),
            provider_metadata=dict(payload.get("provider_metadata") or {}),
        )
    if cls is ModelCapabilities:
        return ModelCapabilities(**payload)
    if cls is ModelPreferences:
        return ModelPreferences(**payload)
    if cls is ModelBudget:
        return ModelBudget(**payload)
    if cls is ModelUsage:
        return ModelUsage(**payload)
    if cls is ModelValidationResult:
        return ModelValidationResult(**payload)
    if cls is ModelError:
        return ModelError(
            kind=ModelErrorKind(payload["kind"]),
            message=str(payload.get("message") or ""),
            retryable=bool(payload.get("retryable")),
            provider_name=payload.get("provider_name"),
            model_name=payload.get("model_name"),
            raw_error_ref=payload.get("raw_error_ref"),
            metadata=dict(payload.get("metadata") or {}),
        )
    if cls is ModelTurnRequest:
        return ModelTurnRequest(
            request_id=str(payload["request_id"]),
            run_id=str(payload["run_id"]),
            session_id=str(payload["session_id"]),
            task_id=str(payload["task_id"]),
            phase_id=str(payload["phase_id"]),
            action_id=str(payload["action_id"]),
            purpose=ModelPurpose(payload["purpose"]),
            messages=[_model_message_from_payload(item) for item in payload.get("messages") or []],
            tools=[ModelToolSchema.from_dict(item) for item in payload.get("tools") or []],
            tool_choice=ToolChoicePolicy.from_dict(payload.get("tool_choice") or {}),
            model_preferences=ModelPreferences.from_dict(payload.get("model_preferences") or {}),
            budget=ModelBudget.from_dict(payload.get("budget") or {}),
            context_metadata=dict(payload.get("context_metadata") or {}),
            policy_metadata=dict(payload.get("policy_metadata") or {}),
            trace_metadata=dict(payload.get("trace_metadata") or {}),
        )
    if cls is ModelTurnResult:
        return ModelTurnResult(
            request_id=str(payload["request_id"]),
            response_id=str(payload["response_id"]),
            status=ModelTurnStatus(payload["status"]),
            assistant_message=ModelMessage.from_dict(payload["assistant_message"]) if payload.get("assistant_message") else None,
            tool_calls=[ModelToolCall.from_dict(item) for item in payload.get("tool_calls") or []],
            usage=ModelUsage.from_dict(payload.get("usage") or {}),
            finish_reason=payload.get("finish_reason"),
            validation=ModelValidationResult.from_dict(payload["validation"]) if payload.get("validation") else None,
            error=ModelError.from_dict(payload["error"]) if payload.get("error") else None,
            provider_name=payload.get("provider_name"),
            model_name=payload.get("model_name"),
            latency_ms=payload.get("latency_ms"),
            trace_event_ids=list(payload.get("trace_event_ids") or []),
            raw_response_ref=payload.get("raw_response_ref"),
            metadata=dict(payload.get("metadata") or {}),
        )
    return cls(**payload)

