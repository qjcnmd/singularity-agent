# Trace / Observation / Audit Events模块数据流

模块数据流文档 ID: trace-observation-audit-events

源码证据路径:
- src/singularity/observability/models.py
- src/singularity/observability/recorder.py
- src/singularity/observability/store.py
- src/singularity/policy/audit.py

关键符号:
- TraceEvent
- TraceSpan
- TraceArtifact
- TraceTimelineItem
- TraceSummary
- TraceRecorder

字段清单:
- TraceEvent: event_id, event_type, run_id, session_id, task_id, phase_id, action_id, parent_event_id, timestamp, monotonic_ms, component, severity, summary, payload, artifact_refs, policy_decision_id, approval_grant_id, sandbox_id, command_id, transaction_id, verification_id, span_id, redaction_applied, payload_hash
- TraceSpan: span_id, parent_span_id, run_id, session_id, task_id, phase_id, action_id, name, component, started_at, ended_at, duration_ms, status, error_type, error_message, attributes, artifact_refs
- TraceArtifact: artifact_id, run_id, session_id, task_id, kind, path, relative_path, size_bytes, sha256, content_type, redacted, sensitive, summary, metadata
- TraceTimelineItem: timestamp, event_id, event_type, component, summary, severity, related_ids, artifact_refs
- TraceSummary: run_id, session_id, task_id, total_events, total_spans, total_artifacts, action_count, failed_action_count, command_count, sandboxed_command_count, mutation_count, verification_count, policy_denial_count, approval_count, replan_count, error_count, critical_events, key_artifacts, model_usage_summary

## 这一层解决什么问题

Trace 层记录运行事件、span、artifact、timeline 和 summary；audit 相关数据由 policy 与 approval 链路写入，用于复现和最终报告。

## 当前源码位置

- src/singularity/observability/models.py
- src/singularity/observability/recorder.py
- src/singularity/observability/store.py
- src/singularity/policy/audit.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

各组件调用 `trace.record()` / `trace.emit()` -> `TraceRecorder` redaction -> `TraceStore` 写 events/spans/artifacts -> timeline/summary/final report/evaluation result 引用。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`TraceRecorder.emit()` / `record()` -> `TraceStore.append_event()` 先生成对象 `TraceEvent` 并写入 run 目录 `events.jsonl`；`TraceRecorder.start_span()` / `end_span()` 生成 `TraceSpan` 写入 `spans.jsonl`，`TraceRecorder.write_artifact()` 生成 `TraceArtifact` 写入 `artifacts.jsonl` 和 `index.json`。`TraceStore.get_timeline()` 读取 events/spans 生成 `TraceTimelineItem`，`TraceStore.summarize()` 生成 `TraceSummary`；final report 和 evaluation result 只消费 summary/artifact refs。policy audit 仍由 `PolicyAuditWriter.append()` 写 `audit.jsonl`，trace event 不能替代 audit entry。

## 真实对象完整结构

### TraceEvent（追踪事件）

所有运行组件的原子记录。**边界**：trace 对象，落盘到 `events.jsonl`；摘要投影进 context，但完整 payload 不进入模型请求。

```python
@dataclass(frozen=True)
class TraceEvent:
    event_id: str
    event_type: TraceEventType       # 169 个成员的枚举
    run_id: str
    session_id: str
    task_id: str | None
    phase_id: str | None
    action_id: str | None
    parent_event_id: str | None
    timestamp: datetime
    monotonic_ms: int
    component: str
    severity: TraceSeverity
    summary: str
    payload: dict[str, Any] = field(default_factory=dict)
    artifact_refs: list[str] = field(default_factory=list)
    policy_decision_id: str | None = None
    approval_grant_id: str | None = None
    sandbox_id: str | None = None
    command_id: str | None = None
    transaction_id: str | None = None
    verification_id: str | None = None
    span_id: str | None = None
    redaction_applied: bool = True
    payload_hash: str = ""
```

### TraceSpan（追踪跨度）

记录有开始/结束时间的操作区间。**边界**：trace 对象，落盘到 `spans.jsonl`；不进入模型请求。

```python
@dataclass(frozen=True)
class TraceSpan:
    span_id: str
    parent_span_id: str | None
    run_id: str
    session_id: str
    task_id: str | None
    phase_id: str | None
    action_id: str | None
    name: str
    component: str
    started_at: datetime
    ended_at: datetime | None
    duration_ms: int | None
    status: TraceStatus
    error_type: str | None
    error_message: str | None
    attributes: dict[str, Any] = field(default_factory=dict)
    artifact_refs: list[str] = field(default_factory=list)
```

### TraceArtifact（追踪产物）

