# Tool Registry / Tool Exposure模块数据流

模块数据流文档 ID: tool-registry-exposure

源码证据路径:
- src/singularity/tools/models.py
- src/singularity/tools/registry.py
- src/singularity/tools/router.py
- src/singularity/model/tools.py
- src/singularity/model/openai_format.py

关键符号:
- ToolSpec
- ToolResult
- ToolExecutionRequest
- RegisteredToolRecord
- ToolOrigin
- ToolRegistry
- tool_schema_to_openai

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

工具注册层把内置工具和插件工具统一为 `ToolSpec（工具规格）`，先导出 provider-neutral 工具 schema，再由模型/provider 格式层投影成当前 OpenAI-compatible provider 可见的工具 schema。

## 当前源码位置

- src/singularity/tools/models.py
- src/singularity/tools/registry.py
- src/singularity/tools/router.py
- src/singularity/model/tools.py
- src/singularity/model/openai_format.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`AgentGraphBuilder._build_tools_protocol()` 创建 `ToolRegistry` -> 注册 mutation/edit/command/workspace/code-index/verification/plugin tools -> `AgentLoop` 调用 `tools.openai_tools()` 生成模型可见 OpenAI-compatible schema。`ToolRegistry.schema_export()` 是 provider-neutral 导出，不是 provider payload。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`AgentGraphBuilder._build_tools_protocol()` -> `ToolRegistry.register()` -> `ToolRegistry.list_model_visible()` / `schema_export()` / `to_openai_tools()` -> `ModelToolRenderer.render()` 先注册内置 read/edit/command/verification/workspace 工具，为每个 `ToolSpec` 生成对象 `RegisteredToolRecord` 和 `ToolOrigin`。`schema_export()` 只导出 admitted/enabled 工具的中立描述：name、version、description、input_schema、permission_level、side_effects、capabilities、operation、cache_policy、idempotency_policy、retry_policy、execution_backend、origin；不包含 OpenAI 顶层 `type="function"`、`function` 或 `parameters` 包装，也不包含 handler、resource_resolver、record metadata 或 policy 对象。当前 OpenAI-compatible 入口 `to_openai_tools()` 读取中立 schema 后调用 `tool_schema_to_openai()`，只把 name、description、input_schema 投影为 provider tool schema。插件工具经过 `PluginManager.activate()` 后也以同一入口注册；registry 本体不写 sqlite/jsonl，执行阶段才由 tool protocol 写 `tool_protocol.sqlite3`。重复名、冻结后注册、非法 backend 或 schema 校验失败会抛错或产生 diagnostic，工具不会暴露给模型。
## 真实对象完整结构

### ToolSpec（工具规格）

内置/插件工具的完整注册描述。**边界**：内部治理对象，不落盘、不进入模型；只有 `ModelToolRenderer.render()` 投影的 name/description/parameters_schema 进入 provider。Pydantic BaseModel，`input_model`/`handler`/`resource_resolver` 排除序列化。

```python
class ToolSpec(BaseModel):
    name: str
    version: str = "0.0.1"
    description: str
    input_model: type[BaseModel]           # exclude
    output_model: type[BaseModel] | None = None  # exclude
    handler: Callable[[Any], Any]          # exclude
    permission_level: PermissionLevel = PermissionLevel.READ_ONLY
    risk_tags: tuple[str, ...] = ()
    timeout_seconds: float = 5.0
    max_output_chars: int = 20000
    cacheable: bool = False
    idempotent: bool = True
    uses_edit_executor: bool = False
    uses_mutation_manager: bool = False
    uses_command_executor: bool = False
    delegates_policy_constraints: bool = False
    capabilities: tuple[Capability, ...] = ()
    operation: OperationKind | None = None
    resource_resolver: Callable | None = None   # exclude
    side_effects: ToolSideEffectKind | None = None
    sensitivity: ToolSensitivityLevel = ToolSensitivityLevel.WORKSPACE
    cache_policy: ToolCachePolicy | None = None
    idempotency_policy: ToolIdempotencyPolicy | None = None
    retry_policy: ToolRetryPolicy = Field(default_factory=ToolRetryPolicy)
    execution_backend: ToolExecutionBackendKind = ToolExecutionBackendKind.IN_PROCESS
    approval_profile: dict[str, Any] = Field(default_factory=dict)
    artifact_policy: dict[str, Any] = Field(default_factory=dict)
    streamable: bool = False
    enabled: bool = True
```

### ToolOrigin（工具来源）

记录工具的注册来源和权限。**边界**：内部治理对象，不进入模型；投影进 trace event 和 plugin lock。

```python
class ToolOrigin(BaseModel):
    kind: ToolOriginKind = ToolOriginKind.BUILTIN
    plugin_id: str | None = None
    local_tool_name: str | None = None
    exposed_name: str | None = None
    manifest_hash: str | None = None
    source_path: str | None = None
    required_permissions: tuple[str, ...] = ()
    approved_permissions: tuple[str, ...] = ()
    activation_hash: str | None = None
    schema_digest: str | None = None
```

