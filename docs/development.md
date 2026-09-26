# 开发与验证

安装依赖和构建发布程序见 [安装与运行](INSTALL.md)。产品方向见 [宪章](constitution.md)，模块关系、运行流程与源码入口见 [架构图谱](architecture.md)。

## 本地运行

在仓库根目录构建 Rust 后端和桌面资源：

```powershell
npm --prefix apps/desktop ci
cargo build -p singularity_app --locked
npm --prefix apps/desktop run build
npm --prefix apps/desktop run prepare:runtime
npm --prefix apps/desktop start
```

Electron 加载 Vite production 资源，启动私有 stdio Rust 子进程，不启动开发 HTTP 服务。前端修改后重新 build 并刷新窗口；后端或 Electron 修改后退出托盘应用、重新构建并启动。独立验证设置 `SINGULARITY_HOME`，并按验证需要复制配置与凭据；交付沿用用户当前数据目录。

同一数据目录只运行一个 Rust 程序。更新工作台时先确认任务空闲，再从托盘退出旧程序并启动新版本。可试用产物为 `apps/desktop/release/win-unpacked/Singularity.exe`，必须保留同目录所有资源；不再使用单独的 `target/singularity.exe` 作为工作台入口。

关闭窗口隐藏到托盘；托盘退出关闭 stdin，Rust 取消任务并等待结算。后端十秒内未退出时 Electron 终止子进程，已持久化历史仍保留；未完成回合按现有恢复规则处理。

评估入口复用同一 Agent：

```powershell
cargo run -p singularity_app --locked -- --json "summarize this repository"
```

该命令会调用已配置模型，每次创建并保存新会话。评估器通过 `SINGULARITY_HOME` 隔离配置与会话，具体配置见安装说明。

页面状态回归可使用本地模拟 Provider，在独立数据目录中验证触发、流式更新、完成、刷新和再次使用；这不替代真实模型验证。模型选择及调用范围按项目指令执行，临时提供商、会话和进程在验证后清理。

## 检查

