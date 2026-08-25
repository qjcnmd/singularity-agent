# Singularity

Singularity 是一个由 Rust 实现的本地 coding-agent harness。`sg` 无参数进入长驻交互式 TUI，`--print`/`--json` 进行单次无交互执行；两种入口在进程内复用同一个 Agent/Session/Provider 运行时。核心运行时、模型调用和工具保持平台无关。

当前支持 Windows x86-64。其他平台可以编译和运行确定性测试。

## 运行结构

```text
sg（单进程，双入口）
  ├─ 交互式 TUI（无参数）：主会话流 + 多行编辑器 + 状态行
  └─ --print / --json：单次无交互执行
        └─ crates/runtime：Turn 执行唯一所有者
             ├─ TurnRunner（单轮管线）+ Conversation（Thread 生命周期协调：
             │  单活动 turn、steer 注入当前轮、followUp FIFO 自执行、设置终态后自动生效）
             └─ AgentLoop（headless core：Agent 循环 + ToolRegistry read/glob/grep/bash/edit/write）
                  └─ OpenAiProvider（Chat / Responses）+ 会话 JSONL（唯一权威正文）
crates/app-server（stdio JSON-RPC 适配器）：GUI 接入面，执行全部委托 runtime
```

详细边界、对象、事件流和失败路径见 [`docs/singularity.md`](docs/singularity.md)。

## 安装

从源码构建：

```powershell
cargo build --release --locked --package singularity_cli
```

将生成的 `sg.exe` 所在目录加入 `PATH`。Windows 运行前必须安装 [Git for Windows](https://git-scm.com/install/windows)，使 `bash.exe` 可从宿主机 `PATH` 发现；目标项目所需的其他工具链由用户安装并加入宿主机 `PATH`。

完整安装、源码构建和更新说明见 [`docs/INSTALL.md`](docs/INSTALL.md)。

## Provider 配置

Provider 配置持久化在 `%USERPROFILE%\.singularity\config.json` 及其引用的私有认证文件 `auth.json`；也可用进程环境层整体覆盖（任一 provider 变量出现即只使用该层）：

```dotenv
SINGULARITY_BASE_URL=https://provider.example/v1
SINGULARITY_API_KEY=replace-with-your-api-key
SINGULARITY_MODEL=your-model-name
```

运行时不会自动读取项目 `.env`。每个模型必须显式声明 `api_protocol: chat|responses`，不会根据 URL 推断或跨协议 fallback；配置非法时 fail closed。模型限额优先使用条目中的 `max_context_tokens` / `max_output_tokens`，其次使用内置静态表；未知模型应显式声明这两项，缺省时仅使用保守默认值 `128000` / `4096`。

思考档位是模型配置的一部分：`reasoning_variants` 是唯一事实源，每个 variant 必须写 `enabled`，启用档位可写 `wire_effort`；选择形如 `provider_id/model_id#variant`。示例：

```json
{
  "version": 1,
  "default_provider": "dashscope",
  "default_model": "dashscope/deepseek-v4-flash-0731#high",
  "providers": {
    "dashscope": {
      "base_url": "https://dashscope.aliyuncs.com/compatible-mode/v1",
      "models": {
        "deepseek-v4-flash-0731": {
          "api_protocol": "chat",
          "max_context_tokens": 1000000,
          "max_output_tokens": 384000,
          "reasoning_variants": {
            "off": {"enabled": false},
            "high": {"enabled": true, "wire_effort": "high"},
            "max": {"enabled": true, "wire_effort": "max"}
          },
          "default_variant": "high",
          "tool_reasoning_history": "reasoning_content"
        }
      }
    }
  }
}
```

TUI 内用 `/model` 快速选择模型，用 `/settings` 设置当前 Thread 的 provider/model/reasoning（活动 turn 期间排队到该轮结束后生效）。`/resume`、`/new`、`/session`、`/compact` 与 `/name` 管理会话。TUI 不编辑 provider 注册、认证或全局配置。可选环境变量 `SINGULARITY_MODEL_CONTEXT_TOKENS` / `SINGULARITY_MODEL_MAX_OUTPUT_TOKENS` 覆盖限额，默认 128000/4096。

## 使用

交互模式（长驻 TUI）：

```powershell
sg
sg --session <thread-id>
```

无交互模式（goal 是必需位置参数）：

```powershell
sg --print "审查并修复当前仓库中的失败测试"
sg --print "只读解释当前模块的数据流" --model gpt-example
```

JSONL 事件输出（供脚本与评估器消费，逐行事件 + 终态 `summary` 行）：

```powershell
sg --json "修复失败测试" --model gpt-example
```

会话选项：默认持久化；`--session <id>` 恢复既有 Thread；`--no-session` 本次不持久化。会话正文位于 `~/.singularity/sessions/<uuid>.jsonl`（唯一事实源）；测试与自动化可通过 `SINGULARITY_HOME` 隔离用户状态。

## 安全边界

- 命令在进程内执行并继承进程权限；内部状态（会话、备份、配置）位于 `~/.singularity`，与工作区隔离。
- 工具信任后直接执行，可读写任意路径；密钥边界由 provider 错误脱敏承担。
- 显式超时、取消、进程树终止和输出上限由运行时统一处理。
- provider 原始响应、密钥与内部审计字段不会投影到公共工具结果或事件流。

## 从源码验证

需要 Rust 1.96.0：

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings
cargo test --workspace --all-targets --locked --no-fail-fast
cargo build --release --locked --package singularity_cli
```

## 许可证

项目主体使用 [MIT License](LICENSE)。
