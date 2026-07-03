# ModelTurn / Provider / Tools Exposure模块数据流

模块数据流文档 ID: model-turn-provider-tools

源码证据路径:
- src/singularity/model/models.py
- src/singularity/model/runner.py
- src/singularity/model/request_builder.py
- src/singularity/model/tools.py
- src/singularity/model/providers.py
- src/singularity/model/openai_format.py
- src/singularity/model/streaming.py
- src/singularity/model/messages.py
- src/singularity/model/validation.py
- src/singularity/model/budget.py

关键符号:
- ModelTurnRequest
- ModelTurnResult
- ModelMessage
- ModelToolSchema
- ModelToolCall
- ToolChoiceMode
- ToolChoicePolicy
- ModelRunner
- ProviderRequest
- ProviderResponse
- ProviderStreamEvent
- StreamingAccumulator
- ChatCompletionProvider
- OpenAICompatibleModelProvider
- model_messages_to_openai
- model_tool_to_openai
- serialize_tool_choice
- provider_tool_call_dict

字段清单:
- ContentBlock: type, text, artifact_ref, metadata
- ModelMessage: role, content, name, tool_call_id, metadata
- ModelToolSchema: name, description, parameters_schema, capability_tags, risk_tags, metadata
- ToolChoicePolicy: mode, tool_name, allowed_tool_names, max_tool_calls
- ModelToolCall: tool_call_id, tool_name, arguments, raw_arguments, parse_status, validation_errors, provider_metadata
- ModelCapabilities: supports_tools, supports_parallel_tool_calls, supports_streaming, supports_json_mode, supports_structured_outputs, supports_system_message, supports_developer_message, max_context_tokens, max_output_tokens, input_modalities, output_modalities
- ModelPreferences: provider_name, model_name, temperature, top_p, max_output_tokens, json_mode, structured_output_schema, stream, fallback_models
- ModelBudget: max_input_tokens, max_output_tokens, max_total_tokens, max_retries, max_latency_ms, max_cost_estimate
- ModelUsage: input_tokens, output_tokens, total_tokens, cached_input_tokens, reasoning_tokens, cost_estimate
- ModelTurnRequest: request_id, run_id, session_id, task_id, phase_id, action_id, purpose, messages, tools, tool_choice, model_preferences, budget, context_metadata, policy_metadata, trace_metadata
- ModelValidationResult: valid, errors, warnings, repaired, repair_message
- ModelError: kind, message, retryable, provider_name, model_name, raw_error_ref, metadata
- ModelTurnResult: request_id, response_id, status, assistant_message, tool_calls, usage, finish_reason, validation, error, provider_name, model_name, latency_ms, trace_event_ids, raw_response_ref, metadata
- ProviderRequest: request_id, purpose, messages, tools, tool_choice, preferences, policy_metadata, trace_metadata
- ProviderResponse: response_id, message, tool_calls, usage, finish_reason, provider_name, model_name, raw_response
- ProviderStreamEvent: type, text_delta, tool_call_id, tool_name, arguments_delta, usage_delta, error, metadata

## 这一层解决什么问题

模型层把上下文、工具 schema、tool choice、budget 和 provider 偏好组装为 `ModelTurnRequest（模型单轮请求）`，再把 provider 响应解析为 `ModelTurnResult（模型单轮结果）`。

## 当前源码位置

- src/singularity/model/models.py
- src/singularity/model/runner.py
- src/singularity/model/request_builder.py
- src/singularity/model/tools.py
- src/singularity/model/providers.py
- src/singularity/model/openai_format.py
- src/singularity/model/streaming.py
- src/singularity/model/messages.py
- src/singularity/model/validation.py
- src/singularity/model/budget.py

## 关键类、函数、字段

关键符号和字段清单按源码声明顺序列出，便于和对象流小节对照。

## 真实运行时调用链

