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

- `ContextSnapshot（上下文快照）` 完整字段列在字段清单中，落盘到 context store。
- `ToolObservation（工具观察）` 完整字段列在字段清单中，消费者是 context 渲染、planner 证据和 failure analysis。

## 谁生成这些对象

compaction commit 成功后生成 `ContextSnapshot`；`ContextManager.add_tool_result()` / `add_tool_protocol_result()` 生成 `ToolObservation`。`RecoveryManager.recover()` 从 SQLite、planner/protocol 状态和 trace 尾事件重建 `RecoveredContext`。`PartialCompactionRange` 由 compaction 调用方显式给定，生产源码没有自动构造点。

## 谁消费这些对象

recovery/compaction 消费 snapshot；ContextManager、assembler、planner/failure analysis 消费 tool observation。observation 生成的安全 tool message 与 recovered messages/items 可进入后续模型请求；snapshot/recovered/range 本体不进入 provider。

## 是否落盘

当前 trace run 的 `context.sqlite3` 在 `context_snapshots` 保存 snapshot，在 `observations` 保存 tool observation，并在 `messages` 保存模型可见 tool message；写入前移除 raw keys/脱敏。`RecoveredContext` 与 `PartialCompactionRange` 不落盘。

## 是否进入 trace / audit

ContextManager 记录 observation/snapshot/compaction 的 id、digest、token 与范围摘要；raw tool result 留在 artifact/store，不进入 trace payload。恢复警告写 recovery event/diagnostics；本层不写 policy audit。

## 失败路径

并发版本不一致抛 `ContextVersionConflict`；SQLite/compaction/预算错误中止提交，旧 snapshot 仍有效。Recovery 通过 `recovery_warnings` 与 `recommended_next_action` 表达未决工具、approval、process/transaction；空或反向 `PartialCompactionRange` 抛 `ValueError`。

## 当前结构问题

observation、模型 tool message 与 raw artifact 是三种不同投影；恢复逻辑必须按 id/digest 连接，不能从 preview 推回 raw result。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
