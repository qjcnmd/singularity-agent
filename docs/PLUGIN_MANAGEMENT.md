# Singularity 插件管理

Singularity 插件是本地项目或用户扩展。插件发现、manifest 校验、权限审批、启用状态和工具贡献由 `src/singularity/plugins/` 与 `src/singularity/tools/registry.py` 处理。

## 当前入口

```bash
singularity-agent plugin list --json
singularity-agent plugin status --json
```

插件启用/禁用能力也由当前 CLI 提供，具体命令以 `singularity-agent plugin --help` 为准。

## Manifest

插件 manifest 必须对应 `PluginManifest（插件清单）` 的当前字段。完整字段和消费者见 `docs/architecture/modules/plugin-tools-registry.md`，不要在本文件复制字段子集。

## 维护规则

- 不新增绕过 `PluginManager`、`ToolRegistry`、`PolicyEngine` 或 `ApprovalGate` 的插件入口。
- 不把旧插件命名或旧 manifest 读取方式作为默认路径。
- 修改插件 manifest 字段、工具贡献结构、状态文件或 lock 文件时，同步更新 `docs/architecture/modules/plugin-tools-registry.md` 并运行 `python scripts/verify_runtime_docs.py`。