`AgentLoop.run()` -> `ModelRunner.build_request_from_context()` -> `PromptAssemblyPipeline` -> provider registry -> provider chat/completion 或 streaming -> `ModelRunner.run_turn()` -> `ModelTurnResult` -> tool protocol 或 finalization。provider 对外实现以 `OpenAICompatibleModelProvider` 为当前 OpenAI-compatible adapter；`ChatCompletionProvider` 只是 `AgentLoop` 和 context compaction fallback 使用的最小 Protocol，不是第二套 provider runtime。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`ModelRunner.build_request_from_context()` -> `ModelTurnRequestBuilder.build_request()` 先把 `ContextBundle`、`PromptBundle`、`ModelToolRenderer.render()` 生成的 `ModelToolSchema`、planner context 和 allowed tool names 生成对象 `ModelTurnRequest`。`ModelTurnRequest.context_metadata["tool_protocol"]` / `trace_metadata["tool_protocol"]` 记录工具协议结构化元数据（version、tool_names、tool_count、has_verification_tools、has_edit_tools），不再把动态工具协议摘要拼入 model-visible prompt。`ModelRunner.run_turn()` 把 request 投影成 provider payload，只发送 messages、tool schema、tool choice、budget 与必要生成参数；OpenAI-compatible payload 的 messages、tools 和 tool choice 由 `openai_format.py` 中的 `model_messages_to_openai()`、`model_tool_to_openai()` 和 `serialize_tool_choice()` 统一生成，`OpenAICompatibleModelProvider` 不再维护第二份格式转换，`ModelToolRenderer.to_provider_tools()` 也复用同一 tool schema 投影。非 streaming 时 provider 返回完整 response，`ProviderResponse.from_openai_response()` 解析 message、tool calls 和 usage。streaming 时 `ModelRunner._send_with_retry()` 生成 `ProviderRequest` 并调用 `ModelRunner._stream_provider_response()`，OpenAI-compatible provider 发送 `stream=true` 和 `stream_options.include_usage=true` 到 chat completions endpoint，逐行读取 SSE `data:` chunk，把 content delta、tool call delta、usage payload 和 finish reason 转成 `ProviderStreamEvent`；`StreamingAccumulator` 合并文本和工具参数，最终生成 `ProviderResponse`。review critic 和 final reviewer 复用同一 `ModelRunner.run_turn()` 通道，但模型辅助审查输出先经过共享 output boundary：优先 Structured Outputs / JSON Schema；provider 或 adapter 不支持时使用 strict tool calling，并通过 tool choice pinned 到单个 tool；仍不支持时才使用 `json_mode`。Structured Outputs 由 `ModelPreferences.structured_output_schema` 投影为 `response_format={"type":"json_schema", "json_schema": ...}`；strict tool calling 由 `ModelToolSchema.metadata["strict"]` 和 `ToolChoicePolicy(mode=SPECIFIC_TOOL, tool_name=...)` 投影到 provider payload，请求文案要求 exactly one tool call 和 JSON object arguments；`json_mode` fallback 投影为 `response_format={"type":"json_object"}`。所有路径返回后仍做本地 schema validation / Pydantic validation；JSON parse error、schema validation error、tool-call argument parse/schema mismatch 只做 bounded retry，并在安全 metadata 中记录 `retry_reason` 的枚举值：`none`、`json_parse_error`、`schema_validation_error`、`tool_call_parse_error`、`provider_error` 或 `business_rule_validation_failed`。strict tool calling 的 review boundary 只在目标 tool 存在、`raw_arguments` 是 JSON object 且同一 Pydantic schema 与 business validator 通过时，接受 raw argument projection 来避免临时 review tool 与普通 tool registry 边界造成的无价值 parse retry；格式错误、非 object、schema validation failure 仍是 bounded retry。provider transient error 由 `ModelRetryController` 执行 bounded retry 和 exponential backoff with jitter，review output boundary 只记录 provider fallback path；business-rule validation failure 直接走 rule-only fallback path，形成 graceful degradation，但不触发模型重试，也不改变 fail-closed deterministic gate 或 hard gate。随后 `ModelRunner._normalize_tool_calls()` 生成 `ModelToolCall`，`ModelToolCall.to_provider_tool_call()` 通过 `provider_tool_call_dict()` 生成 provider tool-call projection，tool protocol 中的 `ToolCallEnvelope.to_provider_tool_call()` 复用同一函数。`_emit_response_received()` 写 trace 事件，`_write_raw_artifact()` 在配置允许时写 redacted raw artifact。`ModelTurnResult.usage` 被 `ContextManager.record_model_usage()` 消费，`tool_calls` 进入 `ToolProtocolEngine.process_model_turn()`，完整 metadata 不写入 provider payload。

## 真实对象完整结构

### ModelTurnRequest（模型单轮请求）

