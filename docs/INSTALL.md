# 安装与运行

Singularity 当前发布目标是 Windows x86-64。可执行文件之间使用同目录发现，不需要注册服务、为 Singularity 安装解释器或配置 sandbox backend。目标仓库需要的 Python、Node.js、Rust 等工具链仍需由用户安装并加入宿主机 `PATH`。

## 安装 release

1. 从 GitHub Release 下载 `singularity-<version>-windows-x86_64.zip` 和 `SHA256SUMS.txt`。
2. 在 PowerShell 7 中核对 SHA-256：

   ```powershell
   (Get-FileHash .\singularity-<version>-windows-x86_64.zip -Algorithm SHA256).Hash.ToLowerInvariant()
   Get-Content .\SHA256SUMS.txt
   ```

3. 解压到固定目录，例如 `D:\Apps\Singularity`。
4. 保持以下文件位于同一目录：

   ```text
   sg.exe
   singularity_app_server.exe
   singularity-command-runner.exe
   singularity-windows-sandbox-setup.exe
   ```

5. 将目录加入当前用户的 `PATH`，重新打开 PowerShell 7，然后运行：

   ```powershell
   sg --help
   sg config doctor
   ```

release archive 不执行安装脚本，也不修改系统级 `PATH`。

## Release Authenticode 签名

Windows tagged release 的签名接口已经接入 `.github/workflows/release.yml`：四个发布 `.exe` 会先使用 Authenticode 和 SHA-256 文件摘要签名，再生成 archive、`SHA256SUMS.txt`、SBOM 和 provenance attestation。这里的“接口已接入”不代表当前本地 checkout 已经持有真实证书或能够证明真实签名。

工作流使用以下 GitHub 配置，名称必须完全一致：

- encrypted secret `WINDOWS_CODESIGNING_PFX_BASE64`：代码签名证书及私钥的 base64 PFX 内容。
- encrypted secret `WINDOWS_CODESIGNING_PFX_PASSWORD`：上述 PFX 的密码。
- repository variable `WINDOWS_CODESIGNING_TIMESTAMP_URL`：RFC3161 timestamp URL。

PFX 只在 Windows runner 的临时 CurrentUser certificate store 中使用。工作流会检查 leaf certificate 的私钥、当前有效期和 Code Signing EKU，按精确 thumbprint 选择证书，并在成功或失败后删除临时 PFX、临时工具日志和本次导入的证书；密码、PFX 内容和 thumbprint 不写入仓库、artifact 或日志。

只有 `push` 的 `v*` tag 是正式发布边界。该边界缺少任一配置、`signtool.exe`、有效证书、签名、RFC3161 timestamp 或 Authenticode policy 验证失败时会 fail closed，不会退回 unsigned release，也不会发布资产。`workflow_dispatch` 始终生成 `dev-<run>` artifact：没有任何签名配置时会明确 warning，并在 artifact 名称中标记 `unsigned`；配置完整时可以签名，但仍不是 tagged release。仓库没有真实证书和私钥时，只能确认上述安全接口已接入，不能据此宣称本地或当前任务已完成真实签名。

## 配置 provider

首次配置或迁移旧配置时，可以复制 `.env.example` 到任意临时位置并填写：

```dotenv
SINGULARITY_BASE_URL=https://provider.example/v1
SINGULARITY_API_KEY=replace-with-your-api-key
SINGULARITY_MODEL=your-model-name
```

然后显式导入到用户级配置：

```powershell
sg config import-env --file C:\path\to\.env
```

导入完成后，该 `.env` 不再参与运行时解析，可以安全移出项目。Singularity 在 app-server 启动时捕获一次配置快照：

1. 进程环境中只要存在任一 provider 变量，就只使用进程环境层。
2. 否则读取 `%USERPROFILE%\.singularity\config.json` 及其引用的私有认证文件。
3. 三个必需值必须来自同一层，缺失时关闭失败。

