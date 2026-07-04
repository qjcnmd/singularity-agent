# Context Compaction / Observation Store模块数据流

模块数据流文档 ID: context-compaction-observation-store

源码证据路径:
- src/singularity/context/models.py
- src/singularity/context/compaction.py
- src/singularity/context/store.py
- src/singularity/context/recovery.py

关键符号:
- ContextSnapshot
- ToolObservation
- RecoveredContext
- PartialCompactionRange

字段清单:
- ContextSnapshot: snapshot_id, run_id, session_id, task_id, goal, summary, retained_item_ids, known_observation_ids, version, created_at, retained_messages, metadata
- ToolObservation: id, tool_name, tool_call_id, ok, raw_result, preview, truncated, metadata, run_id, turn, created_at, input_tokens, preview_tokens, raw_digest, source_refs, cache_hit, duration_seconds, error_code, tool_version, truncation_reason, sensitivity
- RecoveredContext: run_id, messages, context_items, last_bundle, planner_state, pending_tool_calls, completed_tool_call_ids, pending_policy_approval, active_process_sessions, open_mutation_transactions, last_verification_status, last_safe_checkpoint, recommended_next_action, recovery_warnings, trace_last_event
- PartialCompactionRange: start_turn, end_turn, checkpoint_id

## 这一层解决什么问题

该层负责上下文快照、压缩摘要、工具观察和 crash recovery 所需的最近状态，防止长任务上下文无限增长。

## 当前源码位置

- src/singularity/context/models.py
- src/singularity/context/compaction.py
- src/singularity/context/store.py
- src/singularity/context/recovery.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`ContextManager` 写入 context store -> compaction 生成 `ContextSummaryEnvelope` / `ContextSnapshot` -> recovery 读取最近 bundle、工具状态和 planner 状态 -> `AgentLoop` 恢复下一步。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`ObservationStore.append_message()` / `save_observation()` -> `ContextCompactionPlanner.prepare()` 先读取 dialogue/tool observation 并生成对象 `CompactionPlan`。`ContextCompactionExecutor.summary_envelope_for_plan()` 生成 `ContextSummaryEnvelope`，`ContextCompactionCommitter.commit()` 调用 `ObservationStore.compact_items()` / `append_item()` 标记旧 item 并把 summary item 写入 `context.sqlite3`。恢复时 `ContextCompactionCommitter.recover_after_failure()` 和 context recovery 读取 `ContextSnapshot`、pending tool calls 与最后 bundle；失败会写入 failure payload，不伪造已压缩成功的 context。

## 真实对象完整结构

### ContextSnapshot（上下文快照）

compaction commit 成功后的上下文检查点。**边界**：落盘对象，写入 `context.sqlite3` 的 `context_snapshots` 表；不进入模型请求。

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
```

### ToolObservation（工具观察）

工具执行结果的 context 层记录。**边界**：落盘对象，写入 `context.sqlite3` 的 `observations` 表；其安全 tool message 投影进入下一轮模型请求。

```python
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
```

### RecoveredContext（恢复上下文）

crash recovery 重建的运行时状态。**边界**：内部治理对象，不落盘；其 messages/items 可进入后续模型请求。

```python
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
```

### 关键枚举值域

`ToolObservation.sensitivity` 使用 `ContextSensitivity` 枚举（与 context-assembly-prompt-frame 模块共享）：

```python
class ContextSensitivity(str, Enum):
    PUBLIC = "public"
    WORKSPACE = "workspace"
    SENSITIVE = "sensitive"
    SECRET = "secret"
```

### 数据流概述

`ContextManager.add_tool_result()` / `add_tool_protocol_result()` 生成 `ToolObservation`，写入 `context.sqlite3`。`add_tool_protocol_result()` 先把 `ToolProtocolResultEnvelope` 投影为 `ToolObservationView`，若投影已带 `observation_id`，则该值同时作为 `ToolObservation.id`、tool message payload 中的 `observation_id`、`ContextItem.item_id` 以及 artifact `ContextReference.source_item_id` / `observation_id`；若缺失才生成新的 observation id。`ContextCompactionPlanner.prepare()` 生成 `CompactionPlan`，utility score 使用 `context/ranking.py` 中与 assembler 共享的 layer/authority 权重 helper；`ContextCompactionExecutor.summary_envelope_for_plan()` 生成 `ContextSummaryEnvelope`，`ContextCompactionCommitter.commit()` 标记旧 item 并写 summary item。恢复时 `RecoveryManager.recover()` 从 SQLite、planner/protocol 状态和 trace 尾事件重建 `RecoveredContext`。observation 生成的安全 tool message 与 recovered messages/items 可进入后续模型请求；snapshot/recovered/range 本体不进入 provider。

## 谁生成这些对象

compaction commit 成功后生成 `ContextSnapshot`；`ContextManager.add_tool_result()` / `add_tool_protocol_result()` 生成 `ToolObservation`。协议结果路径的 observation id 来自 `ToolObservationView.observation_id` 或本层新生成的 id，生成后会同步写入 store item 与 source refs，不再让协议结果、模型可见 payload 和 context store 分别拥有不同身份。`RecoveryManager.recover()` 从 SQLite、planner/protocol 状态和 trace 尾事件重建 `RecoveredContext`。`PartialCompactionRange` 由 compaction 调用方显式给定，生产源码没有自动构造点。

## 谁消费这些对象

`ContextCompactionCommitter.recover_after_failure()` 和 recovery 消费 snapshot；`ContextManager.add_tool_result()`、`ContextAssembler.build_bundle()` 和 `Planner.update_from_tool_result()` 消费 tool observation。observation 生成的安全 tool message 与 recovered messages/items 可进入后续模型请求；snapshot/recovered/range 本体不进入 provider。

## 是否落盘

当前 trace run 的 `context.sqlite3` 在 `context_snapshots` 保存 snapshot，在 `observations` 保存 tool observation，并在 `messages` 保存模型可见 tool message；写入前移除 raw keys/脱敏。`RecoveredContext` 与 `PartialCompactionRange` 不落盘。

## 是否进入 trace / audit

ContextManager 记录 observation/snapshot/compaction 的 id、digest、token 与范围摘要；`ObservationStore.save_bundle()` 的 `context.bundle_built` 事件还记录 `duration_ms` 和 `compaction_decision_duration_ms` 数值，不记录 bundle 正文。raw tool result 留在 artifact/store，不进入 trace payload。恢复警告写 recovery event/diagnostics；本层不写 policy audit。

## 失败路径

并发版本不一致抛 `ContextVersionConflict`；SQLite/compaction/预算错误中止提交，旧 snapshot 仍有效。Recovery 通过 `recovery_warnings` 与 `recommended_next_action` 表达未决工具、approval、process/transaction；空或反向 `PartialCompactionRange` 抛 `ValueError`。

## 当前结构问题

observation、模型 tool message 与 raw artifact 是三种不同投影；恢复逻辑必须按 id/digest 连接，不能从 preview 推回 raw result。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
