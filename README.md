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

在目标仓库或其父目录创建 `.env`，也可以设置同名进程环境变量：

```dotenv
SINGULARITY_BASE_URL=https://provider.example/v1
SINGULARITY_API_KEY=replace-with-your-api-key
SINGULARITY_MODEL=your-model-name
```

只要进程环境中出现任一 provider 变量，Singularity 就只使用该环境层；否则从当前目录向父目录查找最近的 `.env`。`SINGULARITY_MODEL_PROVIDER` 可选，默认值为 `openai_compatible`。密钥不会通过 CLI 参数接收，doctor 只显示脱敏的 present/missing 状态。

可选的 `SINGULARITY_MODEL_CONTEXT_TOKENS` 和 `SINGULARITY_MODEL_MAX_OUTPUT_TOKENS` 分别覆盖 context window 和最大输出 token 数；默认值为 `128000` 和 `4096`。前者必须为 `1..=2000000`，后者必须为 `1..=256000`，且最大输出必须严格小于 context window。工具能力不是用户配置项：每次 run/resume 都调用 capability negotiation；同一 `ProviderConfigSnapshot` 与 effective model 已有成功结果时命中 snapshot cache，只有 cache miss 才执行固定、无用户数据的 probe。OpenAI-compatible probe 分别验证直接工具定义容量、strict schema、并行/单调用、Agent 实际使用的 developer/user 角色，以及 assistant tool calls → tool results → 下一轮原生工具调用的完整历史；若响应暴露无法安全回传的 reasoning content，则必须证明 adapter 能关闭该模式，否则拒绝。真实 ToolSpec 的 direct definitions 是默认模式；只有 adapter 的单 router probe 也通过并明确协商 `tool_definition_mode=routed` 时才启用有能力损失的路由模式，单纯的容量数值不足不会隐式切换。普通 coding 回合始终使用 `auto`，completion、plan 和 verification 由本地状态机 fail closed；OpenAI-compatible adapter 不从一次 `auto` 响应推断 `required` 能力。工具名在 `ToolRegistry`、模型 schema、wire 和历史中使用同一 `builtin_*` canonical name，不做隐式别名转换。真实参数仍在本地接受完整 `ToolSpec` validation，文本伪工具调用只拒绝、不解析也不执行。

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

运行真实评估：

```powershell
sg eval run docs/evaluation/public-representative-task.json --run-id representative-001 --json
```

默认状态库位于当前工作目录的 `.singularity/rust-app-server.sqlite3`。sandbox 自身的受限账户元数据和 helper 缓存默认位于 `%USERPROFILE%\.singularity`；只有需要改变该位置时才设置 `SINGULARITY_HOME`。

## 安全边界

- 工作区写入前进行规范化路径检查；符号链接、junction 和 `..` 不能绕过根目录边界。
- `.git`、`.singularity`、环境文件、密钥和其他受保护路径默认拒绝模型写入。
- 命令默认无网络，且必须经过 restricted token、Job Object、进程树终止、超时和有界输出捕获。
- 网络被拒绝时只接受 elevated offline identity；不能执行时直接失败，不使用本地进程或无沙箱后端。
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

完整 release 构建：

```powershell
cargo build --workspace --bins --release --locked
```

## 许可证

项目主体使用 [MIT License](LICENSE)。`crates/windows-sandbox` 的来源和 Apache-2.0 许可见 [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) 与该 crate 内的 `UPSTREAM.md`、`LICENSE`、`NOTICE`。