如需为当前进程临时提供完整的多-provider 配置，可以用 `SINGULARITY_MODELS_CONFIG` 指向一个 JSON 文件；该进程环境层会整体覆盖用户级默认配置。`default_model` 和 `thread.start.model` 使用完整 `provider_id/model_id`，`providers` 的键和 `models` 的键分别构成 provider 与模型 allowlist。每个模型必须明确声明 `api_protocol`（`chat` 或 `responses`）和 `max_output_tokens`；`max_context_tokens` 可以省略表示上下文窗口未知。`adapter` 当前只支持 `openai_compatible`。`api_key_env` 只能是环境变量名，密钥不会写入 JSON、快照 debug 或上游请求的 model 字段。示例：

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

进程启动时一次性读取该文件及所引用的密钥环境变量，并建立不可变 snapshot；不根据 base URL 猜协议，也不自动轮换 provider/model。显式未知 provider、未 allowlist 的 model 或 malformed selector 会 fail closed。项目 `.env` 不会被自动读取；它只可作为显式 `import-env` 的一次性输入。用户级配置的 `/models` 发现只用于公开 ID 列表，能力仍由显式覆盖决定。

思考档位必须逐模型声明，`reasoning_variants` 是唯一事实源；每个 variant 都写 `enabled`，启用档位可写一个 `wire_effort`，而 `off` 必须显式写成 `enabled:false`。`default_variant` 必须精确命中；无 map 表示不支持。Chat 纯开关只允许一个无 wire 的 `on`，high/max 等多档必须逐项写 wire；Responses 的每个启用档位必须写 wire。selector 可用 `provider_id/model_id#variant` 精确选择，未知档位、未声明的 `#off` 和不支持的模型 fail closed，不承诺自动 catalog 识别。

Provider 私有 reasoning/output items 为 approval、重启和跨 turn 的官方续接保留在私有 checkpoint/本地 SQLite 中；它们不投影到公共 conversation、trace、Evaluation 或错误正文。SQLite 不是内容加密层，Responses `encrypted_content` 仍是 provider opaque blob。

可选的 legacy `SINGULARITY_MODEL_CONTEXT_TOKENS` 和 `SINGULARITY_MODEL_MAX_OUTPUT_TOKENS` 分别覆盖 context window 和最大输出 token 数；默认值为 `128000` 和 `4096`。前者必须为 `1..=2000000`，后者必须为 `1..=1000000`，且最大输出必须严格小于 context window。

Provider 配置值不会被静默 trim 或纠正。进程环境和显式导入文件中的模型、地址、密钥、provider 名称及 token limit 值如果含 `CR`、`LF`、`NUL` 或首尾空白，会以 `provider_configuration_invalid` fail closed，且不会产生任何 provider attempt；导入文件的标准 `CRLF` 行尾仍会正常解析。

