# Tool Execution / Tool Protocol模块数据流

模块数据流文档 ID: tool-execution

源码证据路径:
- src/singularity/tool_protocol/models.py
- src/singularity/tool_protocol/engine.py
- src/singularity/tool_protocol/state.py
- src/singularity/tool_protocol/validator.py
- src/singularity/tool_protocol/scheduler.py
- src/singularity/tool_protocol/result.py
- src/singularity/tool_protocol/recovery.py
- src/singularity/tool_protocol/trace.py
- src/singularity/tools/executor.py

关键符号:
- ToolCallEnvelope
- ToolCallBatch
- ToolExecutionPlan
- ToolCallRecord
- ToolProtocolResultEnvelope
- ToolProtocolTurnResult
- ToolProtocolEngine
- ToolProtocolValidator
- ToolProtocolScheduler
- ToolProtocolResultBuilder
- ToolProtocolRecoveryManager

字段清单:
- ToolCallEnvelope: protocol_version, run_id, session_id, task_id, phase_id, model_request_id, model_response_id, assistant_message_id, tool_call_id, tool_name, raw_arguments, parsed_arguments, normalized_arguments, argument_digest, tool_schema_hash, allowed_tool_names, proposed_at, proposed_by_model, parse_status, validation_errors, metadata, phase
- ToolCallBatch: batch_id, run_id, session_id, task_id, phase_id, model_request_id, model_response_id, assistant_message, tool_calls, supports_parallel_execution, max_tool_calls, created_at, batch_digest
- ToolExecutionPlan: plan_id, batch_id, execution_mode, ordered_calls, parallel_groups, blocked_calls, reasons, requires_approval_count, side_effect_count
- ToolCallRecord: record_id, envelope, phase, previous_phase, policy_decision_id, approval_grant_id, execution_started_at, execution_finished_at, tool_result_digest, context_message_id, error_kind, error_message, attempts, created_at, updated_at
- ToolProtocolResultEnvelope: tool_call_id, tool_name, ok, status, error_code, error_kind, content_preview, content_digest, raw_result_ref, artifact_refs, observation_id, policy_decision_id, approval_grant_id, truncated, redacted, metadata
- ToolProtocolTurnResult: status, batch_id, executed_count, failed_count, rejected_count, pending_approval_count, appended_tool_message_count, next_action, recovery_report, metadata
- ToolProtocolValidationResult: valid, batch, errors, warnings, assistant_message_valid, protocol_version
- ToolProtocolRecoveryReport: pending_call_ids, running_call_ids, succeeded_but_not_appended_call_ids, assistant_message_ids_missing_tool_messages, recovered_call_ids, warnings, next_action
- ToolProtocolEvent: event_id, run_id, batch_id, tool_call_id, event_type, payload, created_at
- ToolProtocolResultBinding: binding_id, record_id, tool_call_id, result_id, result, raw_result_ref, context_message_id, result_digest, appended, created_at, metadata

## 这一层解决什么问题

工具执行层校验模型提出的 tool call，建立 batch、执行计划、状态记录、结果 envelope 和恢复报告，确保工具结果能正确回写模型上下文。

## 当前源码位置

- src/singularity/tool_protocol/models.py
- src/singularity/tool_protocol/engine.py
- src/singularity/tool_protocol/state.py
- src/singularity/tool_protocol/validator.py
- src/singularity/tool_protocol/scheduler.py
- src/singularity/tool_protocol/result.py
- src/singularity/tool_protocol/recovery.py
- src/singularity/tool_protocol/trace.py
- src/singularity/tools/executor.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`AgentLoop` 收到 `ModelTurnResult.tool_calls` -> `ToolProtocolValidator.validate_assistant_message()` -> `ToolProtocolScheduler.schedule()` -> `ToolProtocolEngine.process_model_turn()` -> `ToolExecutor.execute_request()` -> `ToolProtocolResultBuilder.build()` -> `ContextManager.add_tool_protocol_result()` -> 后续模型 turn。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`ToolProtocolEngine.process_model_turn()` -> `ToolProtocolValidator.validate_assistant_message()` 先把 `ModelTurnResult.tool_calls` 生成对象 `ToolCallEnvelope` 和 `ToolCallBatch`，再由 `ToolProtocolScheduler.schedule()` 生成 `ToolExecutionPlan`。`ToolExecutor.execute_request()` 消费 `ToolExecutionRequest` 并返回 `ToolResult`，`ToolProtocolResultBuilder.build()` 生成 `ToolProtocolResultEnvelope`；`ToolProtocolStateStore.upsert_record()`、`transition()`、`append_event()` 和 `bind_result()` 把 batch、record、event、binding 写入 `tool_protocol.sqlite3`。`ContextManager.add_tool_protocol_result()` 把安全 tool message 写入 `context.sqlite3`，raw result 只通过 artifact ref/digest 进入 trace。

