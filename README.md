## 快速开始

需要 Windows 10 / 11 x86-64 和 [Git for Windows](https://git-scm.com/install/windows)（命令工具用它的 Bash）。

1. 解压发布包，双击 `Singularity.exe`。
2. 在“设置 > 模型”里添加提供方、模型和 API Key。
3. 添加本机项目，新建任务，选好模型后发送消息。

配置文件、MCP、技能和数据目录见[安装与配置](docs/INSTALL.md)。

> Agent 继承 `Singularity.exe` 的本机权限，可以读写任意文件和运行命令；项目只用来提供上下文和分组，不限制访问范围。应用不监听网络端口，只连接你配置的模型和 MCP 服务。

## 仓库结构

Rust 后端采用单向依赖 `app → agent → {model, mcp} → {app_core, protocol}`，桌面前端位于 `apps/desktop/`。完整文件清单与符号入口见[源码索引](directory.html)（单一真相源）。

| 路径 | 职责 |
| --- | --- |
| `crates/protocol/` | 前后端共享 JSON-RPC 契约、事件/DTO 与 `examples/export_types.rs` TypeScript 导出器 |
| `crates/core/` (`app_core`) | 产品身份常量、数据根目录定位、工作区路径规范化/重叠判断、分层 `AGENTS.md` 与 `SKILL.md` 加载、原子写盘与 Token 估算 |
| `crates/model/` | `config.json` 读写脱敏、独立联网工具客户端（`web.rs`）、协议无关 `ModelClient`、SSE 切帧与 Messages / Responses 双协议编解码 |
| `crates/mcp/` | `mcp.json` 校验与多路径凭据脱敏、按 `(directory, server)` 复用的 `McpManager` 连接池与 rmcp 客户端 |
| `crates/agent/` | Agent 主循环、模块化上下文组装、并发工具调度、JSONL v1 会话日志、Shadow Git 每回合快照与回滚引擎、分段压缩与内置工具集（含 `bash`/`read`/`edit`/`write`/`view_image`/`web_*`/`skill`/`ask_user_question`/`mcp_*`） |
| `crates/app/` | `Singularity.exe` 入口、滚动诊断日志、stdio JSON-RPC 传输与 `StreamHub` 事件中心、`AppServer` 会话与工作区协调、`Conversation` 状态机与后端黑盒不变量测试 |
| `apps/desktop/` | Electron 主进程/预加载桥接、React 桌面工作台界面与 Playwright 端到端冒烟套件 |

## 文档

| 文档 | 内容 |
| --- | --- |
| [安装与配置](docs/INSTALL.md) | 安装、模型和 MCP 配置、项目指令与技能、数据目录 |
| [开发指南](docs/development.md) | 源码构建、调试、检查和打包 |
| [架构说明](docs/architecture.md) | 运行边界、模块职责、状态由谁维护、源码导航 |
| [源码索引](directory.html) | 完整目录树、文件职责与公开符号入口 |
| [架构图](architecture.html) | 完整运行流程、执行顺序与分支 |
| [已确认的取舍](docs/decisions.md) | 讨论后决定保留的设计和接受的限制 |

## 许可证

[MIT License](LICENSE)