工具选择及通用工具能力由运行时自动协商；模型专属的消息角色和 reasoning history 则必须通过下面的模型字段显式声明。协议选择、能力缓存、工具 schema 与 fail-closed 边界以 [架构事实文档](singularity.md#7-model-与-provider) 为准。

需要跨仓库、跨 worktree 共用同一 provider 时，运行一次：

```powershell
sg config import-env
# 或：sg config import-env --file C:\path\to\.env
```

该命令把地址、默认 selector 和 `auth_generation` 提交点写入 `%USERPROFILE%\.singularity\config.json`，把 API key 写入同目录不可预测且权限受限的版本化 `auth.v1-<随机值>.json`；读取时先读取 config，再按其中引用读取同一 auth generation，因此不会观察到新 config 配旧 auth。命令最后输出 `selectable=true|false`：只有已有完整、有效的协议、token 限制、reasoning/thinking/tool/role 能力覆盖且认证存在时才为 `true`。新模型或缺少能力字段的模型只写入不可选择的 ID skeleton，不会从 URL、模型名或路径猜测协议、上下文或输出上限；包含 `#variant` 的 selector 只有在该模型已有显式 variant 时才允许导入。密钥不会出现在命令行参数、模型目录、缓存、诊断或 Git 中。运行时解析优先级为进程环境 > 用户目录配置，因此临时进程设置仍可覆盖用户默认值。

Provider 地址必须是非空的绝对 `http`/`https` URL，且必须带 host；不得包含用户名、密码、query 或 fragment。原始合法 path 会保留，地址不会被静默 trim 或修正。Provider ID 和 reasoning variant 不得包含 `/` 或 `#`；模型 ID 可以在 selector 的第一个 `/` 之后包含内部 `/`，但不得包含空白、控制字符或 `#`，并受长度上限约束。

查看 provider 的公开模型 ID，并在需要时刷新 `/models`：

```powershell
sg config models
sg config models --refresh
```

`/models` 响应只提供模型 ID，不提供协议、上下文窗口或 reasoning 能力。模型 ID 会进入用户目录的非敏感缓存；缓存读取有 1 MiB 上限，并校验 schema、endpoint hash、记录数量和每个 ID，非法或超大缓存只报告 `invalid`/`read_failed`，不会阻断已配置运行，也不会把控制字符或换行 ID 打印到 CLI。发现结果仍明确标记 `fresh`、`stale`、`unavailable` 或 `not_configured`。只有 `config.json` 中声明 `api_protocol`、`max_output_tokens` 以及需要的 `reasoning_variants`，并且完整模型校验、合法地址和认证都通过时才会标记为可选择。`max_context_tokens` 可选；省略时运行时保留 `unknown`，不猜测窗口或写入默认值，Agent 的上下文预算会显式处理未知状态。未知能力不会从模型名、URL 或 OpenAI-compatible 适配器推断。

OpenAI-compatible 模型可以设置 `supports_developer_role` 来声明 Chat wire 是否原生接受 `developer` 消息角色；设为 `false` 时，内部 developer 消息会在发送前投影为 `system`，不改变内部消息语义。省略时保持兼容默认值 `true`；provider 不接受 `developer` 角色时应显式写入 `false`。

模型若在带工具调用的续接中返回 provider reasoning，必须显式设置 `tool_reasoning_history`：`disabled`（省略时的默认值）表示不回放历史 reasoning；`reasoning_content` 只适用于 `api_protocol: "chat"`，在 assistant tool-call 续接中原样回放 Chat Completions 的 `reasoning_content`；`responses_items` 只适用于 `api_protocol: "responses"`，原样回放 Responses reasoning items。启用非 `disabled` 值时，必须同时声明启用的 `reasoning_variants` 和匹配的 `default_variant`。取值应依据供应商官方协议说明或实际 wire 证据填写，不能从 `/models` 返回的模型 ID、模型名、URL 或 OpenAI-compatible 适配器推断。

用户配置可以为单个模型补充显式覆盖，例如：

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
          "thinking_wire_format": "enable_thinking",
          "tool_reasoning_history": "reasoning_content",
          "reasoning_variants": {
            "off": {"enabled": false},
            "low": {"enabled": true, "wire_effort": "low"},
            "medium": {"enabled": true, "wire_effort": "medium"},
            "high": {"enabled": true, "wire_effort": "high"},
            "xhigh": {"enabled": true, "wire_effort": "xhigh"},
            "max": {"enabled": true, "wire_effort": "max"}
          },
          "default_variant": "max"
        }
      }
    }
  }
}
```

`thinking_wire_format` 只在 Chat Completions 模型上生效：`thinking_type` 使用 `thinking: {"type": ...}`，`enable_thinking` 使用供应商明确规定的顶层 `enable_thinking` 布尔字段。Responses 仍固定使用其 `reasoning` 对象。上下文窗口未知时保留 `unknown`，不猜测或填入默认值；Agent 会跳过本地窗口上限检查，但仍遵守显式输出上限。

## Windows sandbox 首次运行

用户不需要选择 backend 或维护 sandbox 配置文件。AgentLoop 的命令工具固定经过 `WindowsSandboxBackend`：

1. 启动时自动选择当前平台可用的严格 sandbox；用户不需要选择 backend 或维护安全配置文件。
2. 首次需要平台初始化时，应用自动完成必要准备并在确需提升权限时显示系统提示。
3. 越界写入、网络请求和受保护路径由 Policy/Approval 处理；不能在当前平台安全执行时明确失败，不降级为本地进程。
4. 命令工具接受 `command` 字符串以及可选 `cwd`、`timeout_seconds`；PATH、shell 方言和可信内部参数转换由平台 adapter 处理。
5. 运行时统一提供取消、超时、进程树终止和有界输出；这些实现细节不会进入模型工具 schema。

默认 sandbox home 是 `%USERPROFILE%\.singularity`。确实需要把其状态放到其他盘时，可在启动 `sg` 前设置绝对路径：

```powershell
$env:SINGULARITY_HOME = "D:\SingularityHome"
```

该变量是位置覆盖，不是安全模式开关。

## 从源码构建

前置条件：

- Git（2.49 或更高版本使用 `git clone --revision` 的固定 commit 快速路径；较旧版本使用严格 sandbox 内的 no-checkout clone、detached checkout 和精确校验）
- Rust 1.96.0（MSVC 工具链）
- Visual Studio Build Tools 的 Desktop development with C++ 组件
- PowerShell 7

仓库已经通过 `rust-toolchain.toml` 固定 toolchain。只构建四个产品 release binary：

```powershell
git clone https://github.com/qjcnmd/singularity-agent.git
Set-Location singularity-agent
$env:CARGO_TARGET_DIR = "D:\Temp\singularity-target"
cargo build --release --locked --package singularity_cli --package singularity_app_server --package singularity_windows_sandbox --bins
```

将 `$env:CARGO_TARGET_DIR\release` 中的四个 binary 保持在同一目录。若不设置 `CARGO_TARGET_DIR`，默认输出位于仓库的 `target\release`。

## 运行与状态位置

```powershell
sg run "检查当前项目并修复一个明确问题"
```

默认 app-server SQLite 位于命令启动目录下：

```text
.singularity/rust-app-server.sqlite3
```

开发和自动化场景可以通过 `SINGULARITY_APP_SERVER_DB` 指向另一个数据库文件；普通安装不需要设置。

## 更新与卸载

更新时退出正在运行的任务，用新 release 的四个 binary 一起替换旧目录，不要混用不同版本的 helper。

卸载时删除安装目录并从 `PATH` 移除该目录。工作区中的 `.singularity` 和 sandbox home 都是用户状态，不会由解压式安装自动删除；确认不再需要历史和 sandbox 缓存后再手动清理。

## 完整验证

```powershell
$env:CARGO_TARGET_DIR = "D:\Temp\singularity-target"
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings
cargo test --workspace --all-targets --locked --no-fail-fast
cargo build --release --locked --package singularity_cli --package singularity_app_server --package singularity_windows_sandbox --bins
```

影响 AgentLoop、provider、工具、sandbox 或 approval 时，还需在代表性工作区配置真实 provider，并通过发布产品链运行普通任务：

```powershell
sg run "检查当前项目并完成一项可验证的修改"
```

Evaluation 是源码仓库中的独立开发工具，不进入发布包。修改 Evaluation runner、task set 或评估证据合同后，才从源码运行：

```powershell
cargo run --locked -p singularity_evaluation --bin singularity-evaluation -- run docs/evaluation/public-representative-task.json --run-id development-validation-<timestamp> --json
```

两类真实验证都不能用 fake、mock 或 scripted provider 代替；Evaluation 结果不替代普通产品链验证。
