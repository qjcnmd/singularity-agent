# 仓库地图（Repository Map）

> 本文是仓库的浅层导航图：先看总览与依赖，再按 crate 展开到文件。
> 深层的架构事实、协议与行为见 [`singularity.md`](singularity.md)。
> 目录树以 2026-08-25 `main@d2cfb25` 为准。

## 1. 仓库总览

```mermaid
flowchart TD
    ROOT["仓库根目录"]

    ROOT --> CRATES["crates/ — 7 个 Rust crate"]
    ROOT --> DOCS["docs/ — 架构与安装文档"]
    ROOT --> PLAN["plan/ — 执行计划与讨论记录"]
    ROOT --> GITHUB[".github/ — CI 工作流与 Issue 模板"]
    ROOT --> ROOTFILES["根级文件：Cargo.toml · README.md · AGENTS.md · deny.toml · rust-toolchain.toml"]

    CRATES --> C1["core — 跨 crate 基础（取消/权限/项目指令）"]
    CRATES --> C2["protocol — stdio JSON-RPC 协议类型"]
    CRATES --> C3["model — 模型 Provider 与 OpenAI 兼容传输"]
    CRATES --> C4["agent — AgentLoop 与工具/会话/压缩"]
    CRATES --> C5["runtime — Thread/Turn 生命周期与执行管线"]
    CRATES --> C6["app-server — 桌面端 stdio JSON-RPC 后端"]
    CRATES --> C7["cli — sg 入口（TUI/--print/--json）"]
```

## 2. Crate 依赖方向

依赖只沿一个方向：`cli` 与 `app-server` 是入口，`core`/`protocol` 是最底层。
`runtime` 不依赖 `protocol`/`cli`；`protocol` 只服务 app-server 适配器。

```mermaid
flowchart LR
    CLI["cli（sg 入口）"] --> RUNTIME["runtime"]
    CLI --> MODEL["model"]
    CLI --> CORE["core"]
    APPSERVER["app-server"] --> RUNTIME
    APPSERVER --> MODEL
    APPSERVER --> PROTOCOL["protocol"]
    APPSERVER --> AGENT["agent"]
    APPSERVER --> CORE
    RUNTIME --> AGENT
    RUNTIME --> MODEL
    RUNTIME --> CORE
    AGENT --> MODEL
    AGENT --> CORE
    MODEL --> CORE
```

## 3. crates/core — 跨 crate 基础

无依赖的叶子 crate：取消令牌、文件权限、项目指令与用户主目录。

```mermaid
flowchart TD
    subgraph core["crates/core"]
        lib["lib.rs — crate 入口与对外导出"]
        cancellation["cancellation.rs — 可跨线程/异步边界传播的取消令牌"]
        fs_owner["fs_owner.rs — 会话目录/文件的属主权限（Unix 0600/0700）"]
        project_instructions["project_instructions.rs — AGENTS.md 加载、合并与预算截断"]
        user_home["user_home.rs — 用户主目录与 SINGULARITY_HOME 解析"]
    end
    cli["cli / app-server / agent / runtime"] --> core
```

## 4. crates/protocol — 协议类型

单一文件 crate，只依赖 serde。定义 stdio JSON-RPC 的全部方法、事件与对象形状。

```mermaid
flowchart TD
    subgraph protocol["crates/protocol"]
        lib["lib.rs — JSON-RPC 方法注册表、消息 envelope、线程设置、事件与 usage 线格式"]
    end
    appserver["app-server"] --> protocol
    protocol -. "runtime 不依赖它" .-> runtime["runtime"]
```

## 5. crates/model — Provider 与 OpenAI 兼容传输

模型消息类型、Provider 能力契约、配置快照，以及 Chat Completions / Responses 双协议的
请求序列化、流式解码与 HTTP 传输。`AgentLoop` 只执行 Provider 已声明的能力。

