# Singularity

Singularity 是用 Rust 实现的本地 Coding Agent，通过 Electron 桌面工作台使用。项目、任务、模型配置和执行状态由本机的 Rust 后端管理，React 界面负责显示和操作。

当前发布目标为 Windows x86-64。

## 产品结构

```text
Singularity.exe（Electron：窗口、托盘、原生文件夹选择）
  └─ preload IPC → 主进程 → stdio → Rust AppServer
       └─ app::runtime → Agent 循环 → 工具与模型提供方
```

完整运行流程见 [HTML 架构图](architecture.html)；模块职责、状态归属与源码入口见[架构说明](docs/architecture.md)。

## 安装

发布包包含 Electron 桌面程序、React 资源和 Rust 后端，运行时无需单独安装 Node.js。请保留解压后的完整目录。命令工具需要 [Git for Windows](https://git-scm.com/install/windows) 提供的 Bash。

完整安装和源码构建说明见 [`docs/INSTALL.md`](docs/INSTALL.md)。

## 使用

双击发布目录中的 `Singularity.exe`。关闭窗口会隐藏到托盘，任务继续运行；点击托盘图标重新显示，从托盘菜单退出应用。

首次使用时，在“设置 > 模型”中添加兼容 OpenAI Chat 或 Responses 协议的提供方、模型和 API Key。API Key 只能写入，界面只显示是否已配置。然后添加本机项目、新建任务，在底部输入框的选择器中选好模型和思考程度，发送消息开始工作。

空闲时发送消息会开始新的一轮；运行中发送的消息会排队，在下一次模型请求前交给模型。历史保存在会话日志里，执行状态由后端维护，刷新或关闭窗口不会停止正在运行的任务。详细操作见[工作台交互](docs/desktop-ui.md)。

## 本地数据与权限

用户数据目录默认是 `%USERPROFILE%\.singularity`，保存模型配置、凭据、已登记项目与会话日志；设置 `SINGULARITY_HOME` 可改用别的绝对路径（例如让并行的第二个实例或自动化任务使用独立数据）。取值无效时程序明确报错，不会把数据写到别的位置。目录内容和备份方式见 [数据存储与目录维护](docs/INSTALL.md#6-数据存储与目录维护)。

工作台从打包资源加载，RPC 与事件走私有进程管道，不监听 HTTP 或 WebSocket 端口。渲染进程通过隔离的 preload 调用桌面能力。

Agent 继承 `singularity.exe` 的本机权限，可以读写文件和运行命令。项目只用于提供上下文、给任务分组和补全文件名，不限制文件访问范围。

## 开发与维护

| 文档 | 用途 |
| --- | --- |
| [安装与运行](docs/INSTALL.md) | 发布包安装、源码构建、模型和技能配置、数据维护 |
| [工作台交互](docs/desktop-ui.md) | 项目、任务、输入、注入内容与显示约定 |
| [开发指南](docs/development.md) | 本地运行、检查和发布流程 |
| [架构图](architecture.html) | 完整运行流程、执行顺序与条件分支 |
| [架构说明](docs/architecture.md) | 模块职责、状态由谁维护与源码导航 |
| [宪章](docs/constitution.md) | 产品方向与边界 |

## 许可证

项目主体使用 [MIT License](LICENSE)。