## 真实对象完整结构

- `ToolCallEnvelope（工具调用信封）` 完整字段列在字段清单中，是模型 tool call 的规范化边界。
- `ToolProtocolResultEnvelope（工具协议结果信封）` 完整字段列在字段清单中，是进入 context/tool message 前的结果边界。

## 谁生成这些对象

- `ToolProtocolValidator.validate_assistant_message()` 从模型响应生成 `ToolCallEnvelope`、`ToolCallBatch` 和 `ToolProtocolValidationResult`；`ToolProtocolScheduler.schedule()` 根据 tool side effect/并行安全性生成 `ToolExecutionPlan`。
- `ToolProtocolStateStore.upsert_record()` / `transition()` 创建并更新 `ToolCallRecord`；`append_event()` 生成协议库内部的 `ToolProtocolEvent`。`ToolProtocolResultBuilder.build()` 或 engine 的 `_synthetic_result()` 生成 `ToolProtocolResultEnvelope`，`bind_result()` 生成 `ToolProtocolResultBinding`。
- `ToolProtocolEngine.process_model_turn()` 生成正常/拒绝/待批准的 `ToolProtocolTurnResult`；`ToolProtocolRecoveryManager` 从 records、bindings 与 context message 状态生成 `ToolProtocolRecoveryReport` 和恢复用 turn result。

## 谁消费这些对象

- scheduler、state store、engine 与 `ToolExecutor.execute_request()` 消费 envelope/batch/plan；这些对象来自模型响应，不回送为下一次模型请求。
- state binding 与 `ContextManager.add_tool_protocol_result()` 消费 `ToolProtocolResultEnvelope`。ContextManager 只把 redacted `ToolObservationView.to_model_payload()` 作为 tool message 加入下一轮模型请求，不发送 raw arguments、raw result 或完整协议元数据。
- `AgentLoop`/`RunController` 消费 `ToolProtocolTurnResult`；recovery/controller 消费 recovery report；state query/recovery 消费 `ToolProtocolEvent` 和 `ToolProtocolResultBinding`。validation/plan/turn/recovery 对象本身不进模型。

## 是否落盘

- `ToolProtocolStateStore` 位于当前 trace run 目录的 `tool_protocol.sqlite3`。`ToolCallBatch` 写 `tool_call_batches`；`ToolCallRecord` 写 `tool_call_records`；协议内部 event 写 `tool_protocol_events`；`ToolProtocolResultBinding` 与安全 result envelope 写 `tool_result_bindings`。
- `ToolCallEnvelope` 嵌在 batch/record 的安全 JSON 中；state 写入前剔除 raw result keys 并脱敏 raw arguments。`ToolExecutionPlan`、`ToolProtocolValidationResult`、`ToolProtocolTurnResult` 与 `ToolProtocolRecoveryReport` 不单独落盘。
- context 回写另写同一 run 的 `context.sqlite3` observations/items/messages；大结果正文由 `ToolProtocolResultBuilder._persist_raw()` 写 trace artifact，binding 只保存 `raw_result_ref`/digest。

## 是否进入 trace / audit

- `ToolProtocolTrace.emit()` 写 observability `events.jsonl` 的 `tool_protocol.batch_created`、`plan_built`、call state、`call_completed`、`result_bound` 等事件；payload 只保留 call id、tool name、digest、状态、计数和 artifact refs，明确删除 raw arguments/result/content。
- `ToolProtocolEvent` 是 `tool_protocol.sqlite3` 内部恢复日志，不是 `TraceRecorder` 的 `TraceEvent`；二者不可互换。协议 event 用于 replay/recovery，observability event 用于 timeline/report。
- planner/policy gate 产生的 request/decision 由 `ToolExecutor` 的 policy 链写 audit；protocol envelope、validation 与 recovery report 不直接进入 policy audit。

## 失败路径

- validator 对 invalid JSON、unknown/disallowed tool、schema mismatch、duplicate/missing call id 返回 invalid `ToolProtocolValidationResult`；engine 将错误转换成 rejected/synthetic result，而不是执行 handler。
- `ToolCallRecord.phase` 表达 waiting approval、running、succeeded、rejected、failed、cancelled；`ToolProtocolResultEnvelope` 用 `ok/status/error_code/error_kind` 保存执行失败，turn result 汇总 failed/rejected/pending approval 并给出 `next_action`。
- recovery report 明确列 pending、running、succeeded-but-not-appended、assistant message missing tool message；missing record/binding、conflicting replay 或无法安全 append 时保持阻塞，不伪造成功 tool message。

## 当前结构问题

协议状态 SQLite 与 observability trace 是两套持久化系统；修改 call phase、binding 或恢复规则时必须同时核对 state schema、`ToolExecutor.execute_request()`、`ContextManager.add_tool_protocol_result()` 的 append 原子性和 trace 安全投影。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