```mermaid
flowchart TD
    subgraph model["crates/model"]
        subgraph types["types/ — 请求/响应/消息/工具/用量类型"]
            t_mod["mod.rs — 类型导出入口"]
            t_message["message.rs — ModelMessage 与角色"]
            t_request["request.rs — ModelTurnRequest 与偏好"]
            t_response["response.rs — ModelTurnResponse 与终态原因"]
            t_tool["tool.rs — ToolCall/Schema 与选择策略"]
            t_usage["usage.rs — token 计数器与校验结果"]
            t_reasoning["reasoning.rs — Provider 私有推理重放"]
        end
        subgraph provider["provider/ — 能力契约与遥测"]
            p_mod["mod.rs — Provider trait"]
            p_contract["contract.rs — 能力契约与本地校验"]
            p_runtime["runtime.rs — SelectedModel 解析"]
            p_telemetry["telemetry.rs — attempt 事件与流能力"]
        end
        subgraph openai["openai/ — 双协议适配"]
            o_mod["mod.rs — 适配器入口与 tool_choice payload"]
            o_chat["chat.rs — Chat Completions 请求/响应"]
            o_responses["responses.rs — Responses 请求/响应"]
            o_wire["wire.rs — 端点解析与 OpenAiCompletion"]
        end
        subgraph transport["transport/ — HTTP 与流解码"]
            tr_mod["mod.rs — 完成骨架与 attempt 观测"]
            tr_http["http.rs — 有界 body 读取与错误解析"]
            tr_retry["retry.rs — Retry-After 头解析"]
            tr_stream["stream.rs — SSE 流式解码器（双协议）"]
        end
        subgraph config["config/ — 分层配置"]
            cfg_mod["mod.rs — 配置分层解析与快照"]
            cfg_schema["schema.rs — schema 类型与校验"]
            cfg_runtime["runtime.rs — Provider 实例组装"]
            cfg_selection["selection.rs — provider/model selector 解析"]
            cfg_filesystem["filesystem.rs — 有界文件读取"]
            cfg_user["user/ — 用户配置与 auth.json"]
        end
        catalog["catalog.rs — 编译期模型限额表"]
        error["error.rs — 类型化 ProviderError 与重试合同"]
    end
    agent["agent"] --> model
```

## 6. crates/agent — AgentLoop、工具、会话与压缩

模型无关的执行核心：双层循环状态机、六工具注册表、线性 JSONL 会话与上下文压缩引擎。
`runtime` 的 TurnRunner 调用这里的 `Agent`。

```mermaid
flowchart TD
    subgraph agent["crates/agent"]
        loop_["loop.rs — 双层循环：模型请求、工具批、Steer 注入、压缩"]
        message["message.rs — 会话消息/内容块数据模型"]
        compaction["compaction.rs — 触发判定、安全切点与摘要生成"]
        subgraph session["session/ — 线性 JSONL 会话"]
            s_mod["mod.rs — 稳定 façade 与 SessionProjection"]
            s_format["format.rs — JSONL schema 与严格校验"]
            s_file["file.rs — 有界文件 I/O 与追加限制"]
            s_manager["manager.rs — 会话生命周期与追加"]
            s_context["context.rs — 上下文条目投影"]
            s_repair["repair.rs — 崩溃恢复与孤立工具修复"]
            s_repository["repository.rs — 会话发现与只读投影"]
        end
        subgraph tools["tools/ — 六工具与注册表"]
            t_mod["mod.rs — 工具模块组织"]
            t_registry["registry.rs — 名称→ToolSpec 注册表与参数校验"]
            t_read["read.rs — 有界流式读文件"]
            t_glob["glob.rs — 文件名模式递归匹配"]
            t_grep["grep.rs — 正则逐行搜索"]
            t_bash["bash.rs — Shell 执行与进程树隔离"]
            t_edit["edit.rs — 唯一精确文本替换"]
            t_write["write.rs — 写入/覆盖/建目录"]
            t_truncate["truncate.rs — 输出截断算法"]
            t_walk["walk.rs — 只读目录遍历辅助"]
        end
    end
    runtime["runtime"] --> agent
```

## 7. crates/runtime — Thread/Turn 生命周期

三种产品形态共用的唯一执行层：无交互入口与 TUI 进程内调用 `Conversation`，
app-server 通过它执行同一 runtime。`TurnRunner` 是单个 turn 的唯一所有者。

```mermaid
flowchart TD
    subgraph runtime["crates/runtime"]
        conversation["conversation.rs — Thread 长驻协调器：单活动 turn、Steer、followUp、设置时序"]
        runner["runner.rs — 单个 turn 完整管线：准备、执行、事件投影、终态落盘"]
        events["events.rs — typed TurnEvent 单一事实源"]
        objects["objects.rs — Thread/Turn/usage 公开对象"]
        store["store.rs — 会话创建/定位/修复重开入口"]
        error["error.rs — Turn 失败分类（stage/cause）"]
    end
    cli["cli"] --> runtime
    appserver["app-server"] --> runtime
```

## 8. crates/cli — sg 入口

