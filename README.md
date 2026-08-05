# Singularity

Singularity 是一个由 Rust 实现的本地命令行编码代理。核心产品运行时、协议、模型调用、工具和评估保持平台无关；职责明确的辅助工具可以使用其他主流语言，但不得形成第二套产品运行时或绕过 Rust 主链路与安全协议。当前发行包绑定源自 OpenAI Codex CLI 的 Windows 原生 sandbox adapter，并在不能满足请求权限时关闭失败（fail closed）。

当前支持 Windows x86-64。其他平台可以编译和运行确定性测试，但由于没有严格命令沙箱，AgentLoop 不会声明可用。

## 运行结构

```text
sg
  -> singularity_app_server (stdio JSON-RPC)
     -> AgentLoop -> ToolBroker -> WorkspaceTools
        -> WindowsSandboxBackend
     -> OpenAiProvider
     -> SessionStore (SQLite)
```

详细边界、对象、状态流和失败路径见 [`docs/singularity.md`](docs/singularity.md)。

## 安装

从 GitHub Release 下载 `singularity-<version>-windows-x86_64.zip`，校验 `SHA256SUMS.txt` 后解压。压缩包内四个可执行文件必须保留在同一目录：

```text
sg.exe
singularity_app_server.exe
singularity-command-runner.exe
singularity-windows-sandbox-setup.exe
```

将该目录加入 `PATH` 即可，不需要单独安装或选择 sandbox backend。第一次执行需要离线命令沙箱的任务时，系统可能显示一次 Windows UAC 提权提示；setup helper 会建立受限账户、访问控制列表和网络隔离，之后自动复用。用户拒绝提权或 setup 失败时命令不会退回本地进程执行。

Singularity 的核心产品运行时由 Rust 实现，但可以处理 Python、Rust、Node.js、Go 等不同语言的目标仓库。目标项目所需工具链由用户安装并加入宿主机 `PATH`；sandbox 会解析真实可执行文件并授予其安装目录只读/执行权限，不需要维护额外的 sandbox 路径配置。

完整安装、源码构建和更新说明见 [`docs/INSTALL.md`](docs/INSTALL.md)。

## Provider 配置

Provider 配置持久化在用户目录的 `.singularity` 中。已有旧 `.env` 时，显式导入一次：

```dotenv
SINGULARITY_BASE_URL=https://provider.example/v1
SINGULARITY_API_KEY=replace-with-your-api-key
SINGULARITY_MODEL=your-model-name
```

```powershell
sg config import-env --file C:\path\to\.env
sg config models
```

运行时不会自动读取项目 `.env`。只要当前进程中出现任一 provider 变量，Singularity 就只使用该进程环境层；否则读取 `%USERPROFILE%\.singularity\config.json` 及其引用的私有认证文件。`SINGULARITY_MODEL_PROVIDER` 可选，默认值为 `openai_compatible`。密钥不会通过 CLI 参数接收，doctor 只显示脱敏的 present/missing 状态。

需要为当前进程临时提供一份完整的多-provider 配置时，可以设置 `SINGULARITY_MODELS_CONFIG` 指向 JSON 文件；该进程环境层会整体覆盖用户级默认配置。该文件只保存环境变量名，不保存密钥；`default_model` 和 thread 的 `--model`/`thread.start.model` 都使用完整的 `provider_id/model_id`：

```json
{
  "default_model": "opencode-go/deepseek-v4-flash",
  "providers": {
    "opencode-go": {
      "adapter": "openai_compatible",
      "base_url": "https://opencode.ai/zen/go/v1",
      "api_key_env": "OPENCODE_API_KEY",
      "models": {
        "deepseek-v4-flash": {
          "api_protocol": "chat",
          "max_context_tokens": 1000000,
          "max_output_tokens": 384000
        }
      }
    }
  }
}
```

配置在 app-server 或 Evaluation 进程启动时只捕获一次；provider、model、协议和 limits 在一个 turn/trial 内保持不变。每个模型必须明确写 `chat` 或 `responses`，不会根据 URL 推断或跨协议 fallback；model id 不在 allowlist、provider 不存在或 selector 不是 `provider_id/model_id` 时 fail closed。

思考档位也是模型配置的一部分，不按 provider 名称猜测、自动 catalog 或静默降档。`reasoning_variants` 是唯一事实源：每个 variant 必须写 `enabled`，启用档位可写一个 `wire_effort`；`off` 只有显式写成 `enabled:false` 才可选择，`default_variant` 必须精确命中。没有 map 表示不支持思考。Chat 纯开关只能声明一个无 wire 的 `on`，high/max 等多档必须逐项写 wire；Responses 的每个启用档位都必须写 wire。

