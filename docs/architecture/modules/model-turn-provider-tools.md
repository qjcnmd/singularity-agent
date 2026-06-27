# ModelTurn / Provider / Tools Exposure模块数据流

模块数据流文档 ID: model-turn-provider-tools

源码证据路径:
- src/singularity/model/models.py
- src/singularity/model/runner.py
- src/singularity/model/request_builder.py
- src/singularity/model/tools.py
- src/singularity/model/providers.py
- src/singularity/model/messages.py
- src/singularity/model/validation.py
- src/singularity/model/budget.py

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
- src/singularity/model/providers.py
- src/singularity/model/messages.py
- src/singularity/model/validation.py
- src/singularity/model/budget.py

## 关键类、函数、字段

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`AgentLoop.run()` -> `ModelRunner.build_request_from_context()` -> `PromptAssemblyPipeline` -> provider registry -> provider chat/completion -> `ModelRunner.run_turn()` -> `ModelTurnResult` -> tool protocol 或 finalization。

## 真实对象完整结构

- `ModelTurnRequest（模型单轮请求）` 完整字段列在字段清单中，生成者是 `ModelRunner`，消费者是 provider adapter 和 trace recorder。
- `ModelTurnResult（模型单轮结果）` 完整字段列在字段清单中，生成者是 `ModelRunner`，消费者是 `AgentLoop`、`ContextManager`、`ToolProtocolEngine`。

## 谁生成这些对象

context/prompt/message converter 生成 `ContentBlock`/`ModelMessage`；`ModelToolRenderer` 从 registry 生成 `ModelToolSchema`。request builder/调用方生成 choice、preferences、budget 与 `ModelTurnRequest`，provider adapter 提供 capabilities。
provider response parser/normalizer 生成 `ModelToolCall`/`ModelUsage`；validator 生成 `ModelValidationResult`，ModelRunner 的成功/invalid/failed 分支生成 `ModelTurnResult` 与 `ModelError`。

## 谁消费这些对象

ModelRunner/provider adapter 消费 request。provider payload只含安全 messages、tool name/description/parameters/strict、序列化 tool choice 与支持的 generation 参数；message/tool/request metadata、capability/risk、policy/trace metadata、budget 对象不发送。
AgentLoop 消费 validation/error/turn result，ToolProtocol 消费 tool calls，usage/reporting 消费 usage；这些 response 对象不自动进入下一轮模型，只有 ContextManager 追加的 assistant/tool message进入。

## 是否落盘

ModelTurnRequest/Result 没有独立 store；消息与 usage 投影写 `context.sqlite3`，raw provider request/response仅在配置允许时由 `ModelRunner._write_raw_artifact()` 写 redacted trace artifact。evaluation result聚合 token/cache/turn统计，不复制完整对象。

## 是否进入 trace / audit

ModelRunner 写 request-created、response-received、tool-call、output-rejected、request-failed events；payload保存 request/response ids、purpose、message/tool count、schema hash、usage、latency/error摘要和 artifact ref，不保存 message正文或 raw secrets。本层不写 policy audit。

## 失败路径

`ModelTurnStatus` 区分 invalid、failed、timeout、cancelled、budget_exceeded；provider auth/rate/network/timeout/invalid request映射为 `ModelError.kind/retryable`。validator对 tool call invalid JSON/schema/unknown/duplicate/max-count返回错误，AgentLoop再决定 retry、blocked或fatal。

## 当前结构问题

内部 `ModelTurnRequest` 比 provider JSON宽；维护时必须同时核对 `_model_messages_to_openai()`、`_model_tool_to_openai()` 和 tool-choice serialization，防止内部 metadata/provenance泄漏。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
