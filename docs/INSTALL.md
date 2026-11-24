# 安装与配置

本文介绍发布包的安装、模型和 MCP 配置、项目指令与技能，以及数据目录。从源码构建见[开发指南](development.md)。

## 安装发布包

### 系统要求
- **操作系统**：Windows 10 / 11 (x86-64)
- **必备工具**：[Git for Windows](https://git-scm.com/download/win)（Agent 依赖 `bash.exe` 执行命令与搜索；请确保 `bash.exe` 或 Git `bin` 目录位于系统 `PATH`）
- **可选依赖**：根据被开发项目的需要，安装对应语言工具链（如 Node.js、Python、Rust、`ripgrep` 等）

### 运行
1. 把发布包解压到任意本地目录，保留完整目录，运行时不需要另装 Node.js。
2. 双击运行 `Singularity.exe`。不需要配置端口或网络代理。
   - **窗口行为**：点击窗口关闭按钮会最小化至系统托盘，后台任务继续执行；右键托盘图标选择“退出”可完全结束程序。
   - **快捷键**：<kbd>Ctrl</kbd> + <kbd>R</kbd> 重新加载界面，不会中断正在运行的任务。

## 模型配置

可通过工作台界面的 **“设置 > 模型”** 可视化配置，也可直接编辑数据目录下的配置文件：
- **模型配置与 API Key**：`~/.singularity/config.json`，保存时一起原子替换。文件包含密钥，请勿泄露或提交到版本控制。

模型与 MCP 的 HTTPS 连接使用 Windows 系统信任的证书。企业代理使用自有证书时，将其根证书安装到 Windows 信任存储中。

### 配置文件示例

默认模型只用于空白页的初始选择；首次发送保存该任务的模型，之后沿用它或在任务内切换。

“获取可用模型”只导入模型 ID，上下文窗口、最大输出 Token 和思考选项由用户手动填写。工作台用档位按钮配置可用思考程度，使用时在模型选择器中切换。任务未选择思考档位时，使用提供方默认行为。

`~/.singularity/config.json`：
```json
{
  "default_model": { "provider": "deepseek", "model": "deepseek-flash", "reasoning": "high" },
  "providers": {
    "deepseek": {
      "api_key": "sk-your-api-key-here",
      "base_url": "https://api.deepseek.com/anthropic/v1",
      "api_protocol": "messages",
      "models": {
        "deepseek-flash": {
          "max_context_tokens": 500000,
          "max_output_tokens": 65536,
          "reasoning_variants": ["off", "low", "high", "max"]
        }
      }
    }
  }
}
```

`reasoning_variants` 直接填写提供方接受的档位名称，例如 `["off", "low", "high"]`。Messages 协议支持 `off`、`on` 以及具体档位（启用时发送 `thinking: {"type":"adaptive"}`，具体档位附带 `output_config.effort`）；Responses 协议须填写具体档位或 `off`，不接受 `on`。

### `base_url` 规范与常见厂商示例

`api_protocol` 必须在提供方上设置为 `messages` 或 `responses`，该提供方的所有模型统一使用它。模型配置不包含协议字段。

系统会在 `base_url` 后追加 `/messages`、`/responses` 或 `/models`。请填写 API 根地址，按提供方要求保留版本号和自定义路径，不填写完整接口地址：

| 提供方 | 推荐 `base_url` | 协议类型 (`api_protocol`) |
| --- | --- | --- |
| **Anthropic 官方** | `https://api.anthropic.com/v1` | `messages` |
| **DeepSeek 官方** | `https://api.deepseek.com/anthropic/v1` | `messages` |
| **OpenAI 官方** | `https://api.openai.com/v1` | `responses` |

## 联网搜索

可通过 **“设置 > 联网搜索”** 配置 DeepSeek 独立搜索与网页抓取服务（所有聊天模型通用，不绑定特定对话提供方），配置保存在 `~/.singularity/config.json` 的 `web_search` 字段：

```json
{
  "web_search": {
    "base_url": "https://api.deepseek.com",
    "api_key": "sk-your-deepseek-api-key"
  }
}
```

- **默认地址**：`base_url` 留空时默认 `https://api.deepseek.com`，分别请求 `POST /v1/tools/search` 与 `POST /v1/tools/fetch`。
- **按任务开关**：配置 API 密钥并保存后，新建任务页工具栏会出现 `web_*` 胶囊（包含 `web_search` 与 `web_fetch`），可按任务决定是否开启；未配置或清除密钥时不注入联网工具，不占用上下文 Token。

## MCP 工具

通过 **“设置 > MCP”** 可添加本地（stdio）或远程（Streamable HTTP）服务器。配置保存在 `~/.singularity/mcp.json`：

```json
{
  "mcpServers": {
    "chrome_devtools": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "chrome-devtools-mcp@latest", "--isolated", "--no-usage-statistics"],
      "startupTimeoutSec": 60,
      "toolTimeoutSec": 120
    },
    "custom_remote": {
      "type": "http",
      "url": "https://mcp.internal.example.com/sse",
      "headers": {
        "Authorization": "Bearer token-here"
      }
    }
  }
}
```

- **可省略字段**：参数 `args`、额外环境变量 `env` 和请求头 `headers` 省略时按空值读取；工作目录 `cwd` 省略时使用任务的项目目录。设置接口与文件使用相同规则。
- **命令路径**：相对路径（如 `./server.exe`）按服务器工作目录解析；只有文件名的命令（如 `npx`）从系统 `PATH` 查找。绝对路径直接使用。
- **生效时机**：配置在下一轮任务开始时生效。工具清单与项目指令一起在每轮开始时加载，同一轮内不变。
- **按任务关闭**：新建任务页的胶囊可以关掉某个服务器；关掉后这个任务不连接它，工具和服务器说明都不发给模型，其他任务不受影响。MCP 管理页只负责增删改和查看状态。
- **连接状态**：打开设置只查看已有状态，不会主动连接。每轮开始时复用正常的连接并重新获取工具列表，服务器更新工具后下一轮生效。连接失败时可点击“重试连接”。工具在一轮中途被服务器移除时，调用会直接失败，不会自动重试。
- **统一延迟加载**：所有启用的 MCP 服务器均采用按工具粒度的延迟加载——仅在指令中注入工具名、无参标记与精简简介，由固定的 `mcp_schema`（支持单次查一个或多个指定工具）与 `mcp_call` 代理按需读取参数定义并调用，降低上下文占用并保持工具列表前缀稳定。
- **环境要求**：本地 stdio 服务需要在系统 PATH 中安装对应的运行环境（如 Node.js 或 Python）。

## 项目指令与技能

### 项目指令 (`AGENTS.md`)
Agent 在每轮开始时完整读取指令，同一轮内不变，压缩不会重新读取。按以下顺序合并，越靠后越具体：
1. 用户全局指令：`~/.singularity/AGENTS.md`
2. 项目指令：从项目根目录（含 `.git` 的目录；找不到时就是任务目录）逐级到任务目录，每一级的 `AGENTS.md`

### 技能目录 (Skills)
在输入框开头或空白后输入 `/` 可触发技能候选项，也可直接键入完整名称。正文中的引用同样生效，多个技能用空白分隔，例如 `使用 /code-review /find-simplifications 审查项目`。后端接受发送前，按当前项目的技能目录完整读取所选技能，把原始任务文字和带名称、入口路径及资源基准目录的技能全文一起交给模型。同一条消息里重复引用只加载一次，辅助文件仍由模型按需读取。界面和草稿保留原文；排队期间技能文件变化不影响这次输入，重新打开任务沿用保存的内容，再次引用则读取当前文件。已识别技能读取失败时发送报错并保留草稿。未知名称、路径以及紧贴标点的 `/技能名，`、`(/技能名)` 都保留原文；`/a/b` 不表示两个技能。模型自行选择技能时调用 `skill` 工具按名称加载正文；新建任务页可以关掉 Skills 胶囊，关掉后技能目录和这个工具都不发给模型。

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

## 数据目录

所有配置、凭据与会话日志保存在数据根目录：
- **默认路径**：`%USERPROFILE%\.singularity`
- **自定义路径**：设置系统环境变量 `SINGULARITY_HOME` 为指定的绝对路径；未设置或只含空白时使用默认目录。

### 目录结构

| 相对路径 | 说明 |
| --- | --- |
| `config.json` | 模型提供方配置、API Key、模型元数据、默认模型选择与联网搜索配置（`web_search`） |
| `mcp.json` | MCP 服务器列表、环境变量与请求头 |
| `workspaces.json` | 已添加的项目及其目录 |
| `sessions/<id>.jsonl` | 每个任务的完整历史（当前格式为 v1），工具结果分别保存模型正文与必要的展示字段；后台命令保存纯文本输出尾部及字节范围 |
| `sessions/images/<id>/` | 各任务中发送或读取的图片文件 |
| `sessions/outputs/<id>/` | 各任务的完整超长工具输出 |
| `logs/singularity.log` | 后端结构化诊断日志（单文件 5 MiB 自动滚动保留 3 份，可用 `SINGULARITY_LOG` / `RUST_LOG` 调整级别） |
| `AGENTS.md` | 用户全局指令（可选） |
| `skills/` | 用户级全局技能脚本与说明 |

Bash 或 MCP 工具输出超限时，完整文本保存到 `sessions/outputs/<id>/`。Bash 的原始彩色输出与模型纯文本分别保存，历史记录保留路径；短输出直接保存在会话中。图片和完整输出在压缩、停止、重新打开任务后仍保留，删除任务时一并清理。备份会话时应一起复制 JSONL、图片目录和输出目录。

### 数据备份与升级
- **备份**：退出所有 Singularity 进程后，直接复制整份 `.singularity` 目录即可完成备份。
- **卸载**：直接删除应用解压目录即可；用户数据目录（`.singularity`）默认保留，如需清理请手动删除。

开发阶段修改会话格式后，退出应用并清空数据目录中的 `sessions/`，包括附件，再启动新版。模型配置和项目登记独立保存。
