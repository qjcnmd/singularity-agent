# Singularity 插件管理

Singularity 插件是本地项目或用户扩展。插件发现、manifest 校验、权限审批、启用状态和工具贡献由 `src/singularity/plugins/` 与 `src/singularity/tools/registry.py` 处理。

## 当前状态

插件发现、manifest 校验和启用状态仍由 Python oracle/dev-only 模块维护；Rust public CLI 当前不暴露插件管理命令。需要把插件能力接入 public runtime 时，必须先在 Rust protocol/app-server/CLI 中定义当前对象和验证路径，不能恢复 Python CLI 作为默认入口。

## Manifest

插件 manifest 必须对应 `PluginManifest（插件清单）` 的当前字段。完整字段和消费者见 `docs/architecture/modules/plugin-tools-registry.md`，不要在本文件复制字段子集。

## 维护规则

- 不新增绕过 `PluginManager`、`ToolRegistry`、`PolicyEngine` 或 `ApprovalGate` 的插件入口。
- 不把旧插件命名或旧 manifest 读取方式作为默认路径。
- 修改插件 manifest 字段、工具贡献结构、状态文件或 lock 文件时，同步更新 `docs/architecture/modules/plugin-tools-registry.md` 并运行 `python scripts/verify_runtime_docs.py`。
