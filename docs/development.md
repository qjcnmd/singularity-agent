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

### 应用回归测试

CI 会执行现有的应用回归测试，检查历史读取失败后，停止等待是否正常结束：

```powershell
cargo test -p singularity_app --locked
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

## 4. 桌面 E2E 测试

项目在 `apps/desktop/e2e/` 下提供了基于真实 Electron 窗口的端到端自动化测试脚本。

### 运行冒烟测试 (Smoke Test)

E2E 默认使用日常数据目录，运行前先退出占用该目录的工作台；测试创建的项目与任务会保留在列表中。需要隔离时，把 `SINGULARITY_HOME` 指向另一个包含 `config.json` 与 `auth.json` 的目录。

测试按完整目录复用已登记的项目，同名项目按各自所在行定位。冒烟测试首次添加项目时检查目录选择，重跑时检查取消选择并复用已有项目。

```powershell
$env:SINGULARITY_HOME = "$HOME/.singularity"

# 运行冒烟测试
node apps/desktop/e2e/smoke.mjs
```

### 专项测试脚本

- **生命周期与重启恢复**：`node apps/desktop/e2e/lifecycle.mjs`（验证退出时取消任务，以及重启后的历史状态）
- **压缩执行与恢复**：`node apps/desktop/e2e/compaction.mjs`（验证手动与自动的成功、失败、无内容结果、继续执行、重复操作与刷新恢复；临时提供方使用已有真实端点和凭据，检查后移除）
- **统计与用量指标**：`node apps/desktop/e2e/stats-bar.mjs`（验证 Token 用量、输出速度、首 Token 延迟与缓存命中率）
- **多模态图片管线**：`node apps/desktop/e2e/images.mjs`（验证图片粘贴、拖拽、缩略图预览与模型识图）

| 脚本 | 真实模型调用条件 |
| --- | --- |
| `smoke.mjs`、`images.mjs` | 设置 `$env:SINGULARITY_E2E_MODEL = '1'` 后才调用模型；省略时只检查本地交互 |
| `lifecycle.mjs`、`stats-bar.mjs` | 始终调用真实模型，不受 `SINGULARITY_E2E_MODEL` 控制 |
| `compaction.mjs` | 始终调用真实模型；失败场景由同一真实提供方拒绝不存在的模型，测试结束移除临时配置 |

测试从 `SINGULARITY_HOME` 读取配置和密钥。模型选择由 `SINGULARITY_E2E_SELECTOR` 指定，省略时固定使用 `bai/deepseek-v4.1-flash`；不会读取配置中的 `default_model` 来替换该选择。

```powershell
# 指定测试模型及思考档位；提供方、模型和档位必须已登记在该数据目录的配置中
$env:SINGULARITY_E2E_SELECTOR = 'provider/model#reasoning'
```

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

### CI 与 GitHub Releases
- **CI 工作流** (`.github/workflows/ci.yml`)：推送到 `main` 时运行 Rust 格式化检查、Clippy、编译、应用回归测试及依赖检查。
- **发布工作流** (`.github/workflows/release.yml`)：当推送以 `v*` 开头的语义化 Git Tag（如 `v1.2.0`）时，自动编译 Windows x86-64 二进制与绿色分发包，并生成包含 SHA256 校验和的 GitHub Release。
