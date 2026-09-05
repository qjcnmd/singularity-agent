# Singularity

Singularity 是以 Rust 实现的本地 coding-agent harness。无参数启动浏览器工作台；`--print` 与 `--json` 提供同一 Agent 能力的单次自动化入口。Workspace、Session、模型配置和执行状态由本机 Host 统一管理，浏览器只负责呈现与控制。

当前发布目标为 Windows x86-64。

## 产品结构

```text
singularity（单进程）
  ├─ Web 工作台（默认）
  │    ├─ Workspace / Task 导航
  │    ├─ 连续 Conversation、活动分组与 Details
  │    ├─ resident Composer、模型与思考程度、Steer、Follow-up、Stop
  │    └─ Provider 连接设置与本机目录选择
  └─ --print / --json（单次无交互执行）
       └─ crates/runtime：Turn 与 Session 执行唯一所有者
            └─ AgentLoop + read/glob/grep/bash/edit/write + Provider
```

详细对象、状态与协议边界见 [`docs/singularity.md`](docs/singularity.md)。

## 安装

发布包只有一个运行时程序 `singularity.exe`，另附许可证和说明文件。运行时不需要 Node.js；目标项目需要的 Git、Python、Node.js、Rust 等工具链仍由用户安装并加入 `PATH`。Windows 上必须安装 [Git for Windows](https://git-scm.com/install/windows)，使 `bash.exe` 可被发现。

完整安装和源码构建说明见 [`docs/INSTALL.md`](docs/INSTALL.md)。

## 使用

启动工作台：

```powershell
singularity
```

Host 只监听 `127.0.0.1:3080`，生成一次性入口并交接到默认浏览器。可指定端口或只打印入口：

```powershell
singularity --port 43120
singularity --port 0 --no-open
```

首次使用可在“设置 > 模型连接”中登记兼容 OpenAI Chat 或 Responses 协议的 Provider、模型与 API Key。API Key 是只写字段，页面与 Host 响应只显示脱敏配置状态。随后添加本机 Workspace、创建 Task，并在底部 Composer 的组合选择器中选择当前会话使用的模型与思考程度后提交工作。

运行中可选择影响当前回合或排到当前任务之后；Follow-up 队列支持编辑、立即发送和撤回。会话正文、状态和终态来自持久 ledger，刷新或关闭页面不会停止 Host 中的任务。

无交互入口：

```powershell
singularity --print "审查并修复当前仓库中的失败测试"
singularity --json "修复失败测试" --model provider/model#reasoning
```

`--print` 只输出最终 assistant 文本；`--json` 输出逐行事件并以终态 `summary` 行收尾。`--session <id>` 恢复既有 Thread，`--no-session` 禁用本次持久化。Web 参数不能与无交互参数混用。

## 本地数据与权限

默认用户目录为 `%USERPROFILE%\.singularity`：

- `config.json`：Provider、模型与默认选择；
- `auth.json`：Provider 凭据；
- `workbench.json`：已登记 Workspace；
- `browser-session.key`：浏览器会话签名密钥；
- `sessions/<uuid>.jsonl`：Session 正文与执行 ledger。

浏览器入口使用进程级随机 token 换取绑定当前 `127.0.0.1:<port>` 的签名 HttpOnly cookie，地址随即清理。控制请求要求当前 Host、同源来源和 JSON 内容类型；没有跨源控制接口。

Agent 继承 `singularity.exe` 的本机权限，可读取、编辑文件并运行命令。Workspace 用于项目上下文、Session 分组和文件候选，不是文件系统沙箱。

## 从源码构建

需要 Rust 1.96.0 与 Node.js 24：

```powershell
npm --prefix crates/cli/web ci
npm --prefix crates/cli/web run build
cargo build --release --locked --package singularity_cli --bins
```

前端 production assets 会嵌入 `singularity.exe`。Cargo 输出目录以 `cargo metadata --no-deps --format-version 1` 的 `target_directory` 为准。

## 验证

```powershell
npm --prefix crates/cli/web run build
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings
cargo test --workspace --all-targets --locked --no-fail-fast
cargo build --workspace --bins --locked
git diff --check
```

## 许可证

项目主体使用 [MIT License](LICENSE)。
