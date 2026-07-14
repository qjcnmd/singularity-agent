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

可选的 `SINGULARITY_MODEL_CONTEXT_TOKENS` 和 `SINGULARITY_MODEL_MAX_OUTPUT_TOKENS` 分别覆盖 context window 和最大输出 token 数；默认值为 `128000` 和 `4096`。前者必须为 `1..=2000000`，后者必须为 `1..=256000`，且最大输出必须严格小于 context window。工具能力不是用户配置项：每次 run/resume 都调用 capability negotiation；同一 `ProviderConfigSnapshot` 与 effective model 已有成功结果时命中 snapshot cache，不发网络 probe，只有 cache miss 才执行固定、无用户数据的 probe。probe 先验证产品上限内的直接工具定义与实际 developer/user 消息角色；strict profile 另验证 schema 约束，non-strict profile 只证明原生结构化调用、合法工具名、对象参数和调用数。基础 profile 成立后再用固定单工具请求独立验证 required tool choice；明确的 `400/422` 或 `200` 但未调用工具只关闭该可选能力，authentication、rate limit、server、transport、decode 和 cancellation 等 typed failure 仍终止协商。只有 direct capacity 不成立时才验证与真实 router 同形的 `oneOf/const` envelope。Agent 仅在 contract 明确支持且本地 completion/plan/verification 仍不允许 final 时发送 required，否则保持 auto；未验证的 system message 和 JSON mode 保持关闭。真实工具参数始终由本地完整 `ToolSpec` validation 校验，HTTP 200 文本伪工具调用不能绕过本地状态机。

修改配置后，新启动一次 `sg` 命令即可取得新快照。不要把 `.env` 提交到 Git。

## Windows sandbox 首次运行

用户不需要选择 backend 或维护 sandbox 配置文件。AgentLoop 的命令工具固定经过 `WindowsSandboxBackend`：

1. 先尝试 Codex 风格的 elevated sandbox。
2. 如果尚未完成 setup，Windows 显示 UAC 提示，setup helper 自动建立产品专用的受限账户、ACL、WFP/防火墙规则和 helper 目录。
3. 网络被拒绝时必须使用 elevated offline identity；失败后不会降级。
4. 只有请求允许网络、restricted token 足以满足权限时，才允许 unelevated restricted-token 路径。
5. 所有路径都使用 Job Object、进程树终止、超时和有界输出捕获。
6. 裸命令通过宿主机 `PATH`/`PATHEXT` 解析为规范化可执行文件；安全的外部工具链目录只获得读取和执行权限，写入仍限于 workspace 和 sandbox 临时目录。

默认 sandbox home 是 `%USERPROFILE%\.singularity`。确实需要把其状态放到其他盘时，可在启动 `sg` 前设置绝对路径：

```powershell
$env:SINGULARITY_HOME = "D:\SingularityHome"
```

该变量是位置覆盖，不是安全模式开关。

## 从源码构建

前置条件：

- Git
- Rust 1.96.0（MSVC 工具链）
- Visual Studio Build Tools 的 Desktop development with C++ 组件
- PowerShell 7

仓库已经通过 `rust-toolchain.toml` 固定 toolchain。构建全部 release binary：

```powershell
git clone https://github.com/qjcnmd/singularity-agent.git
Set-Location singularity-agent
$env:CARGO_TARGET_DIR = "D:\Temp\singularity-target"
cargo build --workspace --bins --release --locked
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

开发和自动化场景可以通过 `SINGULARITY_APP_SERVER_DB` 指向另一个数据库文件；普通安装不需要设置。评估输出默认位于 `work/evaluations/<run-id>`，也可以通过 `SINGULARITY_EVAL_OUTPUT_DIR` 覆盖。

## 更新与卸载

更新时退出正在运行的任务，用新 release 的四个 binary 一起替换旧目录，不要混用不同版本的 helper。

卸载时删除安装目录并从 `PATH` 移除该目录。工作区中的 `.singularity`、`work/evaluations` 以及 sandbox home 都是用户状态，不会由解压式安装自动删除；确认不再需要历史、评估产物和 sandbox 缓存后再手动清理。

## 完整验证

```powershell
$env:CARGO_TARGET_DIR = "D:\Temp\singularity-target"
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings
cargo test --workspace --all-targets --locked --no-fail-fast
cargo build --workspace --bins --release --locked
```

影响 AgentLoop、provider、工具、sandbox、approval 或 evaluation 时，还需配置真实 provider 并运行：

```powershell
sg eval run docs/evaluation/public-representative-task.json --run-id release-validation-<timestamp> --json
```

真实验证必须进入 AgentLoop，不能用 fake、mock 或 scripted provider 代替。