大输出（stdout/stderr、diff、report、prompt manifest）的文件引用。**边界**：trace 对象，落盘到 `artifacts.jsonl` + `artifacts/` 文件；artifact ref 进入 final report 和 evaluation result。

```python
@dataclass(frozen=True)
class TraceArtifact:
    artifact_id: str
    run_id: str
    session_id: str
    task_id: str | None
    kind: TraceArtifactKind
    path: Path
    relative_path: str
    size_bytes: int
    sha256: str
    content_type: str
    redacted: bool
    sensitive: bool
    summary: str
    metadata: dict[str, Any] = field(default_factory=dict)
```

### 关键枚举值域

```python
class TraceSeverity(StrEnum):    # TraceEvent.severity
    DEBUG = "debug"
    INFO = "info"
    WARNING = "warning"
    ERROR = "error"
    CRITICAL = "critical"

class TraceStatus(StrEnum):      # TraceSpan.status
    RUNNING = "running"
    SUCCESS = "success"
    FAILED = "failed"
    CANCELLED = "cancelled"
    TIMEOUT = "timeout"
    SKIPPED = "skipped"
    BLOCKED = "blocked"

class TraceArtifactKind(StrEnum): # TraceArtifact.kind
    STDOUT = "stdout"
    STDERR = "stderr"
    DIFF = "diff"
    REPORT = "report"
    SNAPSHOT = "snapshot"
    SANDBOX = "sandbox"
    VERIFICATION = "verification"
    EDIT_PLAN = "edit_plan"
    MODEL_MESSAGE = "model_message"
    PROMPT_MANIFEST = "prompt_manifest"
    COMMAND_LOG = "command_log"
    POLICY_AUDIT_REF = "policy_audit_ref"
    GENERIC = "generic"
```

`TraceEventType` 有 169 个成员，按 dotted-prefix 分组：