### ToolResult（工具结果）

handler 返回值的结构化载体。**边界**：内部治理对象，投影为 `ToolProtocolResultEnvelope` 和 context tool message；不独立落盘。

```python
class ToolResult(BaseModel):
    ok: bool
    content: Any | None = None
    error_code: str | None = None
    error: ToolError | None = None
    truncated: bool = False
    metadata: dict[str, Any] = Field(default_factory=dict)
```

### 关键枚举值域

```python
class PermissionLevel(str, Enum):    # ToolSpec.permission_level
    READ_ONLY = "read_only"
    WRITE = "write"
    SHELL = "shell"
    GIT = "git"

class ToolSideEffectKind(str, Enum): # ToolSpec.side_effects
    NONE = "none"
    READ_WORKSPACE = "read_workspace"
    MUTATE_WORKSPACE = "mutate_workspace"
    EXECUTE_COMMAND = "execute_command"
    NETWORK = "network"

class ToolSensitivityLevel(str, Enum): # ToolSpec.sensitivity
    PUBLIC = "public"
    WORKSPACE = "workspace"
    SENSITIVE = "sensitive"
    SECRET = "secret"

class ToolExecutionBackendKind(str, Enum): # ToolSpec.execution_backend
    IN_PROCESS = "in_process"
    DELEGATED_MUTATION_MANAGER = "delegated_mutation_manager"
    DELEGATED_EDIT_EXECUTOR = "delegated_edit_executor"
    DELEGATED_COMMAND_EXECUTOR = "delegated_command_executor"
    DELEGATED_VERIFICATION_RUNNER = "delegated_verification_runner"
    EXTERNAL_PROCESS = "external_process"

class ToolOriginKind(str, Enum):     # ToolOrigin.kind
    BUILTIN = "builtin"
    PLUGIN = "plugin"
    FUTURE_MCP = "future_mcp"
```

### 数据流概述

各 `register_*` 函数或 `PluginHost.register_tool()` 生成 `ToolSpec`，`ToolRegistry.register()` 生成 `ToolOrigin` 和 `RegisteredToolRecord`。`ToolRegistry.schema_export()` 导出 provider-neutral schema，保留 permission、side_effect、capabilities、operation、cache/idempotency/retry policy、execution_backend 和 origin 供内部控制面/契约检查使用，但不作为模型请求直接发送。`ToolRegistry.openai_tools()` 通过 `tool_schema_to_openai()` 只把 name、description、input_schema 投影为 OpenAI-compatible `type/function/parameters/strict`；`ModelToolRenderer.render()` 只投影 name、description、parameters_schema 进入 `ModelToolSchema`。handler、resource_resolver、record metadata 和 policy 对象不会进入 provider。执行阶段 `ToolExecutionRequest.from_envelope()` 从 `ToolCallEnvelope` 生成 request，handler 返回 `ToolResult`，再投影为 `ToolProtocolResultEnvelope`。

## 谁生成这些对象

内置register函数或`PluginHost.register_tool()`生成含cache/idempotency/retry policy的`ToolSpec`；`ToolRegistry.register()`生成`ToolOrigin`/`RegisteredToolRecord`。provider tool call或`ToolExecutionRequest.from_envelope()`生成execution request；handler/executor/cache/policy/error分支用`ToolResult.success()`/`failure()`生成结果。

## 谁消费这些对象

registry/router/renderer/executor消费spec/record/request。provider只看到`ToolRegistry.openai_tools()`/`ModelToolRenderer`经模型格式层投影的name、description、input schema和strict标志；`schema_export()`中的origin、permission、capabilities、backend、cache/idempotency/retry policy、handler、resource_resolver与policy对象不发送。ToolProtocol将安全`ToolResult`投影回下一轮模型。

## 是否落盘

registry/spec/origin/record/request均不落盘；ToolResult本体也无独立store。tool protocol只在`tool_protocol.sqlite3`保存result digest/ref与安全envelope，大正文写trace artifact，context message写`context.sqlite3`。

## 是否进入 trace / audit

registry/router/executor trace保存tool name、origin kind、admission/exposure reason、argument digest、status/error/output digest，不保存raw secret arguments/result。ToolExecutor构造的PolicyRequest/Decision进入policy audit，registry record本身不写audit。

## 失败路径

registry冻结后注册、重复名、非法write/shell backend或schema会抛错；执行时unknown tool、argument validation、planner/policy denial、approval required、timeout、handler/internal error均转为带`error_code/error`的`ToolResult.failure()`。

## 当前结构问题

内部ToolSpec和`schema_export()`明显宽于provider schema；插件provenance必须留在RegisteredToolRecord或中立控制面导出，不能为模型解释而加alias字段或把metadata暴露到provider parameters。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
