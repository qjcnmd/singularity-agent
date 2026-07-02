# Context Assembly / Prompt Frame模块数据流

模块数据流文档 ID: context-assembly-prompt-frame

源码证据路径:
- src/singularity/context/models.py
- src/singularity/context/manager.py
- src/singularity/context/assembler.py
- src/singularity/context/compaction.py
- src/singularity/context/compression.py
- src/singularity/instructions/prompt_assembly.py
- src/singularity/instructions/models.py

关键符号:
- ContextItem
- ContextReference
- ContextBudgetPlan
- ContextBundle
- ContextSnapshot
- ToolObservation
- PlannerState
- PolicyObservation
- VerificationEvidence
- MutationEvidence
- CommandObservation
- ContextSummaryPayload
- ContextSummaryEnvelope
- CacheAttribution
- PromptManifest
- PromptBundle

字段清单:
- ContextReference: ref_id, ref_type, target, path, line_start, line_end, digest, observed_at, freshness, source_item_id, metadata, observation_id
- ContextItem: item_id, run_id, session_id, task_id, phase_id, layer, source_component, item_type, content, content_digest, created_at, updated_at, importance, relevance_score, authority, freshness, sensitivity, token_count, references, metadata, pinned, expires_at
- ContextBudgetPlan: model_context_window, output_token_reserve, reasoning_token_reserve, tool_schema_tokens, system_tokens, pinned_tokens, evidence_tokens, recent_dialogue_tokens, summary_tokens, available_tokens, used_tokens, overflow_tokens, soft_limit, hard_limit, message_tokens
- ContextRenderPolicy: include_raw_tool_outputs, include_policy_details, include_secret_content, include_full_diff, include_failed_attempts, max_tool_preview_tokens, max_evidence_items, max_recent_turns, require_references_for_claims, redact_sensitive, phase_aware
- ContextBundle: bundle_id, run_id, task_id, phase_id, model, provider, messages, included_item_ids, excluded_item_ids, budget, compression_snapshot_id, retrieval_query, render_policy, created_at, bundle_digest, metadata
- ContextSnapshot: snapshot_id, run_id, session_id, task_id, goal, summary, retained_item_ids, known_observation_ids, version, created_at, retained_messages, metadata
- ToolObservation: id, tool_name, tool_call_id, ok, raw_result, preview, truncated, metadata, run_id, turn, created_at, input_tokens, preview_tokens, raw_digest, source_refs, cache_hit, duration_seconds, error_code, tool_version, truncation_reason, sensitivity
- PlannerState: task_id, current_phase, status, current_plan, completion_criteria, open_actions, blocked_actions, risk_escalations, evidence_refs
- PolicyObservation: decision_id, request_id, outcome, risk_level, reason, constraints_summary, user_decision, approval_grant_id, component, operation, resource, reference
- VerificationEvidence: check_id, command, status, failure_summary, parsed_failures, repair_hints, logs_ref, confidence
- MutationEvidence: transaction_id, files_changed, diff_summary, rollback_ref, status
- CommandObservation: command_id, command_preview, exit_code, status, stdout_preview, stderr_preview, output_ref, resource_limits, policy_decision_id
- ContextUsageReport: layer_token_usage, included_item_ids, excluded_item_ids, stale_item_ids, summary_item_ids, recent_tail_item_ids, input_tokens, cached_input_tokens, cache_hit_ratio, cache_miss_reasons, cache_attribution, recommendations
- CacheAttribution: source, confidence, reasons, evidence, provider_name, model_name
- ContextSummaryPayload: goal, current_state, completed_actions, pending_actions, verified_facts, failed_attempts, policy_constraints, workspace_changes, verification_status, open_questions, reference_ids, omitted_item_ids, confidence
- ContextSummaryEnvelope: version, summary_id, summary_payload, source_item_ids, cache_attribution, previous_summary_digest, summary_digest, rendered_summary, created_at, metadata
- PromptManifest: manifest_id, bundle_id, purpose, source_count, section_count, trust_summary, priority_summary, conflict_count, injection_warning_count, redaction_applied, prompt_hash, token_estimate, folded_developer_into_system, metadata
- PromptBundle: bundle_id, purpose, messages, sections, manifest, token_estimate, prompt_hash, created_at, metadata

## 这一层解决什么问题

Context 层把系统提示、用户目标、planner 状态、memory、project index、工具观察和验证证据整理为可进入模型请求的上下文 bundle。

## 当前源码位置

