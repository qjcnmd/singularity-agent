# 开发与调试指南

本文介绍 Singularity 的本地开发、调试、测试与发布流程。
产品定位与边界见[宪章](constitution.md)，完整运行流程见[架构图](../architecture.html)，模块职责与源码入口见[架构说明](architecture.md)，安装配置见[安装指南](INSTALL.md)。

---

## 1. 本地启动与调试

### 首次拉取与完整编译

在仓库根目录下运行以下命令，完成前端依赖安装与完整启动准备：

```powershell
# 1. 安装前端依赖
npm --prefix apps/desktop ci

# 2. 编译 Rust 后端
cargo build -p singularity_app --locked

# 3. 构建前端并复制后端到桌面资源目录
npm --prefix apps/desktop run build
npm --prefix apps/desktop run prepare:runtime

# 4. 启动 Electron 桌面应用
npm --prefix apps/desktop start
```

### 日常迭代流程

Rust 后端启动后直接通过标准输入/输出处理 RPC，Electron 启动它时不传模式参数。

- **纯前端修改**：重新运行 `npm --prefix apps/desktop run build`，在工作台窗口中按 <kbd>Ctrl</kbd> + <kbd>R</kbd> 即可即时看到最新界面变更。
- **Rust 后端修改**：
  1. 在系统托盘右键退出正在运行的 Singularity 实例（同一数据目录仅允许单一实例运行）。
  2. 重新编译后端并拷贝运行时：
     ```powershell
     cargo build -p singularity_app --locked
     npm --prefix apps/desktop run prepare:runtime
     npm --prefix apps/desktop start
     ```

### 独立隔离调试环境

若不想污染日常的本地配置与会话，可通过指定 `SINGULARITY_HOME` 环境变量启动独立数据环境：

```powershell
$env:SINGULARITY_HOME = "$PWD/outputs/dev-sandbox"
npm --prefix apps/desktop start
```

---

## 2. 质量检查与代码格式化

检查范围按 [仓库指令](../AGENTS.md#验证与交付) 中本次改动的影响确定。以下命令供对应检查使用。

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
# 运行 Clippy 静态检查
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings

# 检查所有二进制与库的编译
cargo check --workspace --all-targets
```

### 回归测试
```powershell
cargo test --workspace --locked
npm --prefix apps/desktop test
```

---

## 3. 跨端协议同步 (TypeScript DTO)

Rust 的 `protocol` crate 维护了后端与前端通信的核心 DTO 与 RPC 方法契约。当修改了 `crates/protocol/src/` 中的结构体或枚举后，需要重新导出 TypeScript 类型声明：

```powershell
# 自动重新生成 apps/desktop/src/protocol.generated.ts
cargo run -p singularity_protocol --features typescript --example export_types
```

导出后请通过 `git status` 确认生成的前端类型定义，并检查前端调用方的类型匹配。

---

## 4. 现场验证

功能完成后，在构建后的 Electron 工作台中实际操作本次改动涉及的功能。按影响检查结果、出错、停止、重复操作和重读历史后的表现，只检查受影响部分。

数据目录、模型调用和临时文件处理按[仓库指令](../AGENTS.md#验证与交付)执行。

---

## 5. 打包构建与发布

### 本地打包免安装产物

```powershell
# 构建 Release 版本后端与前端
npm --prefix apps/desktop run build
cargo build --release --locked --package singularity_app --bins
npm --prefix apps/desktop run prepare:runtime -- release

# 使用 Electron Builder 生成免安装绿色目录
npm --prefix apps/desktop run package
```

产物输出于 `apps/desktop/release/win-unpacked/`，包含 `Singularity.exe` 与完整运行时依赖。

### CI
- **CI 工作流** (`.github/workflows/ci.yml`)：推送到 `main` 时运行 Rust 格式化检查、Clippy、编译、Rust 与前端同步测试及依赖检查。