进入 provider 的完整请求载体。**边界**：模型请求对象，投影成 provider JSON payload 后发送；消息正文、tool schema、tool choice 和生成参数进入 provider，但 `context_metadata`/`policy_metadata`/`trace_metadata` 不发送。

```python
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
```

### ModelTurnResult（模型单轮结果）

provider 响应的规范化载体。**边界**：内部治理对象，不落盘为独立文件；usage 投影写 context，tool_calls 进入 ToolProtocol，error/validation 进入 AgentLoop 决策。

```python
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
```

### ModelError（模型错误）

provider 失败的结构化载体。**边界**：内部治理对象，不落盘；其 kind/message 进入 trace event 和 AgentLoop 重试决策。

```python
@dataclass
class ModelError(Exception, SerializableDataclass):
    kind: ModelErrorKind
    message: str
    retryable: bool = False
    provider_name: str | None = None
    model_name: str | None = None
    raw_error_ref: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)
```

### ProviderRequest（provider 请求）

`ModelRunner._send_with_retry()` 投影给 provider adapter 的请求对象。**边界**：内部 adapter 对象，不直接落盘；OpenAI-compatible provider 只从它派生 chat completions payload。

```python
@dataclass
class ProviderRequest:
    request_id: str
    purpose: str
    messages: list[ModelMessage]
    tools: list[ModelToolSchema] = field(default_factory=list)
    tool_choice: ToolChoicePolicy = field(default_factory=ToolChoicePolicy)
    preferences: ModelPreferences = field(default_factory=ModelPreferences)
    policy_metadata: dict[str, Any] = field(default_factory=dict)
    trace_metadata: dict[str, Any] = field(default_factory=dict)
```

### ProviderResponse（provider 响应）

provider adapter 交回 `ModelRunner` 的规范化响应。**边界**：内部 adapter 对象；streaming 与 non-streaming 都归一到此对象后再生成 `ModelTurnResult`。

```python
@dataclass
class ProviderResponse:
    response_id: str
    message: ModelMessage
    tool_calls: list[ModelToolCall] = field(default_factory=list)
    usage: ModelUsage = field(default_factory=ModelUsage)
    finish_reason: str | None = None
    provider_name: str | None = None
    model_name: str | None = None
    raw_response: dict[str, Any] | None = None
```

### ProviderStreamEvent（provider streaming 事件）

provider streaming chunk 的内部事件。**边界**：只在 `provider.stream()` 和 `ModelRunner._stream_provider_response()` 之间流动；不作为独立 report schema 落盘。

```python
@dataclass
class ProviderStreamEvent:
    type: ProviderStreamEventType
    text_delta: str | None = None
    tool_call_id: str | None = None
    tool_name: str | None = None
    arguments_delta: str | None = None
    usage_delta: dict[str, Any] | None = None
    error: Any | None = None
    metadata: dict[str, Any] = field(default_factory=dict)
```

### 关键枚举值域

```python
class ModelPurpose(str, Enum):       # ModelTurnRequest.purpose
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

class ModelTurnStatus(str, Enum):    # ModelTurnResult.status
    SUCCESS = "success"
    FAILED = "failed"
    INVALID = "invalid"
    TIMEOUT = "timeout"
    CANCELLED = "cancelled"
    BUDGET_EXCEEDED = "budget_exceeded"

class ModelErrorKind(str, Enum):     # ModelError.kind
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

class ProviderStreamEventType(StrEnum):
    TEXT_DELTA = "text_delta"
    TOOL_CALL_DELTA = "tool_call_delta"
    TOOL_CALL_COMPLETED = "tool_call_completed"
    USAGE_DELTA = "usage_delta"
    RESPONSE_COMPLETED = "response_completed"
    ERROR = "error"
```

### 数据流概述

`ContextBundle.messages` + `PromptBundle.messages` 在 request builder 合并为 `ModelTurnRequest.messages`，`ModelToolRenderer.render()` 生成 `ModelToolSchema` 列表。provider 只看到 messages、tool schema、tool choice 和生成参数；`context_metadata`/`policy_metadata`/`trace_metadata` 不发送。工具协议可观测信息保存在 `context_metadata["tool_protocol"]` 和 `trace_metadata["tool_protocol"]`，不作为 prompt source 进入 `PromptAssemblyPipeline`。provider 返回后 `ModelRunner._normalize_tool_calls()` 生成 `ModelToolCall`，`_emit_response_received()` 写 trace event。streaming provider 返回 `ProviderStreamEvent`，由 `StreamingAccumulator` 合并成与 non-streaming 相同的 `ProviderResponse`。`ModelTurnResult.usage` 被 `ContextManager.record_model_usage()` 消费，`tool_calls` 进入 `ToolProtocolEngine.process_model_turn()`。

