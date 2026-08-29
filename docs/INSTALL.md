# 安装与运行

Singularity 当前发布目标是 Windows x86-64。`sg` 是单一可执行文件；目标仓库需要的 Python、Node.js、Rust 等工具链仍需由用户安装并加入宿主机 `PATH`。

## 从源码构建

前置条件：

- Rust 1.96.0（MSVC 工具链）
- Visual Studio Build Tools 的 Desktop development with C++ 组件
- [Git for Windows](https://git-scm.com/install/windows)（提供 Git Bash；`sg` 启动时必须能发现 `bash.exe`）
- PowerShell 7（可选）

仓库通过 `rust-toolchain.toml` 固定 toolchain：

```powershell
git clone https://github.com/qjcnmd/singularity-agent.git
Set-Location singularity-agent
$env:CARGO_TARGET_DIR = "D:\Temp\singularity-target"
cargo build --release --locked --package singularity_cli
```

将 `$env:CARGO_TARGET_DIR\release\sg.exe` 所在目录加入 `PATH`。未设置 `CARGO_TARGET_DIR` 时，构建输出位置由仓库 `.cargo/config.toml` 的 `target-dir` 决定。

验证安装：

```powershell
sg --help
```

## 配置 provider

provider 配置只来自用户配置目录 `%USERPROFILE%\.singularity\config.json` 及其引用的私有认证文件 `auth.json`；provider 目录、endpoint 与 api key 都出自这一层，缺失时 fail closed。`config.json` 声明 provider 目录与默认 selector（完整示例见下），`auth.json` 按 provider 存 api key：

```json
{
  "schema_version": 1,
  "providers": {
    "dashscope": { "api_key": "replace-with-your-api-key" }
  }
}
```

每个模型必须显式声明 `api_protocol: chat|responses`；不会根据 URL 推断协议或跨协议 fallback。模型条目不接受未知字段。模型限额优先使用条目中的 `max_context_tokens` / `max_output_tokens`，其次使用内置静态表；未知模型应显式声明这两项，缺省时仅使用保守默认值 `128000` / `4096`，且最大输出必须严格小于 context window。配置值不会被静默 trim 或纠正：含控制字符或首尾空白的必填值以 `provider_configuration_invalid` fail closed。

### 思考档位

思考档位逐模型声明，`reasoning_variants` 是唯一事实源：每个 variant 必须写 `enabled`，启用档位可写一个 `wire_effort`，`off` 必须显式写成 `enabled:false` 才可选择；`default_variant` 必须精确命中。selector 使用 `provider_id/model_id#variant` 精确选择。Chat 纯开关只允许一个无 wire 的 `on`，high/max 等多档必须逐项写 wire；Responses 的每个启用档位必须写 wire。

完整配置示例：

```json
{
  "version": 1,
  "default_provider": "dashscope",
  "default_model": "dashscope/deepseek-v4-flash-0731#max",
  "providers": {
    "dashscope": {
      "base_url": "https://dashscope.aliyuncs.com/compatible-mode/v1",
      "models": {
        "deepseek-v4-flash-0731": {
          "api_protocol": "chat",
          "max_context_tokens": 1000000,
          "max_output_tokens": 393216,
          "tool_reasoning_history": "reasoning_content",
          "reasoning_variants": {
            "off": {"enabled": false},
            "high": {"enabled": true, "wire_effort": "high"},
            "max": {"enabled": true, "wire_effort": "max"}
          },
          "default_variant": "max"
        }
      }
    }
  }
}
```

续接中需要回放 provider reasoning 时必须设置 `tool_reasoning_history`：`reasoning_content` 只适用于 chat 协议；`responses_items` 只适用于 responses 协议并绑定 function-call IDs；默认 `disabled` 不回放。取值依据供应商官方协议说明或实际 wire 证据填写。

TUI 内用 `/model` 或 `/settings` 为当前 Thread 选择 provider/model/reasoning（写入该 Thread 元数据；活动 turn 期间排队到本轮结束后生效）。provider 注册、认证与全局限额编辑不进入 TUI。

## 运行

交互模式（长驻 TUI）：

```powershell
sg
sg --session <thread-id>
```

无交互模式（goal 是必需位置参数）：

```powershell
sg --print "检查当前项目并修复一个明确问题"
sg --json "检查当前项目并完成一项可验证的修改" --model dashscope/deepseek-v4-flash-0731#max
```

- `--print` 只向 stdout 输出最终 assistant 文本；
- `--json` 输出逐行 JSONL 事件并以终态 `summary` 行收尾（供脚本与评估器解析）；
- `--model <selector>` 只覆盖本次执行；
- `--session <id>` 恢复既有 Thread；`--no-session` 本次不持久化；默认持久化。

会话正文位于 `%USERPROFILE%\.singularity\sessions\<uuid>.jsonl`（唯一持久事实源）；测试与自动化可通过 `SINGULARITY_HOME` 隔离用户状态。

无交互模式中，第一次 Ctrl+C 中断当前 turn，第二次强制退出；退出码 0/130/1 分别表示 completed/interrupted/failed。TUI 用 Esc 停止生成；Ctrl+C 先清空输入并确认退出，再按一次退出（空闲时退出码 0，运行中强制退出码 130）。

## 更新与卸载

更新时用新构建的 `sg.exe` 替换旧文件。卸载时删除安装目录并从 `PATH` 移除；用户状态集中在 `%USERPROFILE%\.singularity\`，不会自动删除。

## 完整验证

```powershell
$env:CARGO_TARGET_DIR = "D:\Temp\singularity-target"
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings
cargo test --workspace --all-targets --locked --no-fail-fast
```

影响 AgentLoop、provider、工具或会话的改动，还需在代表性工作区配置真实 provider 并通过新入口运行普通任务核对链路。
