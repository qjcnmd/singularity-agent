# Plugin Tools Registry模块数据流

模块数据流文档 ID: plugin-tools-registry

源码证据路径:
- src/singularity/plugins/models.py
- src/singularity/plugins/manager.py
- src/singularity/plugins/discovery.py
- src/singularity/plugins/loader.py
- src/singularity/plugins/permissions.py
- src/singularity/plugins/status.py
- src/singularity/plugins/host.py
- src/singularity/plugins/compatibility.py
- src/singularity/tools/registry.py
- src/singularity/model/tools.py

关键符号:
- PluginManifest
- DiscoveredPlugin
- PluginStatus
- PluginToolContribution
- PluginContributionSet
- PluginLoadResult
- PluginManager
- PluginHost
- ToolRegistry
- ModelToolRenderer

字段清单:
- CompatibilitySpec: min_singularity_version, max_singularity_version, min_python, max_python
- PluginManifest: id, name, version, api_version, entrypoint, type, capabilities, permissions, activation, compatibility, config_schema
- DiscoveredPlugin: manifest, manifest_path, plugin_dir, source, manifest_hash, diagnostics
- PluginDiagnostic: plugin_id, severity, code, message, path, details
- PluginStatus: enabled, version, path, manifest_hash, approved_permissions, config, compatibility_status
- PluginLockEntry: plugin_id, version, path, manifest_hash, compatibility_status, enabled
- PluginToolContribution: plugin_id, local_name, exposed_name, required_permissions, spec
- PluginContributionSet: plugin_id, tools, provider, prompt, memory, eval, project_adapter
- PluginLoadResult: plugin_id, loaded, contribution_set, diagnostics

## 这一层解决什么问题

Plugin 层发现、校验、启用插件 manifest，并把插件贡献的工具注入 ToolRegistry，同时保留权限、compatibility 和 lock/status 数据。

## 当前源码位置

- src/singularity/plugins/models.py
- src/singularity/plugins/manager.py
- src/singularity/plugins/discovery.py
- src/singularity/plugins/loader.py
- src/singularity/plugins/permissions.py
- src/singularity/plugins/status.py
- src/singularity/plugins/host.py
- src/singularity/plugins/compatibility.py
- src/singularity/tools/registry.py
- src/singularity/model/tools.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`AgentGraphBuilder._build_tools_protocol()` -> `PluginManager.activate()` -> discovery/loader/permission checks -> `PluginToolContribution.spec` 注册到 `ToolRegistry` -> 模型工具 schema 暴露。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 且插件提供专用工具为例：`discover_plugins()` -> `PluginStatusStore.enabled_for()` -> `PluginLoader.load()` -> `PluginManager.activate()` 先读取 manifest、status 和权限，生成对象 `DiscoveredPlugin`、`PluginStatus`、`PluginLockEntry`、`PluginLoadResult`。通过 policy gate 后，`PluginToolContribution.spec` 被 `ToolRegistry.register()` 消费并生成 `RegisteredToolRecord` / `ToolOrigin`；随后 `ModelToolRenderer.render()` 把 admitted tool 投影为 provider tool schema。插件状态写入 `.singularity/plugin-status.json`，锁写入 `.singularity/plugin-lock.json`，discover/activated/tool_registered/check_failed 写 trace 事件；失败 diagnostic 会阻止注册，而不是暴露半加载工具。

## 真实对象完整结构

### PluginManifest（插件清单）

插件的声明式描述，来自 `plugin.toml`。**边界**：落盘对象，原始输入是 project/user/plugin path 下的 TOML 文件；不进入模型请求。

```python
class PluginManifest(BaseModel):
    id: str
    name: str
    version: str
    api_version: str
    entrypoint: str
    type: PluginType
    capabilities: tuple[str, ...]
    permissions: tuple[PluginPermission, ...]
    activation: dict[str, Any]
    compatibility: CompatibilitySpec
    config_schema: dict[str, Any]
```

### PluginToolContribution（插件工具贡献）

插件注册到 ToolRegistry 的工具描述。**边界**：内部治理对象，不落盘；其 `spec` 被 `ToolRegistry.register()` 消费后生成 `RegisteredToolRecord`。

```python
class PluginToolContribution(BaseModel):
    plugin_id: str
    local_name: str
    exposed_name: str
    required_permissions: tuple[PluginPermission, ...]
    spec: ToolSpec = Field(exclude=True)
```

### PluginLoadResult（插件加载结果）

插件加载的成功/失败结果。**边界**：内部治理对象，不落盘；diagnostics 通过 CLI 和 trace event 保存。

```python
class PluginLoadResult(BaseModel):
    plugin_id: str
    loaded: bool
    contribution_set: PluginContributionSet | None = None
    diagnostics: list[PluginDiagnostic] = Field(default_factory=list)
```

### 关键枚举值域

```python
class PluginType(str, Enum):         # PluginManifest.type
    TOOL = "tool"
    PROVIDER = "provider"
    PROMPT = "prompt"
    MEMORY = "memory"
    EVAL = "eval"
    PROJECT_ADAPTER = "project_adapter"

class PluginPermission(str, Enum):   # PluginManifest.permissions
    READ_WORKSPACE = "read_workspace"
    READ_OUTSIDE_WORKSPACE = "read_outside_workspace"
    WRITE_WORKSPACE = "write_workspace"
    EXECUTE_COMMAND = "execute_command"
    NETWORK_ACCESS = "network_access"
    READ_ENV = "read_env"
    CHANGE_CONFIG = "change_config"

class PluginDiagnosticSeverity(str, Enum): # PluginDiagnostic.severity
    INFO = "info"
    WARNING = "warning"
    ERROR = "error"
```

