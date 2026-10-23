# Singularity

Windows 上的本地 Coding Agent。Rust 后端负责模型调用、工具执行和会话保存，Electron 桌面工作台负责操作和显示。

## 功能

- 对话中读写文件、运行命令，长命令可以转到后台继续跑
- 接入任意兼容 OpenAI Chat 或 Responses 协议的模型
- 支持 MCP 服务器、技能（`/技能名`）和项目指令（`AGENTS.md`）
- 上下文快满时自动压缩，完整历史一直保留在本地会话日志里
- 关闭窗口后任务在托盘里继续运行

## 快速开始

需要 Windows 10 / 11 x86-64 和 [Git for Windows](https://git-scm.com/install/windows)（命令工具用它的 Bash）。

1. 解压发布包，双击 `Singularity.exe`。
2. 在“设置 > 模型”里添加提供方、模型和 API Key。
3. 添加本机项目，新建任务，选好模型后发送消息。

配置文件、MCP、技能和数据目录见[安装与配置](docs/INSTALL.md)。

> Agent 继承 `Singularity.exe` 的本机权限，可以读写任意文件和运行命令；项目只用来提供上下文和分组，不限制访问范围。应用不监听网络端口，只连接你配置的模型和 MCP 服务。

## 文档

| 文档 | 内容 |
| --- | --- |
| [安装与配置](docs/INSTALL.md) | 安装、模型和 MCP 配置、项目指令与技能、数据目录 |
| [开发指南](docs/development.md) | 源码构建、调试、检查和打包 |
| [架构说明](docs/architecture.md) | 运行边界、模块职责、状态由谁维护、源码导航 |
| [架构图](architecture.html) | 完整运行流程、执行顺序与分支 |
| [工作台交互](docs/desktop-ui.md) | 各控件的操作方式 |
| [已确认的取舍](docs/decisions.md) | 讨论后决定保留的设计和接受的限制 |

## 许可证

[MIT License](LICENSE)
