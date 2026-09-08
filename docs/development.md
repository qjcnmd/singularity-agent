# 开发与验证

安装依赖和构建发布程序见 [安装与运行](INSTALL.md)。产品方向见 [宪章](constitution.md)，运行契约与模块职责见 [架构说明](singularity.md)。

## 本地运行

在仓库根目录生成前端资源，再构建或启动 Rust 程序：

```powershell
npm --prefix crates/cli/web ci
npm --prefix crates/cli/web run build
cargo run -p singularity_cli --locked -- --no-open --port 3080
```

打开终端打印的当前进程启动链接。前端资源嵌入可执行文件；修改页面后需重新生成前端资源并构建 Rust 程序，刷新旧进程无法加载新资源。端口已占用时选择另一个端口，保留已有实例。

无交互入口复用同一 Agent：

```powershell
cargo run -p singularity_cli --locked -- --print "summarize this repository"
cargo run -p singularity_cli --locked -- --json "summarize this repository"
```

这些命令会调用已配置模型。自动化和测试可通过 `SINGULARITY_HOME` 隔离配置与会话，具体配置见安装说明。

## 检查

检查范围按 [项目指令](../AGENTS.md#验证与交付) 选择。可用命令如下：

```powershell
cargo fmt --all -- --check
npm --prefix crates/cli/web test
npm --prefix crates/cli/web run build
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings
cargo test --workspace --all-targets --locked --no-fail-fast
git diff --check
```

Rust 检查可用 `-p` 限定所属包。CI 和发布的具体步骤由 `.github/workflows` 维护。

## 可选评估工具

本机评估器位于 `C:/Users/Lenovo/Desktop/Singularity-Evaluator`，通过 `singularity --json` 在隔离工作区运行任务。任务、checker、参数和配置由评估器仓库维护。

是否使用见 [项目指令](../AGENTS.md#验证与交付)。模型、任务和预算按本次明确要求选择。