### 数据流概述

`discover_plugins()` 扫描三类根目录读取 `PluginManifest`，`PluginStatusStore` 读取 `PluginStatus`，`_lock_entry()` 生成 `PluginLockEntry`。`PluginLoader.load()` 返回 `PluginLoadResult`，通过 policy gate 后 `PluginToolContribution.spec` 被 `ToolRegistry.register()` 消费。插件状态写 `.singularity/plugin-status.json`，锁写 `.singularity/plugin-lock.json`。`_policy_gate()` 构造 `proposed_by_model=False` 的 `PolicyRequest` 进入 policy audit ledger。

## 谁生成这些对象

- `CompatibilitySpec` 与 `PluginManifest` 由 Pydantic 在 `_read_manifest()` 调用 `PluginManifest.model_validate()` 时生成；manifest 无效时 `_invalid_manifest()` 用 `model_construct()` 生成仅供诊断继续传播的对象。
- `_read_manifest()` 组合 manifest、路径、source、hash 和 diagnostics 生成 `DiscoveredPlugin`；`discover_plugins()` 扫描 project、`SINGULARITY_PLUGIN_PATH` 与 user config 三类根目录，`_mark_duplicate_ids()` 追加 `PluginDiagnostic`。
- `PluginStatusStore` 读取/写入 `PluginStatus`，`_lock_entry()` 生成 `PluginLockEntry`。`PluginHost.register_tool()` 生成 `PluginToolContribution` 并加入 host 的 `PluginContributionSet`；`PluginLoader.load()` 返回成功或失败的 `PluginLoadResult`。

## 谁消费这些对象

- discovery/check/loader、`PluginManager._policy_gate()` 和 permission admission 消费 `PluginManifest`、`DiscoveredPlugin`、`PluginStatus` 与 diagnostics；`PluginManager.activate()` 消费 `PluginLoadResult` 和 `PluginContributionSet.tools`。
- `_admit_tool_contribution()` 校验 `PluginToolContribution` 的 identity、声明/批准权限、schema、risk tags 与 approval profile；通过后 `ToolRegistry.register()` 消费其 `spec`，并由 `_tool_origin()` 生成内部 provenance。
- plugin 对象不直接进入模型请求。只有 admitted `ToolSpec` 经 `ToolRegistry.openai_tools()` 投影出的 name、description、parameters schema 进入 provider；manifest、status、permissions、origin、diagnostics 与非 tool contribution 均保持内部数据。

## 是否落盘

- `PluginManifest` 原始输入是 project/user/plugin path 下的 `plugin.toml` 或 `singularity-plugin.toml`；`DiscoveredPlugin`、`PluginContributionSet`、`PluginLoadResult` 只在本次启动内存中存在。
- `PluginStatusStore` 将 `PluginStatus` 写入项目 `.singularity/plugin-status.json`；`PluginLockStore.write_entries()` 将本次处理后的 `PluginLockEntry` 写入 `.singularity/plugin-lock.json`。manager 只有处理 enabled plugin 或已有 lock 文件时才重写 lock。
- diagnostics 不建独立报告文件；它们通过 CLI check/list 输出和 trace event 保存。

## 是否进入 trace / audit

- `PluginManager.discover()` 发出 `plugin_discovered`，payload 是 `plugin_id`、`version`、`manifest_hash`、`source`；check/admission 失败发出 `plugin_check_failed`，payload 带 diagnostics。
- `PluginLoader.load()` 发出 `plugin_load_started`、`plugin_load_completed` 或 `plugin_load_failed`；成功 payload 带 tool count，失败 payload 来自 `PluginDiagnostic.to_dict()`。工具注册与激活分别发出 `plugin_tool_registered` 和 `plugin_activated`。
- `_policy_gate()` 构造 `proposed_by_model=False` 的 `PolicyRequest` 并调用 `PolicyEngine.enforce()`；该 request/decision 进入 policy audit ledger。其他 plugin 对象不直接写 audit。

## 失败路径

- TOML、Pydantic、I/O 或值错误转成 `manifest_invalid` diagnostic；重复 id 转成 `duplicate_plugin_id` / `duplicate_plugin_id_enabled`。status 与当前 path/hash 不一致产生 `plugin_status_mismatch`。
- loader 捕获 entrypoint 解析、import、callable 与 register 异常，返回 `PluginLoadResult(loaded=False)` 和 `plugin_load_failed` diagnostic，不把失败插件注册进 registry。
- policy exception/deny 分别产生 `plugin_policy_gate_failed` / `plugin_policy_denied`；tool admission 产生 identity、permission、schema、risk tag 或 high-risk gate diagnostic；`ToolRegistry.register()` 异常转成 `tool_registration_failed`。单插件失败不会回退到不受控加载。

## 当前结构问题

`PluginContributionSet` 预留 provider/prompt/memory/eval/project adapter 字段，但当前 `PluginManager.activate()` 只消费 `tools`；文档不得把这些非 tool 字段描述成已接入运行链。status/lock 属项目本地控制面，修改路径或原子写语义时应同步补充 verifier 覆盖。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
