# Tool Registry / Tool Exposure模块数据流

模块数据流文档 ID: tool-registry-exposure

源码证据路径:
- src/singularity/tools/models.py
- src/singularity/tools/registry.py
- src/singularity/tools/router.py
- src/singularity/model/tools.py

关键符号:
- ToolSpec
- ToolResult
- ToolExecutionRequest
- RegisteredToolRecord
- ToolOrigin
- ToolRegistry

字段清单:
- ToolSpec: name, version, description, input_model, output_model, handler, permission_level, risk_tags, timeout_seconds, max_output_chars, cacheable, idempotent, uses_edit_executor, uses_mutation_manager, uses_command_executor, delegates_policy_constraints, capabilities, operation, resource_resolver, side_effects, sensitivity, cache_policy, idempotency_policy, retry_policy, execution_backend, approval_profile, artifact_policy, streamable, enabled
- ToolResult: ok, content, error_code, error, truncated, metadata
- ToolExecutionRequest: tool_call_id, tool_name, raw_arguments, normalized_arguments, batch_id, run_id, session_id, task_id, phase_id, model_request_id, model_response_id, argument_digest, metadata
- RegisteredToolRecord: spec, origin, admitted, admission_reason, diagnostics, metadata
- ToolOrigin: kind, plugin_id, local_tool_name, exposed_name, manifest_hash, source_path, required_permissions, approved_permissions, activation_hash, schema_digest
- ToolCachePolicy: cacheable, ttl_seconds, max_entries
- ToolIdempotencyPolicy: idempotent, replay_returns_previous
- ToolRetryPolicy: max_attempts

## 这一层解决什么问题

工具注册层把内置工具和插件工具统一为 `ToolSpec（工具规格）`，再投影成 provider 可见的工具 schema。

## 当前源码位置

- src/singularity/tools/models.py
- src/singularity/tools/registry.py
- src/singularity/tools/router.py
- src/singularity/model/tools.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`AgentGraphBuilder._build_tools_protocol()` 创建 `ToolRegistry` -> 注册 mutation/edit/command/workspace/code-index/verification/plugin tools -> `AgentLoop` 调用 `tools.openai_tools()` 生成模型可见 schema。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`AgentGraphBuilder._build_tools_protocol()` -> `ToolRegistry.register()` -> `ToolRegistry.list_model_visible()` / `to_openai_tools()` -> `ModelToolRenderer.render()` 先注册内置 read/edit/command/verification/workspace 工具，为每个 `ToolSpec` 生成对象 `RegisteredToolRecord` 和 `ToolOrigin`，再把 admitted/enabled spec 投影成 provider tool schema。插件工具经过 `PluginManager.activate()` 后也以同一入口注册；registry 本体不写 sqlite/jsonl，执行阶段才由 tool protocol 写 `tool_protocol.sqlite3`。重复名、冻结后注册、非法 backend 或 schema 校验失败会抛错或产生 diagnostic，工具不会暴露给模型。
## 真实对象完整结构

- `ToolSpec（工具规格）` 完整字段列在字段清单中，生成者是各 register_* 函数或 plugin manager。
- `ToolResult（工具结果）` 完整字段列在字段清单中，消费者是 `ToolExecutor`、tool protocol result envelope 和 context observation。

## 谁生成这些对象

内置register函数或`PluginHost.register_tool()`生成含cache/idempotency/retry policy的`ToolSpec`；`ToolRegistry.register()`生成`ToolOrigin`/`RegisteredToolRecord`。provider tool call或`ToolExecutionRequest.from_envelope()`生成execution request；handler/executor/cache/policy/error分支用`ToolResult.success()`/`failure()`生成结果。

## 谁消费这些对象

registry/router/renderer/executor消费spec/record/request。provider只看到`ToolRegistry.openai_tools()`/`ModelToolRenderer`投影的name、description、input schema；origin、permission、risk、handler与policy对象不发送。ToolProtocol将安全`ToolResult`投影回下一轮模型。

## 是否落盘

registry/spec/origin/record/request均不落盘；ToolResult本体也无独立store。tool protocol只在`tool_protocol.sqlite3`保存result digest/ref与安全envelope，大正文写trace artifact，context message写`context.sqlite3`。

## 是否进入 trace / audit

registry/router/executor trace保存tool name、origin kind、admission/exposure reason、argument digest、status/error/output digest，不保存raw secret arguments/result。ToolExecutor构造的PolicyRequest/Decision进入policy audit，registry record本身不写audit。

## 失败路径

registry冻结后注册、重复名、非法write/shell backend或schema会抛错；执行时unknown tool、argument validation、planner/policy denial、approval required、timeout、handler/internal error均转为带`error_code/error`的`ToolResult.failure()`。

## 当前结构问题

内部ToolSpec明显宽于provider schema；插件provenance必须留在RegisteredToolRecord，不能为模型解释而加alias字段或把metadata暴露到parameters。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
