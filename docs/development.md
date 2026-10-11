# 开发与验证

本文提供本地运行、检查与发布步骤。开发、验证与交付规则由[项目指令](../AGENTS.md)维护，安装依赖和构建发布程序见[安装与运行](INSTALL.md)。产品方向见[宪章](constitution.md)，技术取舍见[ADR 导航](constitution.md#技术决策导航)，模块关系、运行流程与源码入口见[架构图谱](architecture.md)。

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

页面回归的范围、模型调用、数据隔离与清理按[项目指令](../AGENTS.md#验证与交付)执行。下面的检查和 E2E 命令用于复现所需行为。

## 检查

检查范围、执行成本、产物和停止条件见[项目指令](../AGENTS.md#验证与交付)。本指南提供各检查的运行入口；CI 和发布步骤由 `.github/workflows` 维护。

## 代码排版

Rust 使用仓库根目录的 `rustfmt.toml`，行宽上限为 110 字符，调用和链式表达式的阈值为 90，结构体字面量的阈值为 40。简单调用和构造器优先单行；较长链式表达式、多字段构造器及复杂嵌套分行展示。

在仓库根目录格式化并检查：

```powershell
cargo fmt --all
cargo fmt --all -- --check
```

## 桌面 E2E

需要完整桌面回归时，使用已构建的 production 资源，安装 PowerShell 7 后在仓库根目录运行以下示例。日常修改按[检查](#检查)选择检查范围，不默认执行这组模型、扩展与取消场景：

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

脚本使用真实 Electron 窗口及 Windows 原生目录对话框；上述模型场景会使用[项目指令](../AGENTS.md#验证与交付)指定的默认模型，可通过 `SINGULARITY_E2E_SELECTOR` 指定本次选择；验证期间不要操作该窗口。输出目录保留 JSON 结果、流事件、会话读取结果与截图。取消 `SINGULARITY_E2E_PACKAGED` 可验证源码启动；`smoke.mjs` 不设置模型和扩展标记时只检查基础桌面行为。Playwright 自身使用的调试连接不属于产品通信；无监听验证须另用普通启动的发布程序执行。`node apps/desktop/e2e/lifecycle.mjs` 使用同一组环境变量验证刚提交任务时退出、重启读取中断历史。`node apps/desktop/e2e/stats-bar.mjs` 验证用量与统计栏，缓存命中率按实际上报的明细核对；这两条脚本均会调用模型。测试完成后删除隔离目录中的凭据副本。

`node apps/desktop/e2e/images.mjs` 检查图片选择、粘贴、拖放、草稿刷新、预览关闭与损坏图片的明确错误；设置 `SINGULARITY_E2E_MODEL=1` 后，使用真实模型检查上传识图、`read` 识图及删除原文件后重启继续识图。图片引导、停止等其他操作按本次变更在运行的 production Electron 工作台中直接验证。涉及模型调用时，使用[项目指令](../AGENTS.md#验证与交付)指定的真实模型。

桌面 E2E 的启动、环境检查、模型默认值和 RPC 调用由 `apps/desktop/e2e/support.mjs` 维护；各脚本独立管理验证流程和应用实例。CI 使用托管运行器的标准工具目录，并缓存 Cargo 依赖与检查工具。

桌面 PNG 与 ICO 图标位于 `apps/desktop/resources`，与 `public/favicon.svg` 一同维护；修改图标时同步更新这些资源，构建流程不自动生成图标。

## 协议更新

Rust 的 `protocol` crate 维护 RPC 方法与 DTO。修改协议后，在仓库根目录运行以下命令更新客户端声明，再检查生成 diff：

```powershell
cargo run -p singularity_protocol --features typescript --example export_types
```

`--json` 的对外事件和终态形状由协议 DTO 与序列化规则维护；客户端声明从同一组 Rust DTO 生成。私有桌面 RPC 与流信封通过 Electron E2E 检查实际请求、反馈和恢复结果。

## 测试保留与删减

新增、保留和删减测试的条件统一见[项目指令](../AGENTS.md#验证与交付)。桌面行为的复现入口见[桌面 E2E](#桌面-e2e)，协议声明的生成入口见[协议更新](#协议更新)。

## CI 与发布

[CI 入口](../.github/workflows/ci.yml) 在推送 `main` 时调用 [共享检查工作流](../.github/workflows/rust-gates.yml)。Windows 任务执行前端构建、Rust 格式、Clippy 和二进制构建；Ubuntu 任务只运行 cargo-deny 与前端生产依赖审计，不编译或验证 Linux 产品。依赖检查复用同一锁文件与策略，保留 Ubuntu 执行器不代表支持 Linux。工具版本和具体步骤由工作流维护。

需要在本地复现完整功能检查时，在安装依赖后执行：

```powershell
npm --prefix apps/desktop run build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings
cargo build --workspace --bins --locked
git diff --check
```

这组命令不包含独立依赖审计、Electron 交互或真实模型验证；按修改范围选择相应检查，不把完整集合用于每次修改。

[发布工作流](../.github/workflows/release.yml) 先复用检查，再构建 Windows x86-64 release 程序。`desktop/prepare.mjs` 从 `cargo metadata.target_directory` 查找 Rust 后端并复制到桌面资源目录；Electron Builder 生成 `apps/desktop/release/win-unpacked`，发布脚本将该目录连同 README、LICENSE 和 INSTALL 归档，另生成 SHA256 校验和。推送 `v*` 标签会发布 GitHub Release；手动运行只生成工作流产物。源码构建命令由 [安装说明](INSTALL.md#从源码构建) 维护。

## 可选评估工具

本机评估器位于 `C:/Users/Lenovo/Desktop/Singularity-Evaluator`，通过 `singularity --json` 在隔离工作区运行任务。任务、checker、参数和配置由评估器仓库维护。

是否使用见 [项目指令](../AGENTS.md#验证与交付)。模型、任务和预算按本次明确要求选择。
