# 安装与运行

Singularity 当前发布目标为 Windows x86-64。发布包的运行时只有 `singularity.exe`；Node.js 只参与源码构建，不是运行依赖。

## 安装发布包

1. 解压发布归档。
2. 将其中 `singularity.exe` 所在目录加入 `PATH`。
3. 安装 [Git for Windows](https://git-scm.com/install/windows)。程序先检查标准 Git 安装目录，再查找 `PATH` 中的 `bash.exe`；自定义安装位置可将 Git 的 `bin` 目录加入 `PATH`，不使用 Windows 的 WSL 启动器代替 Git Bash。
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

## 启动工作台

```powershell
singularity
```

默认监听 `127.0.0.1:3080` 并打开系统默认浏览器。端口占用会明确失败；需要系统选择空闲端口或手动打开时使用：

```powershell
singularity --port 0 --no-open
```

终端会打印普通本机地址，例如 `http://127.0.0.1:3080/`，可直接打开或收藏，不需要 token、登录或浏览器授权。程序只监听本机；控制请求仍校验 Host 与来源。

工作台内的基本流程是：

1. 在“设置 > 模型连接”中登记 Provider、模型、协议和 API Key；
2. 添加一个存在的本机目录作为 Workspace；
3. 创建或恢复 Task；
4. 在输入框右侧选择当前任务的模型与思考程度并发送；运行中按 Enter 排队，Ctrl/Cmd+Enter 插话；
5. 在主栏阅读回答并展开工具结果，在右侧“轨迹”查看请求、用量与错误详情。

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

直接配置时，`auth.json` 与上例的 Provider ID 对应，格式如下；将占位值替换为自己的密钥：

```json
{
  "schema_version": 1,
  "providers": {
    "example": {"api_key": "YOUR_API_KEY"}
  }
}
```

配置文件只保存模型元数据，密钥只放在认证文件中；不要把认证文件提交到项目仓库。

## 项目指令

程序先读取用户数据目录的 `AGENTS.md`，再从任务 cwd 向上找到最近的 Git 根，按项目根到 cwd 的顺序读取各级 `AGENTS.md`；没有 Git 根时以任务目录为根。工作台任务通常以已登记项目目录作为 cwd。

文件内容带来源进入模型上下文，在每个模型步核对变化。单文件最多读取 32 KiB，合并最多 64 KiB，超出时保留预算内前缀并反馈截断；读取失败会明确报错。Skills 用于按需加载专项说明，与这些持续生效的文件指令分开。

## Skills

技能使用带 YAML 元数据的 Markdown 文件，可放在 `技能名/SKILL.md` 或 `技能名.md`。查找顺序如下，同名时靠前的目录优先：

1. 项目根目录的 `.singularity/skills/`；
2. 项目根目录的 `.agents/skills/`；
3. 用户数据目录的 `skills/`，默认 `%USERPROFILE%\.singularity\skills\`；
4. 默认用户环境下的 `%USERPROFILE%\.agents\skills\`。

项目根取 cwd 向上的最近 Git 根，没有 Git 时使用任务目录。显式设置 `SINGULARITY_HOME` 时，用户级技能只从该数据目录加载，适合隔离测试。示例文件：

```markdown
---
name: review
description: 检查代码改动和相关验证结果
---
先阅读改动与相关实现，核实实际影响，再报告有证据的问题。
```

在输入框输入 `/` 查看技能，继续输入按名称前缀筛选；用上下键移动，Enter、Tab 或鼠标选中。选中只填入命令，发送以 `/review` 开头的消息后才加载完整正文。模型会看到技能名称和说明，并可通过 `skill` 工具读取适用技能。

可选元数据 `user-invocable: false` 隐藏手动入口，`disable-model-invocation: true` 禁止模型主动调用；两者默认分别为 `true`、`false`。目录在每个 turn 开始时发现，正文在调用时重新读取，并附带文件来源与相对资源目录。格式错误会指出文件，不影响其他有效技能；技能中的脚本不会因加载而自动执行。

## 评估入口

```powershell
singularity --json "完成一项可验证的修改" --model example/model#high
```

- `--json` 输出 JSONL 事件并以终态 `summary` 行收尾；
- `--model <selector>` 选择本次评估的模型，省略时使用默认模型；
- 每次新建并保存会话，评估器通过 `SINGULARITY_HOME` 隔离数据。

评估器负责超时和进程终止。正常返回时，退出码 0 表示完成，130 表示模型执行中断，1 表示失败；失败原因写入 stderr。进程被外部终止时可能没有终态 summary，已保存的会话仍可供评估器读取。

## 数据、更新与卸载

默认数据目录为 `%USERPROFILE%\.singularity\`，与启动时所在目录无关。设置 `SINGULARITY_HOME` 为绝对路径后，配置、凭据、项目登记、会话和用户级指令均使用该目录；技能的额外查找规则见上文。

| 路径 | 内容 |
| --- | --- |
| `config.json` | Provider、模型元数据与默认选择 |
| `auth.json` | Provider 凭据 |
| `workbench.json` | 已登记项目目录 |
| `sessions/<uuid>.jsonl` | 会话正文、控制记录、请求观测与终态 |
| `sessions/archived/` | 已归档会话 |
| `AGENTS.md`、`skills/` | 用户级文件指令与技能 |

草稿、主题、侧栏和阅读位置保存在浏览器本地存储中，不在上述目录内。更换浏览器或端口会使用不同的浏览器存储。

超长 bash 输出保存在系统临时目录的 `singularity-tool-output/<uuid>/<命令名>.log`，工具结果会给出完整路径。Unix 使用 0700 目录和 0600 文件，Windows 继承用户临时目录 ACL。新建输出文件时清理超过七天的旧输出；保存失败会显示原因，不提供不完整文件的路径，也不改写命令本身的退出状态。

更新前退出需要替换的程序，再替换 `singularity.exe`。备份会话时先退出使用该数据目录的实例，再复制整个数据目录；备份包含凭据，应保留其私密性。卸载只需删除程序并从 `PATH` 移除，用户数据和浏览器存储不会自动删除。移除已登记项目不删除项目文件或会话，归档只把会话移出活动列表。