```python
class TraceEventType(StrEnum):
    # task (3)
    TASK_STARTED = "task.started"
    TASK_COMPLETED = "task.completed"
    TASK_FAILED = "task.failed"
    # phase (2)
    PHASE_STARTED = "phase.started"
    PHASE_COMPLETED = "phase.completed"
    # action (4)
    ACTION_PROPOSED = "action.proposed"
    ACTION_STARTED = "action.started"
    ACTION_COMPLETED = "action.completed"
    ACTION_FAILED = "action.failed"
    # planner (2)
    PLANNER_REPLAN_TRIGGERED = "planner.replan_triggered"
    PLANNER_COMPLETION_ASSESSED = "planner.completion_assessed"
    # semantic_planner (6)
    SEMANTIC_PLANNER_TASK_CONTRACT_MODEL_OK = "semantic_planner.task_contract.model_ok"
    SEMANTIC_PLANNER_TASK_CONTRACT_FALLBACK = "semantic_planner.task_contract.fallback"
    SEMANTIC_PLANNER_SEMANTIC_PLAN_MODEL_OK = "semantic_planner.semantic_plan.model_ok"
    SEMANTIC_PLANNER_SEMANTIC_PLAN_FALLBACK = "semantic_planner.semantic_plan.fallback"
    SEMANTIC_PLANNER_PLANNER_DECISION_MODEL_OK = "semantic_planner.planner_decision.model_ok"
    SEMANTIC_PLANNER_PLANNER_DECISION_FALLBACK = "semantic_planner.planner_decision.fallback"
    # final_reviewer (3)
    FINAL_REVIEWER_ASSESS_DONE = "final_reviewer.assess.done"
    FINAL_REVIEWER_ASSESS_MODEL_OK = "final_reviewer.assess.model_ok"
    FINAL_REVIEWER_ASSESS_FALLBACK = "final_reviewer.assess.fallback"
    # model (5)
    MODEL_REQUEST_CREATED = "model.request.created"
    MODEL_RESPONSE_RECEIVED = "model.response.received"
    MODEL_REQUEST_FAILED = "model.request.failed"
    MODEL_TOOL_CALL_PROPOSED = "model.tool_call.proposed"
    MODEL_OUTPUT_REJECTED = "model.output.rejected"
    # output (8)
    OUTPUT_PARSE_STARTED = "output.parse.started"
    OUTPUT_PARSE_SUCCEEDED = "output.parse.succeeded"
    OUTPUT_PARSE_FAILED = "output.parse.failed"
    OUTPUT_NORMALIZED = "output.normalized"
    OUTPUT_REPAIR_REQUESTED = "output.repair.requested"
    OUTPUT_REPAIR_SUCCEEDED = "output.repair.succeeded"
    OUTPUT_REPAIR_FAILED = "output.repair.failed"
    OUTPUT_FALLBACK_USED = "output.fallback.used"
    # tool_protocol (14)
    TOOL_PROTOCOL_BATCH_CREATED = "tool_protocol.batch_created"
    TOOL_PROTOCOL_CALL_VALIDATED = "tool_protocol.call_validated"
    TOOL_PROTOCOL_CALL_REJECTED = "tool_protocol.call_rejected"
    TOOL_PROTOCOL_PLAN_BUILT = "tool_protocol.plan_built"
    TOOL_PROTOCOL_CALL_SCHEDULED = "tool_protocol.call_scheduled"
    TOOL_PROTOCOL_CALL_STARTED = "tool_protocol.call_started"
    TOOL_PROTOCOL_CALL_COMPLETED = "tool_protocol.call_completed"
    TOOL_PROTOCOL_PARALLEL_GROUP_STARTED = "tool_protocol.parallel_group_started"
    TOOL_PROTOCOL_PARALLEL_GROUP_COMPLETED = "tool_protocol.parallel_group_completed"
    TOOL_PROTOCOL_RESULT_BOUND = "tool_protocol.result_bound"
    TOOL_PROTOCOL_SYNTHETIC_RESULT_CREATED = "tool_protocol.synthetic_result_created"
    TOOL_PROTOCOL_REPLAY_DETECTED = "tool_protocol.replay_detected"
    TOOL_PROTOCOL_RECOVERY_STARTED = "tool_protocol.recovery_started"
    TOOL_PROTOCOL_RECOVERY_COMPLETED = "tool_protocol.recovery_completed"
    # tool (5)
    TOOL_VALIDATION_STARTED = "tool.validation.started"
    TOOL_VALIDATION_FAILED = "tool.validation.failed"
    TOOL_DISPATCH_STARTED = "tool.dispatch.started"
    TOOL_DISPATCH_COMPLETED = "tool.dispatch.completed"
    TOOL_DISPATCH_FAILED = "tool.dispatch.failed"
    # plugin (10)
    PLUGIN_DISCOVERED = "plugin.discovered"
    PLUGIN_CHECK_FAILED = "plugin.check_failed"
    PLUGIN_ENABLED = "plugin.enabled"
    PLUGIN_DISABLED = "plugin.disabled"
    PLUGIN_LOAD_STARTED = "plugin.load_started"
    PLUGIN_LOAD_COMPLETED = "plugin.load_completed"
    PLUGIN_LOAD_FAILED = "plugin.load_failed"
    PLUGIN_ACTIVATED = "plugin.activated"
    PLUGIN_TOOL_REGISTERED = "plugin.tool_registered"
    PLUGIN_EVENT = "plugin.event"
    # policy (3)
    POLICY_REQUESTED = "policy.requested"
    POLICY_DECIDED = "policy.decided"
    POLICY_BLOCKED = "policy.blocked"
    # approval (3)
    APPROVAL_REQUESTED = "approval.requested"
    APPROVAL_GRANTED = "approval.granted"
    APPROVAL_DENIED = "approval.denied"
    # user_decision (1)
    USER_DECISION_RECORDED = "user_decision.recorded"
    # clarification (2)
    CLARIFICATION_REQUESTED = "clarification.requested"
    CLARIFICATION_ANSWERED = "clarification.answered"
    # control_command (1)
    CONTROL_COMMAND_RECEIVED = "control_command.received"
    # command (7)
    COMMAND_REQUESTED = "command.requested"
    COMMAND_STARTED = "command.started"
    COMMAND_OUTPUT_CHUNK = "command.output_chunk"
    COMMAND_COMPLETED = "command.completed"
    COMMAND_FAILED = "command.failed"
    COMMAND_TIMEOUT = "command.timeout"
    COMMAND_KILLED = "command.killed"
    # sandbox (7)
    SANDBOX_REQUESTED = "sandbox.requested"
    SANDBOX_PREPARED = "sandbox.prepared"
    SANDBOX_CAPABILITY_FAILED = "sandbox.capability_failed"
    SANDBOX_STARTED = "sandbox.started"
    SANDBOX_COMPLETED = "sandbox.completed"
    SANDBOX_VIOLATION = "sandbox.violation"
    SANDBOX_CLEANED = "sandbox.cleaned"
    # mutation (6)
    MUTATION_PROPOSED = "mutation.proposed"
    MUTATION_TRANSACTION_STARTED = "mutation.transaction_started"
    MUTATION_APPLIED = "mutation.applied"
    MUTATION_FAILED = "mutation.failed"
    MUTATION_ROLLBACK_STARTED = "mutation.rollback_started"
    MUTATION_ROLLBACK_COMPLETED = "mutation.rollback_completed"
    # patch (1)
    PATCH_PROPOSED = "patch.proposed"
    # edit (5)
    EDIT_PLAN_CREATED = "edit.plan_created"
    EDIT_PATCH_VALIDATED = "edit.patch_validated"
    EDIT_APPLIED = "edit.applied"
    EDIT_REPAIR_ATTEMPTED = "edit.repair_attempted"
    EDIT_FAILED = "edit.failed"
    # review (4)
    REVIEW_STARTED = "review.started"
    REVIEW_FINDING = "review.finding"
    REVIEW_DECISION = "review.decision"
    REVIEW_COMPLETED = "review.completed"
    # verification (5)
    VERIFICATION_PLAN_CREATED = "verification.plan_created"
    VERIFICATION_CHECK_STARTED = "verification.check_started"
    VERIFICATION_CHECK_COMPLETED = "verification.check_completed"
    VERIFICATION_FAILED = "verification.failed"
    VERIFICATION_EVIDENCE_RECORDED = "verification.evidence_recorded"
    # repair (3)
    REPAIR_HINT_CREATED = "repair.hint_created"
    REPAIR_CONTRACT_VALIDATION = "repair.contract_validation"
    REPAIR_SIGNAL_CONSUMED = "repair.signal_consumed"
    # failure_analysis (3)
    FAILURE_ANALYSIS_REQUESTED = "failure_analysis.requested"
    FAILURE_ANALYSIS_COMPLETED = "failure_analysis.completed"
    FAILURE_ANALYSIS_FAILED = "failure_analysis.failed"
    # context (21)
    CONTEXT_SNAPSHOT_CREATED = "context.snapshot_created"
    CONTEXT_COMPACTED = "context.compacted"
    CONTEXT_OBSERVATION_ADDED = "context.observation_added"
    CONTEXT_RENDERED_FOR_MODEL = "context.rendered_for_model"
    CONTEXT_ITEM_ADDED = "context.item_added"
    CONTEXT_ITEM_REDACTED = "context.item_redacted"
    CONTEXT_ITEM_PINNED = "context.item_pinned"
    CONTEXT_ITEM_UNPINNED = "context.item_unpinned"
    CONTEXT_ITEM_STALE = "context.item_stale"
    CONTEXT_ITEM_SUPERSEDED = "context.item_superseded"
    CONTEXT_BUNDLE_BUILT = "context.bundle_built"
    CONTEXT_BUNDLE_OVERFLOW = "context.bundle_overflow"
    CONTEXT_CACHE_USAGE_RECORDED = "context.cache_usage_recorded"
    CONTEXT_SNAPSHOT_SAVED = "context.snapshot_saved"
    CONTEXT_COMPACTION_REQUESTED = "context.compaction_requested"
    CONTEXT_COMPACTION_COMPLETED = "context.compaction_completed"
    CONTEXT_COMPACTION_FAILED = "context.compaction_failed"
    CONTEXT_REFERENCE_RESOLVED = "context.reference_resolved"
    CONTEXT_REFERENCE_STALE = "context.reference_stale"
    CONTEXT_RECOVERY_STARTED = "context.recovery_started"
    CONTEXT_RECOVERY_COMPLETED = "context.recovery_completed"
    # instruction (3)
    INSTRUCTION_SOURCES_COLLECTED = "instruction.sources.collected"
    INSTRUCTION_CONFLICT_DETECTED = "instruction.conflict.detected"
    INSTRUCTION_INJECTION_DETECTED = "instruction.injection_detected"
    # prompt (2)
    PROMPT_COMPILED = "prompt.compiled"
    PROMPT_MANIFEST_CREATED = "prompt.manifest.created"
    # project_index (5)
    PROJECT_INDEX_BUILD_STARTED = "project_index.build_started"
    PROJECT_INDEX_BUILD_COMPLETED = "project_index.build_completed"
    PROJECT_INDEX_BUILD_FAILED = "project_index.build_failed"
    PROJECT_INDEX_REFRESHED = "project_index.refreshed"
    PROJECT_INDEX_UPDATED = "project_index.updated"
    # session (6)
    SESSION_CREATED = "session.created"
    SESSION_CONTINUE_REQUESTED = "session.continue_requested"
    SESSION_RESUME_REQUESTED = "session.resume_requested"
    SESSION_RECOVERY_GATE_STARTED = "session.recovery_gate_started"
    SESSION_RECOVERY_GATE_COMPLETED = "session.recovery_gate_completed"
    SESSION_RECOVERY_BLOCKED = "session.recovery_blocked"
    # workspace (2)
    WORKSPACE_CHECKPOINT_CREATED = "workspace.checkpoint_created"
    WORKSPACE_CONFLICT_DETECTED = "workspace.conflict_detected"
    # kernel (3)
    KERNEL_BOOT_STARTED = "kernel.boot.started"
    KERNEL_BOOT_COMPLETED = "kernel.boot.completed"
    KERNEL_BOOT_FAILED = "kernel.boot.failed"
    # component (2)
    COMPONENT_INITIALIZED = "component.initialized"
    COMPONENT_HEALTH_CHECKED = "component.health_checked"
    # lifecycle (3)
    LIFECYCLE_RUN_STARTED = "lifecycle.run.started"
    LIFECYCLE_SESSION_STARTED = "lifecycle.session.started"
    LIFECYCLE_TASK_STARTED = "lifecycle.task.started"
    # cancellation (1)
    CANCELLATION_REQUESTED = "cancellation.requested"
    # shutdown (2)
    SHUTDOWN_STARTED = "shutdown.started"
    SHUTDOWN_COMPLETED = "shutdown.completed"
    # recovery (2)
    RECOVERY_DETECTED = "recovery.detected"
    RECOVERY_COMPLETED = "recovery.completed"
    # finalization (1)
    FINALIZATION_COMPLETED = "finalization.completed"
    # final_report (3)
    FINAL_REPORT_CREATED = "final_report.created"
    FINAL_REPORT_SECTION_ADDED = "final_report.section_added"
    FINAL_REPORT_COMPLETED = "final_report.completed"
```

