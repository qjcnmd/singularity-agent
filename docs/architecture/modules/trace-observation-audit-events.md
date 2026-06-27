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
    event_type: TraceEventType       # 174 个成员的枚举
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
class TraceSeverity(str, Enum):    # TraceEvent.severity
    DEBUG = "debug"
    INFO = "info"
    WARNING = "warning"
    ERROR = "error"
    CRITICAL = "critical"

class TraceStatus(str, Enum):      # TraceSpan.status
    RUNNING = "running"
    SUCCESS = "success"
    FAILED = "failed"
    CANCELLED = "cancelled"
    TIMEOUT = "timeout"
    SKIPPED = "skipped"
    BLOCKED = "blocked"

class TraceArtifactKind(str, Enum): # TraceArtifact.kind
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

`TraceEventType` 有 174 个成员，按组件分组的关键子集：`task.started`、`task.completed`、`phase.started`、`phase.completed`、`action.started`、`action.completed`、`model.request_started`、`model.request_completed`、`model.tool_calls_proposed`、`tool.started`、`tool.completed`、`tool.rejected`、`policy.requested`、`policy.decided`、`approval.requested`、`approval.granted`、`command.started`、`command.completed`、`sandbox.started`、`sandbox.completed`、`verification.plan_built`、`verification.check_started`、`verification.check_completed`、`repair.analysis_completed`、`context.item_added`、`context.bundle_built`、`context.rendered_for_model`、`instruction_sources_collected`、`prompt_compiled`、`kernel.boot_completed`、`kernel.task_started`、`kernel.finalization_completed`。

### 数据流概述

`TraceEvent` 是最小单元，写入 `events.jsonl`；`TraceSpan` 是区间单元，写入 `spans.jsonl`；`TraceArtifact` 是文件引用，写入 `artifacts.jsonl` + 实际文件。`TraceStore.get_timeline()` 从 events 派生 `TraceTimelineItem`，`TraceStore.summarize()` 聚合为 `TraceSummary`。Policy audit 是独立层，由 `PolicyAuditWriter.append()` 写 `audit.jsonl`，不能用 trace event 替代。完整 trace 对象不自动进入模型；只有 `ContextManager.add_trace_summary()` 生成的安全文本摘要进入 context。

## 谁生成这些对象

`TraceRecorder.emit()`经redactor生成`TraceEvent`；SpanManager的start/end生成追加式`TraceSpan`；TraceArtifactStore写文件后生成`TraceArtifact`。`TraceTimelineBuilder`从events派生`TraceTimelineItem`，`TraceSummaryBuilder`从events/spans/artifacts聚合`TraceSummary`。

## 谁消费这些对象

TraceStore消费event/span/artifact；CLI、final report、evaluation/replay消费timeline/summary/artifact refs。完整trace对象不自动进入模型；只有`ContextManager.add_trace_summary()`生成的安全文本摘要进入context。

## 是否落盘

默认run目录`work/traces/runs/<run_id>/`包含`events.jsonl`、`spans.jsonl`、`artifacts.jsonl`、`index.json`和`artifacts/`文件。Timeline/Summary按需派生不独立落盘；其context投影写`context.sqlite3`。

## 是否进入 trace / audit

TraceEvent在append前执行payload redaction并计算payload_hash；span/artifact通过refs关联。Policy audit是独立JSONL，由PolicyAuditWriter保存request/decision摘要，不能用events.jsonl替代审计账本，也不能把audit entry称为TraceEvent。

## 失败路径

非法run id抛`ValueError`，未知span抛`TraceStoreError`，artifact错误抛`TraceArtifactError`。`TraceRecorder.emit()`写失败降级返回`trace_write_failed` warning dict并输出脱敏stderr警告；业务执行继续，但final diagnostics应暴露trace不完整。

## 当前结构问题

events、spans、artifact index、timeline/summary与policy audit是不同层；新增event时必须定义payload来源、redaction、相关id和artifact refs，不能只在报告端猜测。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
