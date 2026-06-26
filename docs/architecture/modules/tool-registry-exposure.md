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

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`AgentGraphBuilder._build_tools_protocol()` 创建 `ToolRegistry` -> 注册 mutation/edit/command/workspace/code-index/verification/plugin tools -> `AgentLoop` 调用 `tools.openai_tools()` 生成模型可见 schema。

## 真实对象完整结构

- `ToolSpec（工具规格）` 完整字段列在字段清单中，生成者是各 register_* 函数或 plugin manager。
- `ToolResult（工具结果）` 完整字段列在字段清单中，消费者是 `ToolExecutor`、tool protocol result envelope 和 context observation。

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