### 数据流概述

`TraceEvent` 是最小单元，写入 `events.jsonl`；`TraceSpan` 是区间单元，写入 `spans.jsonl`；`TraceArtifact` 是文件引用，写入 `artifacts.jsonl` + 实际文件。`TraceStore.get_timeline()` 从 events 派生 `TraceTimelineItem`，`TraceStore.summarize()` 聚合为 `TraceSummary`。Policy audit 是独立层，由 `PolicyAuditWriter.append()` 写 `audit.jsonl`，不能用 trace event 替代。完整 trace 对象不自动进入模型；只有 `ContextManager.add_trace_summary()` 生成的安全文本摘要进入 context。

## 谁生成这些对象

`TraceRecorder.emit()`经redactor生成`TraceEvent`；SpanManager的start/end生成追加式`TraceSpan`；TraceArtifactStore写文件后生成`TraceArtifact`。`TraceTimelineBuilder`从events派生`TraceTimelineItem`，`TraceSummaryBuilder`从events/spans/artifacts聚合`TraceSummary`。

## 谁消费这些对象

TraceStore消费event/span/artifact；CLI、final report、evaluation/replay消费timeline/summary/artifact refs。完整trace对象不自动进入模型；只有`ContextManager.add_trace_summary()`生成的安全文本摘要进入context。

