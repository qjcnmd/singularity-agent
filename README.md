# Singularity

Singularity 是一个由 Rust 实现的本地 coding-agent harness。当前 CLI 是主要客户端，Desktop 可通过同一 app-server 协议接入；核心运行时、协议、模型调用和工具保持平台无关。职责明确的辅助工具可以使用其他主流语言，但不得形成第二套产品运行时或绕过 Rust 主链路。

当前支持 Windows x86-64。其他平台可以编译和运行确定性测试。

## 运行结构

```text
sg
  -> singularity_app_server（每命令独立 stdio 子进程）
     -> AgentLoop（headless core：Agent 循环 + ToolRegistry read/glob/grep/bash/edit/write）
     -> OpenAiProvider
     -> 会话 JSONL（~/.singularity/sessions/<uuid>.jsonl，唯一权威正文）
     -> 进程内会话索引（启动时从 JSONL 重建，不落盘）
```

详细边界、对象、状态流和失败路径见 [`docs/singularity.md`](docs/singularity.md)。

## 安装

从 GitHub Release 下载 `singularity-<version>-windows-x86_64.zip`，校验 `SHA256SUMS.txt` 后解压。压缩包内两个可执行文件必须保留在同一目录：

```text
sg.exe
singularity_app_server.exe
```

将该目录加入 `PATH` 即可。

Singularity 的核心产品运行时由 Rust 实现，但可以处理 Python、Rust、Node.js、Go 等不同语言的目标仓库。目标项目所需工具链由用户安装并加入宿主机 `PATH`。

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

运行时不会自动读取项目 `.env`。只要当前进程中出现任一 provider 变量，Singularity 就只使用该进程环境层；否则读取 `%USERPROFILE%\.singularity\config.json` 及其引用的私有认证文件。`SINGULARITY_MODEL_PROVIDER` 可选，默认值为 `openai_compatible`。doctor 只显示脱敏的 present/missing 状态。

新 provider 一次录入并自动发现模型：

```powershell
sg config add opencode-go https://opencode.ai/zen/go/v1 --api-key <your-key>
```

该命令校验端点 → 请求 `GET {base_url}/models` 发现模型 id → 用 models.dev 目录元数据（缺失时内置模型表）补齐 context/output 限额，仍未命中回落保守默认 → 把 provider 与限额写入 `%USERPROFILE%\.singularity\config.json` 的 `providers` 段、密钥写入 `auth.v1.json`，并把默认选择指向新 provider 的首个模型。模型限额也可直接手写进 config.json（`.singularity` 内的普通 JSON，同目录两个文件由 CLI 写入时各自原子改名落盘）；带 `#variant` 的默认选择、`reasoning_variants` 与协议要求见下。

配置在 app-server 进程启动时只捕获一次；provider、model、协议和 limits 在一个 turn 内保持不变。每个模型必须明确写 `chat` 或 `responses`，不会根据 URL 推断或跨协议 fallback；model id 不在 allowlist、provider 不存在或 selector 不是 `provider_id/model_id` 时 fail closed。

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

选择 `provider_id/model_id#off` 时必须显式使用该 disabled variant。Chat 发送 `thinking.type=enabled` 与单一解析后的 wire effort；Responses 发送 `reasoning.effort` 与 `include=["reasoning.encrypted_content"]`。工具循环所需的 `reasoning_content`（Chat）以 thinking 内容块随会话 JSONL 持久化，续接时从最后一条 assistant 消息投影为 provider reasoning replay（N2）；Responses 原始 output items 作为 provider opaque continuation state 随会话持久化，在兼容的 provider/model/reasoning 绑定下重放，不进入用户消息或错误正文。

可选的 legacy `SINGULARITY_MODEL_CONTEXT_TOKENS` 和 `SINGULARITY_MODEL_MAX_OUTPUT_TOKENS` 分别覆盖 context window 和最大输出 token 数；默认值为 `128000` 和 `4096`。前者必须为 `1..=2000000`，后者必须为 `1..=1000000`，且最大输出必须严格小于 context window。

工具能力按 provider 静态能力声明（config.json 持久化值为运行时唯一来源，内置模型表与 models.dev 目录仅作 `config add` 录入时的限额 enrichment）决定，不做运行时协商；协议选择、工具 schema 与 fail-closed 边界以 [架构事实文档](docs/singularity.md#9-provider-与模型) 为准。

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

查看与删除会话：

```powershell
sg threads                          # 列出全部会话（含 cwd）
sg session read <session-id>        # 摘要 + 最近片段，不加载全文
sg session delete <session-id>      # 同时删除 JSONL 与进程内索引引用
```

会话正文位于 `~/.singularity/sessions/<uuid>.jsonl`（唯一事实源），进程内索引只缓存展示元数据并在启动时从 JSONL 重建，不落盘；测试与自动化可通过 `SINGULARITY_HOME` 隔离用户状态。

## 安全边界

- 命令在进程内执行并继承进程权限；不做 workspace containment，但内部状态（会话、索引、备份、配置）位于 `~/.singularity`，与工作区隔离。
- 不做受保护路径拒绝规则（对齐 Pi）：工具信任后直接执行，模型可读写任意路径（含 `.git`、环境文件、密钥）；密钥边界由 provider 错误脱敏承担。
- 超时、取消、进程树终止和输出上限由运行时统一处理；read 为有界读取。
- provider 原始响应、密钥、原始工具参数和内部审计字段不会投影到公共工具结果。

## 从源码验证

需要 Rust 1.96.0。为减少系统盘占用，可在单次 PowerShell 7 会话中设置非系统盘 target：

```powershell
$env:CARGO_TARGET_DIR = "D:\Temp\singularity-target"
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings
cargo test --workspace --all-targets --locked --no-fail-fast
```

完整 release 构建：

```powershell
cargo build --release --locked --package singularity_cli --package singularity_app_server --bins
```

## 许可证

项目主体使用 [MIT License](LICENSE)。