- src/singularity/context/models.py
- src/singularity/context/manager.py
- src/singularity/context/assembler.py
- src/singularity/context/compaction.py
- src/singularity/context/compression.py
- src/singularity/instructions/prompt_assembly.py
- src/singularity/instructions/models.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`AgentGraphBuilder._build_model_context()` 创建 `ContextManager` -> `AgentLoop.run()` 每 turn 写入 planner/model/tool/verification 观察 -> `ModelRunner.build_request_from_context()` 读取 bundle 并构造 `ModelTurnRequest`。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`ContextManager.add_user_message()`、`add_assistant_message()`、`add_tool_result()`、`add_tool_protocol_result()`、`add_synthetic_tool_error()`、`add_trace_summary()`、`add_policy_observation()`、`add_planner_state()`、`add_mutation_evidence()`、`add_command_observation()`、`add_verification_evidence()`、`add_workspace_state()`、`add_edit_result()`、`add_project_index()`、`add_memory_context_block()` 与 `add_failure()` 把组件观察投影成 `ContextItem`、`ContextReference` 或专门 observation dataclass，并通过 `ObservationStore.append_message()` / `append_item()` 写入 `context.sqlite3`。随后 `ModelRunner.build_request_from_context()` -> `ContextManager.build_bundle()` -> `ContextAssembler.build_bundle()` 读取这些 item，生成 `ContextBundle` 与 `ContextUsageReport`；同一 request 构建过程调用 `PromptAssemblyPipeline.build_for_model_turn()`，它内部通过 `collect_sources()`、`resolve()`、`build_prompt_bundle()` 和 `compile_prompt()` 生成 `PromptBundle` 与 `PromptManifest`，再由 `ModelTurnRequestBuilder.build_request()` 合并为 `ModelTurnRequest.messages`。溢出时 `ContextAssembler.needs_compression()` 触发 compaction；失败返回 `ContextOverflowError` 或带 excluded item 的 usage report，不把所有 context 无界送入 provider。

## 真实对象完整结构

### ContextItem（上下文条目）

context 层的核心存储单元，所有组件的观察结果先转为 ContextItem 再进入选择/预算/渲染流程。**边界**：内部治理对象，落盘到 `context.sqlite3` 的 `context_items` 表；只有通过 visibility、预算与 redaction 的 item 内容进入 `ContextBundle.messages`，完整 item 元数据不发送给 provider。

```python
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
```

### ContextBundle（上下文包）

进入模型前的消息集合和预算诊断。**边界**：内部治理对象，落盘到 `context.sqlite3` 的 `context_bundles` 表；其 `messages` 字段直接组成 `ModelTurnRequest.messages`，`budget`/`render_policy` 只用于内部诊断。

```python
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
```

### ContextBudgetPlan（上下文预算计划）

token 预算分配与溢出诊断。**边界**：内部治理对象，嵌入 ContextBundle 行落盘；不独立写 trace 或进入模型。

```python
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
    soft_limit: int = 0    # int(model_context_window * 0.9)
    hard_limit: int = 0    # model_context_window
    message_tokens: int = 0
```

### Context 观察与快照对象

这些对象是 ContextManager 的 typed observation 输入或 context store snapshot。**边界**：内部对象；可投影成 ContextItem 或 snapshot 行落盘，只有经过 `ContextAssembler.build_bundle()` 选择、预算与 redaction 的摘要进入模型消息。

```python
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
class CacheAttribution:
    source: CacheAttributionSource = CacheAttributionSource.UNKNOWN
    confidence: float = 0.0
    reasons: list[str] = field(default_factory=list)
    evidence: list[str] = field(default_factory=list)
    provider_name: str | None = None
    model_name: str | None = None
```

### 关键枚举值域

```python
class ContextLayer(str, Enum):       # ContextItem.layer
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

class ContextItemType(str, Enum):    # ContextItem.item_type
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

class ContextAuthority(str, Enum):   # ContextItem.authority
    USER = "user"
    SYSTEM = "system"
    COMPONENT = "component"
    TOOL = "tool"
    MODEL = "model"
    SUMMARY = "summary"

class ContextSensitivity(str, Enum): # ContextItem.sensitivity
    PUBLIC = "public"
    WORKSPACE = "workspace"
    SENSITIVE = "sensitive"
    SECRET = "secret"
```

### 数据流概述

各组件通过 `ContextManager.add_*()` 入口生成 `ContextItem`，写入 `context.sqlite3`。`ContextAssembler.build_bundle()` 根据 token counter、phase、visibility、freshness、sensitivity 和 `ContextRenderPolicy` 选择 item，生成 `ContextBundle` 和 `ContextUsageReport`。`ContextBundle.messages` 直接组成 `ModelTurnRequest.messages`，但 `ContextBudgetPlan`、`ContextRenderPolicy` 和 `ContextUsageReport` 只用于内部诊断。`PromptAssemblyPipeline.build_for_model_turn()` 收集 instruction sources，`PromptCompiler.compile()` 生成 `PromptManifest` 和 `PromptBundle`；`PromptBundle.messages` 与 context messages 在 request builder 合并，`PromptManifest` 不进模型，只用于 hash、预算、trace 与诊断。