例如 DeepSeek 的 high/max/off 选择：

```json
"deepseek-v4-flash": {
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
```

选择 `provider_id/model_id#off` 时必须显式使用该 disabled variant。Chat 发送 `thinking.type=enabled` 与单一解析后的 wire effort；Responses 发送 `reasoning.effort` 与 `include=["reasoning.encrypted_content"]`。工具循环所需的 `reasoning_content` 或 Responses 原始 output items 只在 provider 私有 checkpoint/本地 SQLite 中为 approval、重启和跨 turn 官方续接保留，不进入用户消息、公共 trace、Evaluation 或错误正文；SQLite 不是内容加密层，Responses `encrypted_content` 仍是 provider opaque blob。

可选的 legacy `SINGULARITY_MODEL_CONTEXT_TOKENS` 和 `SINGULARITY_MODEL_MAX_OUTPUT_TOKENS` 分别覆盖 context window 和最大输出 token 数；默认值为 `128000` 和 `4096`。前者必须为 `1..=2000000`，后者必须为 `1..=1000000`，且最大输出必须严格小于 context window。

工具能力由运行时自动协商，不是用户配置项；协议选择、能力缓存、工具 schema 与 fail-closed 边界以 [架构事实文档](docs/singularity.md#7-model-与-provider) 为准。

检查配置：

```powershell
sg config doctor
```

## 常用命令

启动新任务：

```powershell
sg run "审查并修复当前仓库中的失败测试"
sg run "只读解释当前模块的数据流" --model gpt-example
```

继续已有任务：

```powershell
sg threads
sg continue <thread-id> "继续完成剩余修改"
```

`continue` 始终创建新 Turn（新任务回合）；若该线程存在未完成的 Turn（暂停/挂起/等待审批），`continue` 会提示你改用同一 Turn 命令：

```powershell
sg turn status <turn-id>
sg turn resume <turn-id>          # 从持久化存档点恢复暂停/挂起的同一 Turn
sg turn pause <turn-id>           # 暂停正在运行的 Turn，不终止其存档
sg turn input <turn-id> "补充要求" --delivery follow-up   # 向非终态 Turn 追加真实用户输入
```

`turn input` 的 `--input-id` 是幂等键（缺省时自动生成一次）；`--delivery steer|follow-up` 控制投递时机（steer 在下一安全边界消费，follow-up 排队到 Turn 收尾）。

状态、取消和审批：

```powershell
sg turn status <turn-id>
sg turn interrupt <turn-id>
sg approvals
sg approve <request-id> --decision allow --reason "已核对操作范围"
```

追踪：

```powershell
sg trace <run-id> --limit 20
sg trace show <event-id>
```

默认状态库位于当前工作目录的 `.singularity/rust-app-server.sqlite3`。sandbox 自身的受限账户元数据和 helper 缓存默认位于 `%USERPROFILE%\.singularity`；只有需要改变该位置时才设置 `SINGULARITY_HOME`。

## 安全边界

- 工作区写入前进行规范化路径检查；符号链接、junction 和 `..` 不能绕过根目录边界。
- `.git`、`.singularity`、环境文件、密钥和其他受保护路径默认拒绝模型写入。
- 命令默认无网络，并由平台 sandbox 自动执行；超时、取消、进程树终止和输出上限由运行时统一处理。
- 网络或越界操作按 Policy/Approval 合同处理；当前平台无法提供严格 sandbox 时明确失败，不使用本地进程或无沙箱后端。
- provider 原始响应、密钥、原始工具参数和内部审计字段不会投影到公共工具结果。
- 修改工作区后，AgentLoop 必须观察到成功命令验证，且不能带着未解决的可修复失败直接完成。

## 从源码验证

需要 Rust 1.96.0。为减少系统盘占用，可在单次 PowerShell 7 会话中设置非系统盘 target：

```powershell
$env:CARGO_TARGET_DIR = "D:\Temp\singularity-target"
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings
cargo test --workspace --all-targets --locked --no-fail-fast
```

开发期能力评估使用独立工具，不属于 `sg` 或发布包：

```powershell
cargo run --locked -p singularity_evaluation --bin singularity-evaluation -- run docs/evaluation/public-representative-task.json --run-id representative-001 --json
```

完整 release 构建：

```powershell
cargo build --release --locked --package singularity_cli --package singularity_app_server --package singularity_windows_sandbox --bins
```

## 许可证

项目主体使用 [MIT License](LICENSE)。`crates/windows-sandbox` 的来源和 Apache-2.0 许可见 [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) 与该 crate 内的 `UPSTREAM.md`、`LICENSE`、`NOTICE`。