三种形态入口：无参数进入 TUI、`--print` 单次文本、`--json` 逐事件 JSONL。
全部委托 runtime，渲染只消费 typed `TurnEvent`。

```mermaid
flowchart TD
    subgraph cli["crates/cli"]
        main["main.rs — 参数解析与入口分发"]
        tui_["tui.rs — TUI 主循环与事件驱动渲染"]
        print_mode["print_mode.rs — --print 只输出最终文本"]
        jsonl_mode["jsonl_mode.rs — --json 逐行事件 + summary"]
        session_options["session_options.rs — 会话准备（默认/--session/--no-session）"]
        signal["signal.rs — Ctrl+C 计数与两级取消"]
        subgraph tui["tui/ — 界面状态模块"]
            app["app.rs — 应用状态：输入路由、滚动、footer、整帧渲染"]
            transcript["transcript.rs — 事件流投影为可读条目"]
            editor["editor.rs — 底部多行输入编辑器"]
            scroll["scroll.rs — 会话流滚动状态机"]
        end
    end
    cli --> runtime["runtime"]
```

## 9. crates/app-server — 桌面端后端

stdio JSON-RPC 后端：只做准入、协议对象转换与事件投影，执行全部委托 runtime。
`protocol` 类型只存在于本 crate 与适配器。

```mermaid
flowchart TD
    subgraph appserver["crates/app-server"]
        lib["lib.rs — AppServer 状态与公开 API"]
        main["main.rs — stdio 二进制入口"]
        state["state.rs — 运行时状态容器与协调器注册表"]
        dispatch["dispatch.rs — 请求分发与参数解析"]
        events["events.rs — 生命周期事件投影"]
        session_index["session_index.rs — 进程内会话索引"]
        paths["paths.rs — 持久化路径投影"]
        delete["delete.rs — 会话删除"]
        subgraph lifecycle["lifecycle/ — 投影适配器"]
            projection["projection.rs — TurnEvent → JSON-RPC 通知"]
        end
        subgraph transport["transport/ — stdio 传输层"]
            sup["supervisor.rs — 帧分类与 worker 生命周期"]
            framing["framing.rs — 有界 JSON-Lines 帧切分"]
            output["output.rs — 有序 stdout 输出"]
            terr["error.rs — 传输错误投影"]
        end
    end
    appserver --> runtime["runtime"]
    appserver --> protocol["protocol"]
```

## 10. 一次 turn 的生命周期（跨 crate 全景）

无交互入口（`--json`）执行一个 goal 的完整链条；TUI 与 app-server 走同一 runtime。

```mermaid
sequenceDiagram
    participant CLI as cli（sg --json）
    participant RT as runtime（Conversation/TurnRunner）
    participant AG as agent（AgentLoop）
    participant MD as model（Provider）
    participant TL as tools（六工具）
    participant SS as session（JSONL）

    CLI->>RT: run_turn(goal)
    RT->>SS: 打开/修复会话（单写者）
    RT->>AG: 构造 Agent（provider/工具/配置）
    loop 每轮请求
        AG->>MD: complete（消息+工具 schema）
        MD-->>AG: 响应（文本/工具调用/usage）
        alt 有工具调用
            AG->>TL: 按序执行工具批（preflight→执行）
            TL-->>AG: 结果（含 is_error）
            AG->>SS: 追加 toolResult
        else 无工具调用
            AG->>SS: 追加终态 assistant 消息
        end
        AG->>AG: 下一请求前按 usage 判定压缩
    end
    AG-->>RT: AgentOutcome（final_text/usage/截断）
    RT->>SS: 终态 metadata + usage 落盘
    RT-->>CLI: TurnEvent 流 → JSONL 行 + summary
```

## 11. 其他目录

- **docs/**：`singularity.md`（架构事实文档）、`INSTALL.md`（安装）、`tui-manual-verification.md`（TUI 手工验证）、本文件。
- **plan/**：执行计划与讨论记录（`2026-08-25-simplification-execution-plan.md` 是最近一次简化执行计划的唯一事实源）。
- **.github/**：`ci.yml` 入口 → `rust-gates.yml`（supply-chain + Linux/Windows 门禁）、`release.yml`（发布打包）、Issue 模板。
- **根级文件**：`Cargo.toml`（workspace 成员与统一 lint）、`deny.toml`（依赖策略）、`rust-toolchain.toml`（固定工具链）、`AGENTS.md`（仓库指令）、`README.md`。
- **outputs/**、**.worktrees/**、**.codex/** 等为本地工作产物，不入库。
