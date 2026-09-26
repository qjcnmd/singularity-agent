# Singularity

Singularity 是以 Rust 实现的本地 coding-agent harness。通过 Electron 桌面应用启动工作台；`--json` 提供同一 Agent 能力的单次评估入口。Workspace、Session、模型配置和执行状态由本机 Host 统一管理，React 渲染进程负责呈现与控制。

当前发布目标为 Windows x86-64。

## 产品结构

```text
Singularity.exe（Electron：窗口、托盘、原生文件夹选择）
  └─ preload IPC → 主进程 → stdio → Rust AppServer
       └─ crates/runtime → AgentLoop → 工具与 Provider
resources/runtime/singularity.exe --json（单次评估，共用 runtime）
```

项目结构、状态归属与运行流程见 [Mermaid 架构图谱](docs/architecture.md)。

## 安装

发布包包含 Electron 桌面程序、React 资源和 Rust 后端，运行时无需单独安装 Node.js。请保留解压后的完整目录。命令工具需要 [Git for Windows](https://git-scm.com/install/windows) 提供的 Bash。

完整安装和源码构建说明见 [`docs/INSTALL.md`](docs/INSTALL.md)。

## 使用

双击发布目录中的 `Singularity.exe`。关闭窗口会隐藏到托盘，任务继续运行；点击托盘图标重新显示，从托盘菜单退出应用。

首次使用可在“设置 > 模型”中登记兼容 OpenAI Chat 或 Responses 协议的 Provider、模型与 API Key。API Key 是只写字段，页面与 Host 响应只显示脱敏配置状态。随后添加本机 Workspace、创建 Task，并在底部 Composer 的组合选择器中选择当前会话使用的模型与思考程度后提交工作。

运行中按 Enter 将输入排队，Ctrl/Cmd+Enter 插话；队列支持编辑、立即发送和撤回。会话日志保存历史，Host 维护执行状态，刷新或关闭页面不会停止后台任务。详细操作见 [工作台交互](docs/desktop-ui.md)。

评估入口：

```powershell
resources/runtime/singularity.exe --json "修复失败测试" --model provider/model#reasoning
```

`--json` 输出逐行事件并以终态 `summary` 行收尾，每次新建并保存一份会话。评估器通过 `SINGULARITY_HOME` 隔离数据，并负责超时和进程终止。`--app-server` 为桌面内部入口，不能与评估参数混用。

## 本地数据与权限

用户数据目录默认是 `%USERPROFILE%\.singularity`，保存模型配置、凭据、已登记项目与会话日志；设置 `SINGULARITY_HOME` 可改用别的绝对路径（例如让并行的第二个实例或评估任务使用独立数据）。取值无效时程序明确报错，不会把数据写到别的位置。目录内容和备份方式见 [数据、更新与卸载](docs/INSTALL.md#数据更新与卸载)。

工作台从打包资源加载，RPC 与事件走私有进程管道，不监听 HTTP 或 WebSocket 端口。渲染进程通过隔离的 preload 调用桌面能力。

Agent 继承 `singularity.exe` 的本机权限，可读取、编辑文件并运行命令。Workspace 用于项目上下文、Session 分组和文件候选，不是文件系统沙箱。

## 开发与维护

| 文档 | 用途 |
| --- | --- |
| [安装与运行](docs/INSTALL.md) | 发布包安装、源码构建、模型和技能配置、数据维护 |
| [工作台交互](docs/desktop-ui.md) | 项目、任务、输入、轨迹与显示约定 |
| [开发指南](docs/development.md) | 本地运行、相关检查、测试组织和发布流程 |
| [架构图谱](docs/architecture.md) | 模块关系、状态归属、运行流程、源码与改动影响导航 |
| [宪章](docs/constitution.md) | 产品方向与边界 |

发布包附带同目录的 `INSTALL.md`；上表按源码仓库的文档路径组织。

## 许可证

项目主体使用 [MIT License](LICENSE)。
