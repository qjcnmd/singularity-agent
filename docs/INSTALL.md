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

复制仓库中的 `.env.example` 为目标项目或其父目录下的 `.env`，填写：

```dotenv
SINGULARITY_BASE_URL=https://provider.example/v1
SINGULARITY_API_KEY=replace-with-your-api-key
SINGULARITY_MODEL=your-model-name
```

Singularity 在 app-server 启动时捕获一次配置快照：

1. 进程环境中只要存在任一 provider 变量，就只使用进程环境层。
2. 否则从启动目录向父目录查找最近的 `.env`。
3. 三个必需值必须来自同一层，缺失时关闭失败。

可选的 `SINGULARITY_MODEL_CONTEXT_TOKENS` 和 `SINGULARITY_MODEL_MAX_OUTPUT_TOKENS` 分别覆盖 context window 和最大输出 token 数；默认值为 `128000` 和 `4096`。前者必须为 `1..=2000000`，后者必须为 `1..=256000`，且最大输出必须严格小于 context window。

Provider 配置值不会被静默 trim 或纠正。进程环境和 `.env` 中的模型、地址、密钥、provider 名称及 token limit 值如果含 `CR`、`LF`、`NUL` 或首尾空白，会在启动时以 `provider_configuration_invalid` fail closed，且不会产生任何 provider attempt；`.env` 的标准 `CRLF` 行尾仍会正常解析。

工具能力由运行时自动协商，不是用户配置项；协议选择、能力缓存、工具 schema 与 fail-closed 边界以 [架构事实文档](singularity.md#7-model-与-provider) 为准。

修改配置后，新启动一次 `sg` 命令即可取得新快照。不要把 `.env` 提交到 Git。

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
