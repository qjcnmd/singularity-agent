# 安装与配置指南

Singularity 是面向 Windows 的本地 Coding Agent，由 Electron 桌面工作台和 Rust 后端组成。安装包自带完整运行时。

---

## 1. 快速开始（发布包）

### 系统要求
- **操作系统**：Windows 10 / 11 (x86-64)
- **必备工具**：[Git for Windows](https://git-scm.com/download/win)（Agent 依赖 `bash.exe` 执行命令与搜索；请确保 `bash.exe` 或 Git `bin` 目录位于系统 `PATH`）
- **可选依赖**：根据被开发项目的需要，安装对应语言工具链（如 Node.js、Python、Rust、`ripgrep` 等）

### 运行
1. 解压发布压缩包到任意本地目录。
2. 双击运行 `Singularity.exe`。
3. 应用通过私有 stdio 管道与本地 Rust 后端通信，无需配置端口或网络代理。
   - **窗口行为**：点击窗口关闭按钮会最小化至系统托盘，后台任务继续执行；右键托盘图标选择“退出”可完全结束程序。
   - **快捷键**：<kbd>Ctrl</kbd> + <kbd>R</kbd> 可重载前端界面，不会中断 Rust 核心进程。

---

## 2. 从源码构建

如需二次开发或自行打包，请准备以下构建环境：
- **Rust**：1.96.0+ (MSVC 工具链)
- **C++ 构建工具**：Visual Studio Build Tools（勾选 Desktop development with C++）
- **Node.js**：v24.x 与 npm 11.x
- **Git for Windows**

```powershell
# 1. 克隆代码仓库
git clone https://github.com/qjcnmd/singularity-agent.git
Set-Location singularity-agent

# 2. 编译前端桌面界面
npm --prefix apps/desktop ci
npm --prefix apps/desktop run build

# 3. 编译 Rust 后端
cargo build --release --locked --package singularity_app --bins

# 4. 拷贝后端产物到桌面资源目录并打包
npm --prefix apps/desktop run prepare:runtime -- release
npm --prefix apps/desktop run package
```

打包完成后，绿色免安装产物位于 `apps/desktop/release/win-unpacked/`。直接拷贝该目录至任意 Windows 机器即可运行。

---

## 3. 模型 Provider 配置

可通过工作台界面的 **“设置 > 模型”** 可视化配置，也可直接编辑数据目录下的配置文件：
- **模型配置与 API Key**：`~/.singularity/config.json`，保存时一起原子替换。文件包含密钥，请勿泄露或提交到版本控制。

模型与 MCP 的 HTTPS 连接使用 Windows 系统信任的证书。企业代理使用自有证书时，将其根证书安装到 Windows 信任存储中。

### 配置文件示例

默认模型只用于空白页的初始选择；首次发送保存该任务的模型，之后沿用它或在任务内切换。

模型容量可通过“获取模型参数”读取提供方返回的值；提供方未返回时手动填写。工作台用档位按钮配置可用思考程度，使用时在模型选择器中切换；`default_variant` 可省略，已有配置中的默认档位仍然有效。

`~/.singularity/config.json`：
```json
{
  "default_model": { "provider": "deepseek", "model": "deepseek-chat", "reasoning": null },
  "providers": {
    "deepseek": {
      "api_key": "sk-your-api-key-here",
      "base_url": "https://api.deepseek.com/v1",
      "api_protocol": "chat",
      "models": {
        "deepseek-chat": {
          "max_context_tokens": 128000,
          "max_output_tokens": 8192
        },
        "deepseek-reasoner": {
          "max_context_tokens": 128000,
          "max_output_tokens": 8192,
          "default_variant": "high",
          "reasoning_variants": ["high"]
        }
      }
    }
  }
}
```

`reasoning_variants` 直接填写提供方接受的档位名称，例如 `["off", "low", "high"]`。仅支持开关的 Chat 模型可填写 `["off", "on"]`。

### `base_url` 规范与常见厂商示例

`api_protocol` 必须在提供方上设置为 `chat` 或 `responses`，该提供方的所有模型统一使用它。模型配置不包含协议字段。

系统会在 `base_url` 后追加 `/chat/completions`、`/responses` 或 `/models`。请填写 API 根地址，按服务商要求保留版本号和自定义路径，不填写完整接口地址：

| 提供方 | 推荐 `base_url` | 协议类型 (`api_protocol`) |
| --- | --- | --- |
| **OpenAI 官方** | `https://api.openai.com/v1` | `chat` 或 `responses` |
| **DeepSeek 官方** | `https://api.deepseek.com/v1` | `chat` |
| **B.AI 代理** | `https://api.b.ai/v1` | `chat` |
| **Ollama (本地)** | `http://localhost:11434/v1` | `chat` |
| **vLLM / LM Studio** | `http://localhost:8000/v1` | `chat` |

### 模型请求格式

不同端点使用的思考和输出字段可在模型设置中指定：

| 字段 | 默认值 | 作用说明 |
| --- | --- | --- |
| `thinking_wire_format` | `reasoning_effort` | 思考模式字段：`reasoning_effort`、`thinking_type` (Anthropic 风格) 或 `enable_thinking` (布尔值) |
| `chat_output_tokens_field` | `max_tokens` | Chat 协议指定输出上限的字段名，可切换为 `max_completion_tokens` |

---

## 4. MCP (Model Context Protocol) 工具

通过 **“设置 > MCP”** 可添加本地（stdio）或远程（Streamable HTTP）服务器。配置持久化于 `~/.singularity/mcp.json`：

```json
{
  "mcpServers": {
    "chrome_devtools": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "chrome-devtools-mcp@latest", "--isolated", "--no-usage-statistics"],
      "enabled": true,
      "startupTimeoutSec": 60,
      "toolTimeoutSec": 120
    },
    "custom_remote": {
      "type": "http",
      "url": "https://mcp.internal.example.com/sse",
      "headers": {
        "Authorization": "Bearer token-here"
      },
      "enabled": false
    }
  }
}
```

- **可省略字段**：参数 `args`、额外环境变量 `env` 和请求头 `headers` 省略时按空值读取；工作目录 `cwd` 省略时使用任务的项目目录。设置接口与文件使用相同规则。
- **生效时机**：配置和开关在下一轮任务开始时生效。工具清单与项目指令一起在每轮开始时加载，同一轮内不变。
- **连接状态**：打开设置只查看已有状态，不会主动连接。每轮开始时复用正常的连接并重新获取工具列表，服务器更新工具后下一轮生效。连接失败时可点击“重试连接”。工具在一轮中途被服务器移除时，调用会直接失败，不会自动重试。
- **环境要求**：本地 stdio 服务需要在系统 PATH 中安装对应的运行环境（如 Node.js 或 Python）。

---

## 5. 项目指令与技能 (Skills)

### 项目指令 (`AGENTS.md`)
Agent 在每轮开始时完整读取指令，同一轮内不变，压缩不会重新读取。按以下顺序合并，越靠后越具体：
1. 用户全局指令：`~/.singularity/AGENTS.md`
2. 项目指令：从项目根目录（含 `.git` 的目录；找不到时就是任务目录）逐级到任务目录，每一级的 `AGENTS.md`

### 技能目录 (Skills)
在输入框开头或空白后输入 `/` 可触发技能候选项，也可直接键入完整名称。正文中的引用同样生效，多个技能用空白分隔，例如 `使用 /code-review /find-simplifications 审查项目`。发送时按当前项目的技能目录，把可由用户调用的 `/技能名` 替换为 `read 技能文件的绝对路径`；界面和草稿保留原文，正文由模型通过 read 工具读取。未知名称、路径以及紧贴标点的 `/技能名，`、`(/技能名)` 都保留原文；`/a/b` 不表示两个技能。

每个技能放在独立子目录，以 `SKILL.md` 为入口，例如 `.agents/skills/code-review/SKILL.md`；只扫描这一层子目录。入口包含 YAML Frontmatter：

```markdown
---
name: code-review
description: 审查代码改动并生成专业建议
---
请仔细阅读当前暂存区的改动与相关实现，核实实际运行逻辑，指出潜在隐患并提出优化建议。
```

**加载优先级**（同名时靠前优先）：
1. 项目根目录 `.singularity/skills/`
2. 项目根目录 `.agents/skills/`
3. 用户全局目录 `~/.singularity/skills/`
4. 用户主目录 `~/.agents/skills/`

---

## 6. 数据存储与目录维护

系统所有配置、凭据与任务会话集中存储于数据根目录：
- **默认路径**：`%USERPROFILE%\.singularity`
- **自定义路径**：设置系统环境变量 `SINGULARITY_HOME` 为指定的绝对路径；未设置或只含空白时使用默认目录。

### 目录结构

| 相对路径 | 说明 |
| --- | --- |
| `config.json` | 模型提供方配置、API Key、模型元数据与默认模型选择 |
| `mcp.json` | MCP 服务器列表、环境变量与开关状态 |
| `workspaces.json` | 已添加的项目及其目录 |
| `sessions/<id>.jsonl` | 每个任务的完整历史（当前格式为 v1） |
| `sessions/images/` | 会话中发送或读取的图片快照文件 |
| `skills/` | 用户级全局技能脚本与说明 |

### 数据备份与升级
- **备份**：退出所有 Singularity 进程后，直接复制整份 `.singularity` 目录即可完成备份。
- **卸载**：直接删除应用解压目录即可；用户数据目录（`.singularity`）默认保留，如需清理请手动删除。

开发阶段修改会话格式后，退出应用并清空数据目录中的 `sessions/`，包括附件，再启动新版。模型配置和项目登记独立保存。
