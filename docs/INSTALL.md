# 安装与运行

Singularity 当前发布目标为 Windows x86-64。发布包包含 Electron、前端资源和 Rust 后端，不需要单独安装 Node.js。

## 安装发布包

1. 解压发布归档。
2. 保留完整目录，双击 `Singularity.exe`。评估入口位于 `resources/runtime/singularity.exe`。
3. 安装 [Git for Windows](https://git-scm.com/install/windows)。程序先检查标准 Git 安装目录，再查找 `PATH` 中的 `bash.exe`；自定义安装位置可将 Git 的 `bin` 目录加入 `PATH`，不使用 Windows 的 WSL 启动器代替 Git Bash。
4. 按目标项目需要安装 Python、Node.js、Rust 等工具链。

验证安装：

```powershell
resources/runtime/singularity.exe --help
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
npm --prefix apps/desktop ci
npm --prefix apps/desktop run build
cargo build --release --locked --package singularity_app --bins
npm --prefix apps/desktop run prepare:runtime -- release
npm --prefix apps/desktop run package
```

完整桌面产物在 `apps/desktop/release/win-unpacked/`。复制整个目录到没有仓库和 Node.js 的环境即可运行。

## 启动工作台

双击 `Singularity.exe`。关闭窗口隐藏到系统托盘，后台任务继续；点击托盘图标显示窗口，托盘菜单“退出”结束应用。Ctrl+R 刷新界面不会重启 Rust 后端。

工作台通过私有 IPC 和 stdio 管道通信，资源从本地打包目录加载，不监听端口。相同数据目录重复启动只显示现有窗口；不同数据目录允许独立运行。

## Provider 配置

“设置 > 模型”管理 Provider 地址、协议、模型元数据与 API Key；Composer 发送按钮旁的组合选择器管理当前 Task 的模型和思考程度。也可直接维护 `SINGULARITY_HOME\config.json` 和私有认证文件 `auth.json`。桌面设置中的提供方统一选择 `api_protocol: chat|responses`，保存时应用于旗下全部模型，selector 形如 `provider_id/model_id#variant`。手编文件若同时声明提供方与模型协议，以提供方为准；未声明提供方协议时沿用各模型协议。

```json
{
  "default_model": "example/model#high",
  "providers": {
    "example": {
      "base_url": "https://api.example.com/v1",
      "api_protocol": "chat",
      "models": {
        "model": {
          "max_context_tokens": 128000,
          "max_output_tokens": 8192,
          "reasoning_variants": {
            "high": {"wire_effort": "high"}
          },
          "default_variant": "high"
        }
      }
    }
  }
}
```

`base_url` 就是 API 根：写明的已知端点（`/chat/completions`、`/responses`、`/models`）会先被剥掉，剩下的部分逐字作为根，推理端点与模型目录接口都由这一个根拼出。中间层不替你补版本段，因此 `base_url` 要写到端点真正所在的那一级：OpenAI 官方写 `https://api.openai.com/v1`，DeepSeek 写 `https://api.deepseek.com/v1`；写裸主机 `https://api.deepseek.com` 会得到 `https://api.deepseek.com/chat/completions`。同一个根在推理与目录之间只有一个含义。设置页保存只规范输入形状（去首尾空白与结尾斜杠），不改写你写明的地址。

API Key 通过“设置 > 模型”或 `auth.json` 按 Provider 保存。工作台响应、日志和模型目录投影不会返回凭据。

直接配置时，`auth.json` 与上例的 Provider ID 对应，格式如下；将占位值替换为自己的密钥：

```json
{
  "providers": {
    "example": {"api_key": "YOUR_API_KEY"}
  }
}
```

配置文件只保存模型元数据，密钥只放在认证文件中；不要把认证文件提交到项目仓库。

## 端点形状开关

“获取可用模型”查询提供方的 `/models`，优先使用它返回的元数据，再从 [models.dev](https://models.dev/) 中按实际端点与精确模型 ID 补齐。未取得的信息保持未知，在表单中补齐必填容量后才能保存。补充来源显示在候选列表，只有应用并保存后才影响后续任务。

添加与编辑模型使用同一表单。输入模型 ID 后点击“智能配置”，按 ID、地址和提供方协议获取可用数据；用户修改某项后，仅该项转为手动管理，其他字段仍可更新。`automatic_fields` 记录智能管理的字段，未设置的旧模型保留已有手工值，缺失字段可获取补齐。上下文窗口和最大输出 Token 未填完整时保存按钮不可用；不存在隐式填入估算容量的保存路径。高级配置可编辑思考选项，正文可滚动，标题与底部操作始终可见。

“重置表单”清空手动覆盖；再次点击“智能配置”获取推荐值。点击“智能配置”或应用模型目录时更新智能字段，获取失败时保留当前配置；正在执行的轮次继续使用其开始时的配置快照。

下列字段控制请求形状。`supports_developer_role`、`supports_tool_choice` 和 `requires_assistant_content_for_tool_calls` 仍在配置文件中维护；表单保存保留其原值。字段或取值无效时明确报错。

| 字段 | 默认 | 需要改为另一值的情形 |
| --- | --- | --- |
| `thinking_wire_format` | `reasoning_effort` | 思考开关的落点词形：`reasoning_effort` 只发 `reasoning_effort`；`thinking_type` 发 `thinking: {"type": …}`；`enable_thinking` 发顶层布尔，只适用于 Chat。 |
| `supports_developer_role` | `false` | 端点接受 `developer` 角色时设为 `true`；默认把该角色按 `system` 发出（Chat）。 |
| `supports_tool_choice` | `true` | 端点拒绝 `tool_choice` 字段时设为 `false`，带工具的请求不再携带它。 |
| `requires_assistant_content_for_tool_calls` | `false` | 端点要求带工具调用的 assistant 消息必须带 `content` 时设为 `true`，此时该字段写空串而不是 `null`。只适用于 Chat，写在其它协议上会在配置校验时报错。 |
| `requires_reasoning_content_for_tool_calls` | `false` | 端点要求带工具调用的回复必须回传续接数据时设为 `true`；缺少时该次请求明确失败，不带着残缺历史继续。选中关闭思考的变体时这一项不生效。 |
| `chat_output_tokens_field` | `max_tokens` | Chat 请求里 `max_output_tokens` 落在哪个字段：可选 `max_tokens` 或 `max_completion_tokens`，缺省发送 `max_tokens`。取值为空串视为没有声明，按缺省处理。只适用于 Chat，写在 `responses` 上会在配置校验时报错。Responses 一律发 `max_output_tokens`。 |

## MCP 工具

“设置 > MCP”添加、编辑、删除服务器，并通过独立开关启用或停用。展开服务器可查看连接结果、工具名称和说明，失败时修正配置后重新连接。设置中的连接检查使用当前项目目录；尚未选择项目时使用用户数据目录。

本地服务器选择 stdio，填写命令、逐行参数、可选工作目录与环境变量 JSON。命令由系统 PATH 查找；工作目录留空使用任务项目，相对目录从项目解析。服务器所需的 Node.js、Python 等运行环境须在本机安装。远程服务器选择 Streamable HTTP，填写 HTTP/HTTPS 地址和请求头 JSON，例如 `{"Authorization":"Bearer token"}`。连接超时默认 30 秒，工具调用超时默认 120 秒。

配置与开关在下一次发送或压缩后的刷新边界生效。每回合首次模型请求前发现全部已启用工具；手动摘要复用最近请求的工具定义；普通连续请求保持工具名称、schema 与顺序稳定，压缩完成后与当前指令一起刷新。MCP 调用和内建工具使用相同的轨迹、停止与历史保存；工具返回的文本、结构化数据和图片进入现有模型输入与图片快照流程。连接失败会显示具体原因，该服务器的工具不会进入请求。

用户级配置保存在私有 `mcp.json`，可在程序退出后直接维护：

```json
{
  "mcpServers": {
    "chrome_devtools": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "chrome-devtools-mcp@latest", "--isolated", "--no-usage-statistics", "--no-performance-crux"],
      "enabled": true,
      "startupTimeoutSec": 60,
      "toolTimeoutSec": 120
    }
  }
}
```

Chrome DevTools MCP 在第一次浏览器调用时启动独立 Chrome，要求本机安装 Node.js、npm 与 Google Chrome。环境变量和请求头可能包含凭据，设置通过私有桌面通道编辑，配置文件应与 `auth.json` 一样保持私密。当前 MCP 能力面向工具调用，远程认证使用显式请求头令牌。

## 项目指令

程序先读取用户数据目录的 `AGENTS.md`，再从任务 cwd 向上找到最近的 Git 根，按项目根到 cwd 的顺序读取各级 `AGENTS.md`；没有 Git 根时以任务目录为根。工作台任务通常以已登记项目目录作为 cwd。

文件内容带来源进入模型上下文，在每轮任务开始和压缩后重新读取。单文件最多读取 32 KiB，合并最多 64 KiB，超出时保留预算内前缀并反馈截断；读取失败会明确报错。Skills 用于按需加载专项说明，与这些持续生效的文件指令分开。

## Skills

技能使用带 YAML 元数据的 Markdown 文件，可放在 `技能名/SKILL.md` 或 `技能名.md`。查找顺序如下，同名时靠前的目录优先：

1. 项目根目录的 `.singularity/skills/`；
2. 项目根目录的 `.agents/skills/`；
3. 用户数据目录的 `skills/`，即数据根下的 `skills\`；
4. 用户主目录的 `.agents/skills/`（仅在数据根使用默认位置时；显式指定 `SINGULARITY_HOME` 的独立数据目录自成一体，不带入真实用户的技能）。

项目根取 cwd 向上的最近 Git 根，没有 Git 时使用任务目录。

```markdown
---
name: review
description: 检查代码改动和相关验证结果
---
先阅读改动与相关实现，核实实际影响，再报告有证据的问题。
```

在输入框输入 `/` 查看技能，继续输入按名称前缀筛选；用上下键移动，Enter、Tab 或鼠标选中。选中后界面显示 `/review`，同时绑定该技能文件的绝对路径。发送、排队或插话交付给模型时，相应技能词展开为这条路径；输入框和用户消息仍显示技能名。模型通过 `read` 工具读取正文，读取失败作为工具错误返回。

可选元数据 `user-invocable: false` 隐藏手动入口，`disable-model-invocation: true` 隐藏模型可见的技能目录项；两者默认分别为 `true`、`false`。目录在每个 turn 开始及压缩后重新发现，因此同一会话中新加的技能会在下一次输入或压缩后可见。发现时只读取 frontmatter 的 YAML 元数据；手动选择与模型按需调用均由模型使用 `read` 读取原文件，正文以普通工具结果保留在历史中。相对资源路径以技能文件所在目录为准。格式错误会指出文件，不影响其他有效技能；技能中的脚本不会因加载而自动执行。

## 评估入口

```powershell
singularity --json "完成一项可验证的修改" --model example/model#high
```

- `--json` 输出 JSONL 事件并以终态 `summary` 行收尾；stdout 写入失败后该通道不再能确认行边界，因此不再补写任何行（执行事实仍已持久化，输出故障另行报告）；
- `--model <selector>` 选择本次评估的模型，省略时使用默认模型；
- 每次新建并保存会话，评估器通过 `SINGULARITY_HOME` 隔离数据。

评估器负责超时和进程终止。正常返回时，退出码 0 表示完成，1 表示失败；失败原因写入 stderr。进程被外部终止时可能没有终态 summary，已保存的会话仍可供评估器读取。

## 查看图片

可在任务输入框粘贴、拖入或选择图片，也可让 Agent 用 `read` 读取本地图片。支持 PNG、JPEG、WebP、GIF 首帧和 BMP；GIF 和 BMP 转为 PNG。缩略图可点击查看大图，图片可与文字一起发送或单独发送。刷新保留草稿，重启后历史使用已保存的图片快照，原文件的修改或删除不影响已发送图片。

## 数据、更新与卸载

数据目录默认是系统用户主目录下的 `.singularity`，与启动时所在目录无关。设置 `SINGULARITY_HOME` 可改用另一个绝对路径，例如让并行实例或评估任务使用独立数据。配置、凭据、项目登记、会话和用户级指令都使用该目录。路径由系统解析，支持 Windows junction 等目录重定向；无效路径明确报错。

| 路径 | 内容 |
| --- | --- |
| `config.json` | Provider、模型元数据与默认选择 |
| `auth.json` | Provider 凭据 |
| `mcp.json` | MCP 服务器、开关、环境变量与 HTTP 请求头 |
| `workspaces.json` | 已登记项目目录 |
| `sessions/<uuid>.jsonl` | 会话正文、请求中的 Harness 指令与工具定义、请求观测与终态 |
| `sessions/archived/` | 已归档会话 |
| `sessions/images/<任务 ID>/<图片 ID>` | 已发送或工具读取图片的像素快照 |
| `AGENTS.md`、`skills/` | 用户级文件指令与技能 |

文字和图片草稿保存在 IndexedDB，主题、侧栏和阅读位置保存在 localStorage；它们均位于 Electron 的 `%APPDATA%/Singularity/<数据目录哈希>/` 中，各数据目录独立。原浏览器中的草稿和视图偏好不会自动导入，Rust 配置、项目和会话继续沿用原数据目录。

超长 bash 输出保存在系统临时目录的 `singularity-tool-output/<uuid>.log`，工具结果会给出完整路径。输出文件继承 Windows 用户临时目录 ACL。新建输出文件时清理超过七天的旧输出；保存失败会显示原因，不提供不完整文件的路径，也不改写命令本身的退出状态。

当前会话格式为 v11。程序只读取当前格式，升级前须确认旧会话的保留或清理方式；配置与项目登记可继续沿用。

更新前退出需要替换的程序，再替换完整发布目录。备份会话时先退出使用该数据目录的实例，再复制整个数据目录；备份包含凭据，应保留其私密性。卸载只需删除程序并从 `PATH` 移除，用户数据和桌面视图存储不会自动删除。移除已登记项目不删除项目文件或会话，归档只把会话移出活动列表。