ReviewPipeline、ModelCritic、ReviewDecisionEngine、FinalReviewer、CompletionGate、EvidenceLedger、ReviewFinding 和 ReviewReport 是 Singularity 项目内部对象名，不是行业标准术语。描述模型辅助审查边界时使用行业术语：Structured Outputs、tool calling、tool choice、JSON Schema、schema validation、bounded retry、exponential backoff with jitter、fallback path、graceful degradation、fail-closed、deterministic gate、hard gate 和 model-assisted review。

## 谁生成这些对象

context/prompt/message converter 生成 `ContentBlock`/`ModelMessage`；`ModelToolRenderer.render()` 从 registry 生成 `ModelToolSchema`。`ModelTurnRequestBuilder.build_request()` 生成 choice、preferences、budget 与 `ModelTurnRequest`，provider adapter 提供 capabilities。`ModelRunner._send_with_retry()` 生成 `ProviderRequest`。
provider response parser/normalizer 生成 `ProviderResponse`、`ModelToolCall` 和 `ModelUsage`；streaming 路径由 provider adapter 生成 `ProviderStreamEvent`，`StreamingAccumulator` 合成 `ProviderResponse`。`ModelRunner._validate_response()` 生成 `ModelValidationResult`，`ModelRunner.run_turn()` 的成功/invalid/failed 分支生成 `ModelTurnResult` 与 `ModelError`。
`provider_tool_call_dict()` 生成 provider-compatible tool call dict，供 `ModelToolCall` 和 `ToolCallEnvelope` 共同使用，避免模型层和 tool protocol 层维护两套 provider tool-call projection。历史 `src/singularity/provider.py` 不再是运行时入口；OpenAI-compatible provider 入口在 `src/singularity/model/providers.py`。

## 谁消费这些对象

ModelRunner/provider adapter 消费 request。provider payload只含安全 messages、tool name/description/parameters/strict、序列化 tool choice 与支持的 generation 参数；streaming 路径额外发送 `stream=true` 和 usage streaming option。message/tool/request metadata、capability/risk、policy/trace metadata、budget 对象不发送。
`AgentLoop.run_turn()` 消费 validation/error/turn result，`ToolProtocolEngine.process_model_turn()` 消费 tool calls，`ContextManager.record_model_usage()` 消费 usage；这些 response 对象不自动进入下一轮模型，只有 ContextManager 追加的 assistant/tool message进入。

## 是否落盘

ModelTurnRequest/Result、ProviderRequest/Response 和 ProviderStreamEvent 没有独立 store；消息与 usage 投影写 `context.sqlite3`，raw provider request/response仅在配置允许时由 `ModelRunner._write_raw_artifact()` 写 redacted trace artifact。evaluation result聚合 token/cache/turn统计，不复制完整对象。

## 是否进入 trace / audit

ModelRunner 写 request-created、response-received、tool-call、output-rejected、request-failed events；payload保存 request/response ids、purpose、message/tool count、schema hash、usage、latency/error摘要和 artifact ref，不保存 message正文或 raw secrets。本层不写 policy audit。

## 失败路径

`ModelTurnStatus` 区分 invalid、failed、timeout、cancelled、budget_exceeded；provider auth/rate/network/timeout/invalid request映射为 `ModelError.kind/retryable`。streaming provider 产生 `ProviderStreamEventType.ERROR` 时，`ModelRunner._stream_provider_response()` 转成 `ModelError`，不会把半截 stream 当作成功。validator对 tool call invalid JSON/schema/unknown/duplicate/max-count返回错误，AgentLoop再决定 retry、blocked或fatal。

## 当前结构问题

内部 `ModelTurnRequest` 比 provider JSON宽；维护时必须同时核对 `_chat_completion_payload()`、`_model_messages_to_openai()`、`_model_tool_to_openai()`、tool-choice serialization 和 streaming SSE parser，防止内部 metadata/provenance泄漏，且避免 streaming 与 non-streaming 的 tool call/usage 语义分叉。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
