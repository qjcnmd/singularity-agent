# ModelTurn / Provider / Tools Exposure模块数据流

模块数据流文档 ID: model-turn-provider-tools

源码证据路径:
- src/singularity/model/models.py
- src/singularity/model/runner.py
- src/singularity/model/request_builder.py
- src/singularity/model/tools.py

关键符号:
- ModelTurnRequest
- ModelTurnResult
- ModelMessage
- ModelToolSchema
- ModelToolCall
- ModelRunner

字段清单:
- ContentBlock: type, text, artifact_ref, metadata
- ModelMessage: role, content, name, tool_call_id, metadata
- ModelToolSchema: name, description, parameters_schema, capability_tags, risk_tags, metadata
- ToolChoicePolicy: mode, tool_name, allowed_tool_names, max_tool_calls
- ModelToolCall: tool_call_id, tool_name, arguments, raw_arguments, parse_status, validation_errors, provider_metadata
- ModelCapabilities: supports_tools, supports_parallel_tool_calls, supports_streaming, supports_json_mode, supports_system_message, supports_developer_message, max_context_tokens, max_output_tokens, input_modalities, output_modalities
- ModelPreferences: provider_name, model_name, temperature, top_p, max_output_tokens, json_mode, stream, fallback_models
- ModelBudget: max_input_tokens, max_output_tokens, max_total_tokens, max_retries, max_latency_ms, max_cost_estimate
- ModelUsage: input_tokens, output_tokens, total_tokens, cached_input_tokens, reasoning_tokens, cost_estimate
- ModelTurnRequest: request_id, run_id, session_id, task_id, phase_id, action_id, purpose, messages, tools, tool_choice, model_preferences, budget, context_metadata, policy_metadata, trace_metadata
- ModelValidationResult: valid, errors, warnings, repaired, repair_message
- ModelError: kind, message, retryable, provider_name, model_name, raw_error_ref, metadata
- ModelTurnResult: request_id, response_id, status, assistant_message, tool_calls, usage, finish_reason, validation, error, provider_name, model_name, latency_ms, trace_event_ids, raw_response_ref, metadata

## 这一层解决什么问题

模型层把上下文、工具 schema、tool choice、budget 和 provider 偏好组装为 `ModelTurnRequest（模型单轮请求）`，再把 provider 响应解析为 `ModelTurnResult（模型单轮结果）`。

## 当前源码位置

- src/singularity/model/models.py
- src/singularity/model/runner.py
- src/singularity/model/request_builder.py
- src/singularity/model/tools.py

## 关键类、函数、字段

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`AgentLoop.run()` -> `ModelRunner.build_request_from_context()` -> `PromptAssemblyPipeline` -> provider registry -> provider chat/completion -> `ModelRunner.run_turn()` -> `ModelTurnResult` -> tool protocol 或 finalization。

## 真实对象完整结构

- `ModelTurnRequest（模型单轮请求）` 完整字段列在字段清单中，生成者是 `ModelRunner`，消费者是 provider adapter 和 trace recorder。
- `ModelTurnResult（模型单轮结果）` 完整字段列在字段清单中，生成者是 `ModelRunner`，消费者是 `AgentLoop`、`ContextManager`、`ToolProtocolEngine`。

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
