## 快速开始

需要 Windows 10 / 11 x86-64 和 [Git for Windows](https://git-scm.com/install/windows)（命令工具用它的 Bash）。

1. 解压发布包，双击 `Singularity.exe`。
2. 在“设置 > 模型”里添加提供方、模型和 API Key。
3. 添加本机项目，新建任务，选好模型后发送消息。

配置文件、MCP、技能和数据目录见[安装与配置](docs/INSTALL.md)。

> Agent 继承 `Singularity.exe` 的本机权限，可以读写任意文件和运行命令；项目只用来提供上下文和分组，不限制访问范围。应用不监听网络端口，只连接你配置的模型和 MCP 服务。

## crates/ 结构

单向依赖 `app → agent → {model, mcp, core, protocol}`。

```
crates/
├── protocol/                    # RPC 契约：前后端共享的类型定义
│   └── src/
│       ├── lib.rs  rpc.rs  params.rs       # 方法分发与参数类型
│       ├── app.rs                          # 会话/工作台 DTO
│       ├── event.rs                        # 推送到前端的事件
│       ├── background.rs  question.rs      # 后台任务、提问 DTO
│       ├── image.rs  mcp.rs  inspection.rs # 图片、MCP、上下文检查
│       ├── typescript.rs                   # TS 类型导出器
│       └── examples/export_types.rs        # → protocol.generated.ts
│
├── core/                        # 基础设施：数据目录与项目资源
│   └── src/
│       ├── lib.rs                          # write_json_atomic 原子写
│       ├── user_home.rs                    # ~/.singularity 定位
│       ├── project_instructions.rs         # AGENTS.md 加载
│       ├── skills.rs                       # SKILL.md 发现与全文读取
│       └── workspace.rs                    # 项目目录规范化
│
├── model/                       # 模型配置与请求
│   └── src/
│       ├── config/
│       │   ├── manager.rs                  # config.json 读改写（隐藏密钥）
│       │   ├── schema.rs  selection.rs     # 配置结构、ModelSelection
│       │   ├── discovery.rs                # 模型列表查询
│       │   └── user/mod.rs                 # 文件读写入口
│       ├── types/                          # Message/Tool/Request/Response/reasoning
│       ├── openai/
│       │   ├── chat.rs + chat/stream.rs    # Chat Completions 编解码
│       │   ├── responses.rs + responses/stream.rs  # Responses 编解码
│       │   ├── provider.rs  parse.rs  wire.rs      # 端点、错误分类、续接
│       ├── provider/contract.rs            # Provider 抽象
│       └── transport/                      # HTTP 客户端 + SSE 切帧器
│
├── mcp/                         # MCP 客户端
│   └── src/
│       ├── lib.rs                          # McpManager：按项目目录复用连接
│       ├── client.rs  config.rs  result.rs
│
├── agent/                       # Agent 循环：上下文、工具、会话日志
│   └── src/
│       ├── agent/
│       │   ├── mod.rs                      # 主循环（模型↔工具）
│       │   ├── request.rs                  # RequestContext：每轮指令+工具定义
│       │   ├── dispatch.rs                 # 工具并发调度（≤8）
│       │   ├── inbox.rs                    # SteeringInbox 输入队列
│       │   ├── questions.rs                # 提问工具
│       │   └── compaction.rs               # PreparedCompaction 组装
│       ├── compaction.rs                   # 压缩请求
│       ├── request_execution.rs            # 模型请求执行、重试、保存
│       ├── message.rs  image.rs  prompts.rs
│       ├── session/
│       │   ├── manager.rs                  # SessionManager：日志追加写
│       │   ├── format.rs  file.rs          # 日志格式、末尾修复
│       │   └── context.rs                  # 从日志生成模型上下文、Token 估算
│       └── tools/
│           ├── registry.rs                 # 工具注册表、参数校验
│           ├── read.rs  truncate.rs  output.rs  question.rs  mcp.rs
│           └── bash/
│               ├── mod.rs  spec.rs         # 参数与定义
│               ├── exec.rs  pump.rs        # 前台执行、stdout/stderr 异步泵
│               ├── shell.rs                # Job Object 进程树管理
│               └── tasks.rs                # BashTasks：后台任务状态
│
└── app/                         # 桌面后端组装层（main.rs 为 Singularity.exe 入口）
    └── src/
        ├── desktop.rs
        ├── desktop/
        │   ├── app_server.rs               # AppServer：会话生命周期
        │   ├── app_server/{actions,errors,shutdown,workspace}.rs
        │   ├── rpc.rs                      # 方法匹配表分发
        │   ├── transport.rs                # stdio 帧读写
        │   ├── workspace_store.rs          # 项目路径存储
        │   └── file_view.rs                # 文件栏读取
        ├── session_options.rs
        └── runtime/
            ├── mod.rs  history.rs          # 历史投影、用量累计
            ├── thread_catalog.rs           # 任务列表/摘要
            ├── conversation.rs             # Conversation：任务执行体
            └── conversation/
                ├── turn.rs                 # TurnControls：停止/提问
                ├── lifecycle.rs            # 启动条件检查与执行编排
                ├── view.rs                 # 历史读取与事件发布
                ├── state.rs  activity.rs
                ├── compaction.rs           # 独立压缩
                └── background.rs           # 后台命令完成→唤醒回合
```

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