检查范围按 [项目指令](../AGENTS.md#验证与交付) 选择。日常从真实入口执行 E2E，覆盖受影响的操作、反馈与持久结果；完成后保存可重复执行、可检查结果的验证产物。保留的隔离测试只用于 E2E 无法检出的具体故障，必要时用包名和测试名过滤，例如在仓库根目录运行：

```powershell
cargo test -p singularity_runtime --lib --locked operation_start_is_durable_before_the_provider_call_and_terminal_after
```

将示例中的包名和过滤条件换成受影响的行为，确认实际选中了用例。只删测试时确认剩余测试可编译，并运行受影响的保留用例；不因此重跑无关模块。普通文档检查最终内容、链接与 `git diff --check`。CI 和发布步骤由 `.github/workflows` 维护，不作为日常修改的默认验证清单。

## 桌面 E2E

使用已构建的 production 资源，安装 PowerShell 7 后在仓库根目录运行：

```powershell
$env:SINGULARITY_HOME = "$PWD/outputs/desktop-e2e/home"
New-Item -ItemType Directory -Force $env:SINGULARITY_HOME | Out-Null
Copy-Item "$HOME/.singularity/config.json", "$HOME/.singularity/auth.json" $env:SINGULARITY_HOME
$env:SINGULARITY_E2E_OUTPUT = "$PWD/outputs/desktop-e2e"
$env:SINGULARITY_E2E_MODEL = '1'
$env:SINGULARITY_E2E_EXTENDED = '1'
$env:SINGULARITY_E2E_CANCEL = '1'
$env:SINGULARITY_E2E_PACKAGED = "$PWD/apps/desktop/release/win-unpacked/Singularity.exe"
node apps/desktop/e2e/smoke.mjs
```

脚本使用真实 Electron 窗口及 Windows 原生目录对话框，会调用 `bai/deepseek-v4.1-flash`；验证期间不要操作该窗口。输出目录保留 JSON 结果、流事件、会话读取结果与截图。取消 `SINGULARITY_E2E_PACKAGED` 可验证源码启动；不设置模型和扩展标记时只检查基础桌面行为。Playwright 自身使用的调试连接不属于产品通信；无监听验证须另用普通启动的发布程序执行。`node apps/desktop/e2e/lifecycle.mjs` 使用同一组环境变量验证刚提交任务时退出、重启读取中断历史及错误协议版本。测试完成后删除隔离目录中的凭据副本。

两条 E2E 的启动、环境检查、模型默认值和 RPC 调用由 `apps/desktop/e2e/support.mjs` 维护；各脚本独立管理验证流程和应用实例。CI 的两个检查 job 在检出仓库后共用 `.github/actions/isolated-environment` 配置隔离的工具目录。

桌面 PNG 与 ICO 图标位于 `apps/desktop/resources`，与 `public/favicon.svg` 一同维护；修改图标时同步更新这些资源，构建流程不自动生成图标。

## 协议更新

Rust 的 `protocol` crate 维护 RPC 方法与 DTO。修改协议后，在仓库根目录运行以下命令更新客户端声明，再检查生成 diff：

```powershell
cargo run -p singularity_protocol --features typescript --example export_types
cargo test -p singularity_protocol --features typescript --locked
```

事件形状由协议测试中的逐事件 golden 覆盖。序列化 fixture 位于 `crates/protocol/tests/fixtures/`，覆盖流信封和 RPC 响应；仅在有意改变相应合同后，设置 `UPDATE_PROTOCOL_FIXTURES=1` 运行协议测试并检查 JSON 差异，普通测试只核对 fixture。协议测试同时校验生成的 TypeScript 声明与 Rust 合同逐字节一致，消费前端不再另设测试。

## 测试保留与删减

强烈优先把 E2E 作为唯一测试机制：运行工作台或 CLI，从实际入口触发行为，检查响应、界面状态和持久结果；复杂功能验证完整操作过程，需要模型参与的行为调用模型。每次 E2E 完成后生成可核验、可重复的产物，记录版本或构建、前置条件、输入、操作步骤或命令、预期与实测结果，并保留可检查的输出、日志或画面及其位置。

绝不在编写实现代码后补写单元测试。只有具体的真实故障无法由 E2E 检出时，才使用最少必要的隔离测试；先按该系统当前范围写下可能失败的所有方式及 E2E 遗漏原因，再编写测试与实现代码。现有测试也逐项按此标准保留，接口或格式变化时只维护有价值的覆盖，不顺带增加场景或断言。调查用脚本和临时数据在完成验证后清理。

CI 继续运行保留下来的测试，作为 E2E 难以检出故障的回归检查。

## 测试组织

采用 [Rust 的测试组织约定](https://doc.rust-lang.org/book/ch11-03-test-organization.html)：只使用 crate 公共接口的独立集成测试放在 crate 根下的 `tests/`，跨模块但需要内部接口的行为测试集中在各 crate 的 `src/tests/`。

- Runtime 与应用层 中涉及多个模块的行为测试集中在各自的 `src/tests/`。
- 协议的外部契约测试使用 `crates/protocol/tests/`，由 Cargo 自动发现；不把内部模块伪装成此类 target。
- 跨 crate 使用的测试夹具留在拥有相应能力的模块，由 `test-support` feature 开启。只供单个测试组使用的辅助代码与该组放在一起，不增加全仓测试工具包。
- 符合上一节例外而确需新增的隔离测试，按行为或契约放入对应测试组。

## CI 与发布

[CI 入口](../.github/workflows/ci.yml) 在推送 `main` 时调用 [共享检查工作流](../.github/workflows/rust-gates.yml)。Windows 任务执行前端构建、Rust 格式、Clippy、测试和二进制构建；Ubuntu 任务只运行 cargo-deny 与前端生产依赖审计，不编译或验证 Linux 产品。依赖检查复用同一锁文件与策略，保留 Ubuntu 执行器不代表支持 Linux。工具版本和具体步骤由工作流维护。

需要在本地复现完整功能检查时，在安装依赖后执行：

```powershell
npm --prefix apps/desktop run build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings
cargo test --workspace --all-targets --features singularity_protocol/typescript --locked --no-fail-fast
cargo build --workspace --bins --locked
git diff --check
```

这组命令不包含独立依赖审计、Electron 交互或真实模型验证；按修改范围选择相应检查，不把完整集合用于每次修改。

[发布工作流](../.github/workflows/release.yml) 先复用检查，再构建 Windows x86-64 release 程序。`desktop/prepare.mjs` 从 `cargo metadata.target_directory` 查找 Rust 后端并复制到桌面资源目录；Electron Builder 生成 `apps/desktop/release/win-unpacked`，发布脚本将该目录连同 README、LICENSE 和 INSTALL 归档，另生成 SHA256 校验和。推送 `v*` 标签会发布 GitHub Release；手动运行只生成工作流产物。源码构建命令由 [安装说明](INSTALL.md#从源码构建) 维护。

## 可选评估工具

本机评估器位于 `C:/Users/Lenovo/Desktop/Singularity-Evaluator`，通过 `singularity --json` 在隔离工作区运行任务。任务、checker、参数和配置由评估器仓库维护。

是否使用见 [项目指令](../AGENTS.md#验证与交付)。模型、任务和预算按本次明确要求选择。
