# 开发指南

本文介绍从源码构建、本地调试、检查和打包。运行边界和模块职责见[架构说明](architecture.md)，配置文件和数据目录见[安装与配置](INSTALL.md)。

## 1. 本地启动与调试

### 环境要求

- **Rust**：1.96.0+（MSVC 工具链）
- **C++ 构建工具**：Visual Studio Build Tools（勾选 Desktop development with C++）
- **Node.js**：v24.x 与 npm 11.x
- **Git for Windows**

### 首次拉取与完整编译

```powershell
git clone https://github.com/qjcnmd/singularity-agent.git
Set-Location singularity-agent

# 1. 安装前端依赖
npm --prefix apps/desktop ci

# 2. 编译 Rust 后端
cargo build -p app --locked

# 3. 构建前端并复制后端到桌面资源目录
npm --prefix apps/desktop run build
npm --prefix apps/desktop run prepare:runtime

# 4. 启动 Electron 桌面应用
npm --prefix apps/desktop start
```

### 日常迭代流程

Rust 后端启动后直接通过标准输入/输出处理 RPC。

- **纯前端修改**：重新运行 `npm --prefix apps/desktop run build`，在工作台窗口中按 <kbd>Ctrl</kbd> + <kbd>R</kbd> 即可即时看到最新界面变更。
- **Rust 后端修改**：
  1. 在系统托盘右键退出正在运行的 Singularity 实例（同一数据目录仅允许单一实例运行）。
  2. 重新编译后端并拷贝运行时：
     ```powershell
     cargo build -p app --locked
     npm --prefix apps/desktop run prepare:runtime
     npm --prefix apps/desktop start
     ```

### 独立隔离调试环境

若不想污染日常的本地配置与会话，可通过指定 `SINGULARITY_HOME` 环境变量启动独立数据环境：

```powershell
$env:SINGULARITY_HOME = "$PWD/outputs/dev-sandbox"
npm --prefix apps/desktop start
```

## 2. 质量检查与代码格式化

推荐直接使用统一脚本运行与 CI 一致的检查：

```powershell
# 按改动范围执行：rust | protocol | desktop | size | all（默认 all；rust / desktop / all 自动包含 size 检查）
pwsh -File scripts/check.ps1 -Scope rust
pwsh -File scripts/check.ps1 -Scope protocol
pwsh -File scripts/check.ps1 -Scope desktop          # 含编译后端、prepare:runtime 与 E2E
pwsh -File scripts/check.ps1 -Scope desktop -SkipE2E # 仅 lint + 严格构建
pwsh -File scripts/check.ps1 -Scope size             # 仅生产源码 600 行上限与体量摘要
pwsh -File scripts/check.ps1 -Scope all
```

如需分项执行，参考以下完整命令：

### 代码排版与格式化
仓库根目录下配置了统一的 `rustfmt.toml`（行宽上限 110 字符）：

```powershell
# 自动格式化所有 Rust 代码
cargo fmt --all

# 检查格式是否符合规范
cargo fmt --all -- --check
```

### 静态分析与编译检查
```powershell
# 运行 Rust Clippy 静态检查
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings

# 运行前端与桌面主进程 Lint（oxlint）及严格 TypeScript 构建检查
npm --prefix apps/desktop run lint
npm --prefix apps/desktop run build

# 验证跨端协议 TypeScript 声明与 Rust DTO 同步（无漂移）
cargo run -p protocol --features typescript --example export_types --locked
git diff --ignore-cr-at-eol --exit-code -- apps/desktop/src/protocol.generated.ts
```

### 常驻自动化回归测试

```powershell
# 1. Rust 全部常驻测试（含 model/agent 属性测试 + app 后端黑盒不变量集成测试，CI 自动运行）
cargo test --workspace --locked

# 2. 桌面端 Electron + Playwright 冒烟测试（先完成前端构建、cargo build -p app --locked 与 prepare:runtime）
npm --prefix apps/desktop run test:e2e

# 3. 指定打包产物运行完整 4 条 E2E（--exe）或快速打包冒烟（--packaged）
node apps/desktop/e2e/smoke.mjs --exe=<打包目录>/Singularity.exe
node apps/desktop/e2e/smoke.mjs --packaged=<打包目录>/Singularity.exe
```

### 测试保留与清理标准

- **允许长期保留的测试**（`crates/app/tests/`、`apps/desktop/e2e/` 与 `crates/{model,agent}/src/property_tests.rs`）：必须满足其一——① 从外部边界进入的黑盒测试（真实 `singularity.exe` JSON-RPC + 磁盘，或真实 Electron 用户可见流程），只断言**系统级不变量与跨模块契约**（协议合法性、超窗收敛、并发停止/删除不死锁、持久化重启一致、不串流、故障不污染上下文等）；② 针对高组合复杂度纯逻辑的**属性测试（`proptest`）**，用随机输入验证通用铁律（如分帧边界无关、编码角色/工具配对不变量、压缩切块不孤立工具、尾行截断自愈）。两类均**不绑定内部实现拆分、具体策略阈值或参数常数**——后续重构压缩策略、工具实现或内部模块时，测试应基本无需修改且仍能发现真问题。
- **通过后必须清理的测试**：针对局部函数细节、具体参数/阈值（如固定占比阈值、固定重试次数、固定缓存上限）、单次 bug 复现或特定样式动效的临时测试与脚本，验证通过后立即删除，不留在仓库形成常绿维护负担。
- **新增常驻测试前先确认**：在常驻套件里新增独立场景或引入新测试依赖前，先说明覆盖哪条系统不变量及代价，用户同意后再加。

