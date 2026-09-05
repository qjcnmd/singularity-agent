# 安装与运行

Singularity 当前发布目标为 Windows x86-64。发布包的运行时只有 `singularity.exe`；Node.js 只参与源码构建，不是运行依赖。

## 安装发布包

1. 解压发布归档。
2. 将其中 `singularity.exe` 所在目录加入 `PATH`。
3. 安装 [Git for Windows](https://git-scm.com/install/windows)，确认 `bash.exe` 可从 `PATH` 发现。
4. 按目标项目需要安装 Python、Node.js、Rust 等工具链。

验证安装：

```powershell
singularity --help
```

## 从源码构建

需要：

- Rust 1.96.0 MSVC toolchain；
- Visual Studio Build Tools 的 Desktop development with C++；
- Node.js 24 与 npm 11；
- Git for Windows。

```powershell
git clone https://github.com/qjcnmd/singularity-agent.git
Set-Location singularity-agent
npm --prefix crates/cli/web ci
npm --prefix crates/cli/web run build
cargo build --release --locked --package singularity_cli --bins
$metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
Get-Item (Join-Path $metadata.target_directory 'release/singularity.exe')
```

Vite 生成的 production assets 被 Rust 构建嵌入程序。复制 `singularity.exe` 到没有仓库和 Node.js 的目录后仍可完整运行工作台。

前端确定性回归使用 Node.js 24 内置测试运行器，直接加载 Store 与时间线的生产代码：

```powershell
npm --prefix crates/cli/web test
npm --prefix crates/cli/web audit --omit=dev --audit-level=high
```

CI 在 Linux 和 Windows 上运行前端回归、类型检查与打包，以及 Rust 格式、Clippy、测试和构建；独立依赖检查包含 Rust 策略审计与前端生产依赖的高危、严重漏洞门禁。前端测试覆盖快照与事件时序、停止状态、跨 Workspace 选择、草稿及时间线投影；浏览器交互和真实模型验收另行执行。

## 启动工作台

```powershell
singularity
```

默认监听 `127.0.0.1:3080` 并打开系统默认浏览器。端口占用会明确失败；需要系统选择空闲端口或手动打开时使用：

```powershell
singularity --port 0 --no-open
```

终端会打印含一次性 token 的入口。浏览器打开后取得绑定当前地址的签名 HttpOnly cookie，并跳转到不含 token 的根地址。同一地址重启后，签名密钥允许现有浏览器会话继续使用。

工作台内的基本流程是：

1. 在“设置 > 模型连接”中登记 Provider、模型、协议和 API Key；
2. 添加一个存在的本机目录作为 Workspace；
3. 创建或恢复 Task；
4. 在 Composer 右侧选择当前 Task 的模型与思考程度并提交任务，运行中按需选择 Steer 或 Follow-up；
5. 在 Conversation 中阅读最终回答，把工具细节按需展开或放到 Details 查看。

Agent 使用当前进程的完整本机权限。Workspace 限定项目上下文、Session 分组和文件候选，不限制命令或工具可访问的路径。

## Provider 配置

“设置 > 模型连接”管理 Provider 地址、协议、模型元数据与 API Key；Composer 发送按钮旁的组合选择器管理当前 Task 的模型和思考程度。也可直接维护 `%USERPROFILE%\.singularity\config.json` 和私有认证文件 `auth.json`。每个模型必须显式声明 `api_protocol: chat|responses`，selector 形如 `provider_id/model_id#variant`。

```json
{
  "version": 1,
  "default_provider": "example",
  "default_model": "example/model#high",
  "providers": {
    "example": {
      "base_url": "https://api.example.com/v1",
      "models": {
        "model": {
          "api_protocol": "chat",
          "max_context_tokens": 128000,
          "max_output_tokens": 8192,
          "reasoning_variants": {
            "high": {"enabled": true, "wire_effort": "high"}
          },
          "default_variant": "high"
        }
      }
    }
  }
}
```

API Key 通过“模型连接”或 `auth.json` 按 Provider 保存。工作台响应、日志和模型目录投影不会返回凭据。

## 无交互模式

```powershell
singularity --print "检查当前项目并修复一个明确问题"
singularity --json "完成一项可验证的修改" --model example/model#high
```

- `--print` 只向 stdout 输出最终 assistant 文本；
- `--json` 输出 JSONL 事件并以终态 `summary` 行收尾；
- `--model <selector>` 只覆盖本次执行；
- `--session <id>` 恢复既有 Thread；
- `--no-session` 禁用本次持久化。

第一次 Ctrl+C 中断当前 turn，第二次强制退出。退出码 0、130、1 分别表示 completed、interrupted、failed。

## 数据、更新与卸载

持久状态集中在 `%USERPROFILE%\.singularity\`。更新时替换 `singularity.exe`；卸载时删除程序目录并从 `PATH` 移除。用户状态不会自动删除。

## 完整验证

```powershell
npm --prefix crates/cli/web run build
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings
cargo test --workspace --all-targets --locked --no-fail-fast
cargo build --workspace --bins --locked
git diff --check
```
