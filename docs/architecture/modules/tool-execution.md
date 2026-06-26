# Tool Execution / Tool Protocol模块数据流

模块数据流文档 ID: tool-execution

源码证据路径:
- src/singularity/tool_protocol/models.py
- src/singularity/tool_protocol/engine.py
- src/singularity/tool_protocol/state.py
- src/singularity/tools/executor.py

关键符号:
- ToolCallEnvelope
- ToolCallBatch
- ToolExecutionPlan
- ToolCallRecord
- ToolProtocolResultEnvelope
- ToolProtocolTurnResult
- ToolProtocolEngine

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
- src/singularity/tools/executor.py

## 关键类、函数、字段

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`AgentLoop` 收到 `ModelTurnResult.tool_calls` -> `ToolProtocolEngine.process_model_turn()` -> `ToolExecutor.execute()` -> `ToolProtocolResultEnvelope` -> `ContextManager.add_tool_observation()` -> 后续模型 turn。

## 真实对象完整结构

- `ToolCallEnvelope（工具调用信封）` 完整字段列在字段清单中，是模型 tool call 的规范化边界。
- `ToolProtocolResultEnvelope（工具协议结果信封）` 完整字段列在字段清单中，是进入 context/tool message 前的结果边界。

## 谁生成这些对象

这些对象由上文列出的源码组件在运行链路中生成。生成动作必须来自当前源码路径，不允许由文档、测试夹具或解释性包装层伪造。

## 谁消费这些对象

消费方是同一调用链后续组件、trace/audit 记录器、报告生成器或持久化 store。文档只列当前源码中真实调用的消费方。

## 是否落盘

落盘只通过当前源码中的 trace store、SQLite store、workspace state、evaluation output 或 manifest/report 写入路径发生。没有落盘代码的对象只在内存中传递。

## 是否进入 trace / audit

进入 trace / audit 的内容以 `TraceRecorder`、`JsonlTraceRecorder`、`TraceArtifactStore`、policy audit ledger 和相关 `record` / `emit` 调用为准。对象进入模型前必须经过当前工具协议、上下文组装和 redaction 逻辑。

## 失败路径

失败路径由当前源码中的异常、状态枚举、policy decision、verification result、planner outcome 和 result/report 字段表达。不得用旧 schema 或旧命名补充解释。

## 当前结构问题

当前结构仍大量使用字典 payload 连接组件，维护时最容易发生字段漂移。字段清单必须由源码校验脚本约束，不能只依赖人工描述。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