## 是否落盘

默认run目录`work/traces/runs/<run_id>/`包含`events.jsonl`、`spans.jsonl`、`artifacts.jsonl`、`index.json`和`artifacts/`文件。Timeline/Summary按需派生不独立落盘；其context投影写`context.sqlite3`。

## 是否进入 trace / audit

性能 span 只进入现有安全事件的数值 payload：sandbox lifecycle 的 `timing`、`review.completed` 的 `review_stage`/`decision`/`duration_ms`/`critic_duration_ms`/`model_critic_status`/`critic_reused`/`critic_skipped_reason`/`critic_reuse_skip_reason`/`critic_source_status`、`context.bundle_built` 的 `duration_ms`/`compaction_decision_duration_ms`，以及 `retrieval.query.completed` 的 `duration_ms`/`result_count`。这些字段不包含 prompt、response、memory query、credential、环境值或 evaluator-only metadata；evaluation 只按安全 ID、状态和耗时聚合。

TraceEvent在append前执行payload redaction并计算payload_hash；span/artifact通过refs关联。Policy audit是独立JSONL，由PolicyAuditWriter保存request/decision摘要，不能用events.jsonl替代审计账本，也不能把audit entry称为TraceEvent。

## 失败路径

非法run id抛`ValueError`，未知span抛`TraceStoreError`，artifact错误抛`TraceArtifactError`。`TraceRecorder.emit()`写失败降级返回`trace_write_failed` warning dict并输出脱敏stderr警告；业务执行继续，但final diagnostics应暴露trace不完整。

## 当前结构问题

events、spans、artifact index、timeline/summary与policy audit是不同层；新增event时必须定义payload来源、redaction、相关id和artifact refs，不能只在报告端猜测。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