## 谁生成这些对象

- `ContextManager._make_item()` 与各 `add_*` 入口生成 `ContextItem` 和 `ContextReference`；tool result、planner state、memory、project index、policy、verification 与 assistant message 都先转换成这两个内部对象。
- `ContextAssembler.build_bundle()` 根据 token counter、phase、visibility、freshness、sensitivity 和 `ContextRenderPolicy` 生成 `ContextBudgetPlan`、`ContextBundle` 与初始 `ContextUsageReport`；usage reporter 再用实际 provider usage 更新报告。
- compaction executor 生成并校验 `ContextSummaryPayload`，`summary_envelope_for_plan()` 生成 `ContextSummaryEnvelope`。`PromptAssemblyPipeline.build_for_model_turn()` 收集/解析 instruction sources，`PromptCompiler.compile()` 生成 `PromptManifest` 与 `PromptBundle`。

## 谁消费这些对象

- ObservationStore、assembler、compaction 和 failure request 消费 `ContextReference`/`ContextItem`。只有通过 visibility、预算与 redaction 的 item 内容进入 `ContextBundle.messages`；完整 item/reference 元数据不发送给 provider。
- `ModelRunner.build_request_from_context()` 消费 `ContextBundle`；其中 `messages` 直接组成 `ModelTurnRequest.messages`，`ContextBudgetPlan`、`ContextRenderPolicy` 与 `ContextUsageReport` 只用于内部诊断，不作为消息正文。
- compaction committer/recovery 消费 `ContextSummaryEnvelope`，其 `rendered_summary` 通过 summary item 进入后续模型请求。`ModelTurnRequestBuilder.build_request()` 消费 `PromptBundle.messages` 并与 context messages 合并；`PromptManifest` 不进模型，只用于 hash、预算、trace 与诊断。

## 是否落盘

- `ObservationStore` 在当前 trace run 目录的 `context.sqlite3` 写 `context_items`、`context_references`、tool observations/messages、`context_bundles` 和 snapshot 数据。`ContextBudgetPlan`、`ContextRenderPolicy`、usage metadata 嵌在 bundle 行内。
- `ContextSummaryPayload`/`ContextSummaryEnvelope` 嵌入 summary `ContextItem` 与 snapshot metadata，不另建独立表。`PromptBundle` 不写 context DB，也不保存完整 prompt 正文副本。
- 配置 `store_prompt_manifest` 时，`PromptAssemblyPipeline._emit_bundle_events()` 通过 `TraceRecorder.write_artifact()` 写 redacted prompt manifest artifact；默认 trace artifact 索引为 `work/traces/runs/<run_id>/artifacts.jsonl`，文件在同目录 `artifacts/`。

## 是否进入 trace / audit

- context 增量写 `context.item_added` 等摘要事件；bundle 构造写 `context.bundle_built`、`context.rendered_for_model`，payload 只含 bundle id、included/excluded ids、token 统计、`duration_ms` 和 `compaction_decision_duration_ms`，不写完整敏感正文。`messages()` 单独测量是否需要 compaction 的决策时间，再将其附到同一次 bundle persist，避免重复构建或重复持久化。实际 cache usage 写 `context.cache_usage_recorded`。
- prompt assembly 写 `instruction_sources_collected`、`instruction_conflict_detected`、`instruction_injection_detected`、`prompt_compiled` 与 `prompt_manifest_created`。injection excerpt 在事件前替换成 hash/`<redacted>`；manifest artifact 仅在配置开启时产生。
- 本层不写 policy audit；若 context 来源是 policy observation，保存的是已经由 PolicyEngine/audit 产生并经 ContextManager 投影的摘要。

## 失败路径

- bundle 超过 hard limit 且无法通过选择/压缩收敛时 `ContextAssembler` 抛 `ContextOverflowError`；敏感、过期、低相关或超预算 item 进入 `excluded_item_ids`，不通过“截断后仍发送原文”的方式绕过。
- summary 的 invalid JSON、缺 reference、内容漂移、previous/summary digest 或版本不匹配会使 compaction validation/commit 失败，旧 snapshot 仍保持有效。
- prompt 检测到 critical injection 且 `fail_on_critical_injection` 开启时抛 `PromptInjectionWarning`；token estimate 超 `max_prompt_tokens` 时抛 `PromptBudgetExceeded`。`build_for_model_turn()` 将 instruction span 标记 failed 后继续抛出，不降级为未审查 prompt。

## 当前结构问题

`ContextBundle.messages` 与 `PromptBundle.messages` 最终在 request builder 合并，但两套 bundle 的预算/hash/trace 责任不同；维护时必须分别说明“上下文选择”和“指令编译”，不能把所有 ContextItem 描述成模型可见，也不能把 prompt manifest 当作 provider payload。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