## 3. 跨端协议同步 (TypeScript DTO)

`protocol` crate 定义前后端通信的 DTO 与 RPC 方法。修改 `crates/protocol/src/` 中的结构体或枚举后，需要重新导出 TypeScript 类型声明：

```powershell
# 自动重新生成 apps/desktop/src/protocol.generated.ts
cargo run -p protocol --features typescript --example export_types --locked
```

导出后请通过 `git status` 和 `git diff --ignore-cr-at-eol --exit-code -- apps/desktop/src/protocol.generated.ts` 确认生成的前端类型定义，并检查前端调用方的类型匹配。

## 4. 现场验证

### 数据目录与测试桩边界
- **自动化测试隔离**：常驻自动化测试一律使用临时隔离的 `SINGULARITY_HOME`（测试结束自动清理临时目录及派生的 Electron `userData`），不碰用户日常数据。
- **真实模型验收**：生产代码与功能现场验收只用真实的提供方和模型，不写模拟提供方。针对真实模型的现场验收直接使用用户的 `~/.singularity`，启动工作台前退出占用它的实例（单实例锁）；验证结束后清理本次创建的项目、任务及附件，保留用户原有数据。
- **测试桩范围**：仅在测试目录内（`crates/app/tests/`、`apps/desktop/e2e/`）允许使用本地 HTTP SSE 回放/故障注入桩与测试用 MCP stdio 脚本，进程内部不做 Mock。
- **临时文件**：一次性验证脚本、调查脚本、不符合保留标准的临时测试和临时数据用完即删，不留在仓库。

### Windows 自动化环境排障
- **继承完整环境**：Playwright / Electron 自动化须从普通 PowerShell 启动 Node 验证脚本，让 Electron 和 Rust 继承完整的 Windows 环境，不要沿用 `node_repl` 这类裁剪过的环境。若已安装 Git Bash 却报找不到，先检查验证进程的 `ProgramFiles`、`PATH` 与实际 `bash.exe` 路径，修正启动方式后再重跑。
- **清理 `ELECTRON_RUN_AS_NODE`**：从 Electron 宿主进程（如部分自动化代理）启动打包产物前，先清除环境变量 `ELECTRON_RUN_AS_NODE`；否则宿主注入的该变量会让 exe 以 Node 模式静默退出（退出码 0），表现为“启动不了”。真实用户双击运行不受影响。

### 验收范围与专项检查
- 功能完成后，在构建后的 production 版 Electron 工作台中用 Playwright 实际操作本次改动涉及的功能，按影响检查结果、出错、停止、重复操作和重读历史后的表现，只测受影响部分。
- 布局和动效的观感由用户判断：交付时说明要看哪个界面、怎么操作、看什么。
- 公式渲染和样式共用 `apps/desktop/package.json` 中指定的 KaTeX 版本（`overrides` 同时约束 Markdown 插件的传递依赖），更新时检查行内公式、独立公式、矩阵和错误公式；Mermaid 图同样固定版本，更新时检查流程图、时序图、语法错误的图和横向很宽的图；同时在系统“减少动态效果”开启时检查一次（Playwright 默认按未开启模拟，需设 `reducedMotion: 'reduce'`），这时全局过渡样式会让刚改的尺寸读回旧值。

## 5. 打包构建与发布

日常只替换运行目录中的 `resources/app.asar` 和改动涉及的 `resources/runtime/singularity.exe`。首次交付、Electron、运行依赖或打包配置变化时，才执行下面的完整打包。

### 本地打包免安装产物

Windows 包通过 `electronLanguages` 只保留简体中文（`zh-CN`）和英文（`en-US`）的 Electron 语言资源。

```powershell
# 构建 Release 版本后端与前端
npm --prefix apps/desktop run build
cargo build --release --locked --package app --bins
npm --prefix apps/desktop run prepare:runtime -- release

# 使用 Electron Builder 生成免安装绿色目录（--config.directories.output 可选指定输出目录）
npm --prefix apps/desktop run package -- --config.directories.output=<输出目录>
```

产物输出于 `<输出目录>/win-unpacked/`。退出正在运行的 Singularity 后替换目标运行目录，从运行目录验证通过后删除临时打包产物，只保留一份运行包。用户数据保留在 `~/.singularity`。本机具体的运行目录、输出路径与构建缓存清理阈值见本机 `AGENTS.local.md`（如有）。

### CI
- **CI 工作流** (`.github/workflows/ci.yml`)：推送到 `main`，以及目标分支为 `main` 的 pull request，运行生产源码 600 行上限检查、前端 Lint 与严格构建、Rust 格式化检查、跨端协议声明同步校验、Clippy、编译、属性测试与后端黑盒不变量测试，以及依赖安全检查。
