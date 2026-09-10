# Singularity

Singularity 是以 Rust 实现的本地 coding-agent harness。无参数启动浏览器工作台；`--print` 与 `--json` 提供同一 Agent 能力的单次自动化入口。Workspace、Session、模型配置和执行状态由本机 Host 统一管理，浏览器只负责呈现与控制。

当前发布目标为 Windows x86-64。

## 产品结构

```text
singularity（单进程）
  ├─ Web 工作台（默认）：项目与任务、对话、轨迹、模型设置和执行中输入
  ├─ --print / --json：单次无交互执行
  └─ 两类入口共用 crates/runtime
       └─ AgentLoop + read/glob/grep/bash/edit/write/skill + Provider
```

详细对象、状态与协议边界见 [`docs/singularity.md`](docs/singularity.md)。

## 安装

发布包只有一个运行时程序 `singularity.exe`，另附许可证和说明文件。运行时不需要 Node.js；目标项目需要的工具链仍由用户安装。Windows 的命令工具需要 [Git for Windows](https://git-scm.com/install/windows) 提供的 Bash。

完整安装和源码构建说明见 [`docs/INSTALL.md`](docs/INSTALL.md)。

## 使用

启动工作台：

```powershell
singularity
```

默认监听 `127.0.0.1:3080` 并打开默认浏览器，地址可直接再次打开，不需要 token 或登录。可指定端口，或由系统选择空闲端口并只打印地址：

```powershell
singularity --port 43120
singularity --port 0 --no-open
```

首次使用可在“设置 > 模型连接”中登记兼容 OpenAI Chat 或 Responses 协议的 Provider、模型与 API Key。API Key 是只写字段，页面与 Host 响应只显示脱敏配置状态。随后添加本机 Workspace、创建 Task，并在底部 Composer 的组合选择器中选择当前会话使用的模型与思考程度后提交工作。

运行中按 Enter 将输入排队，Ctrl/Cmd+Enter 插话；队列支持编辑、立即发送和撤回。会话日志保存历史，Host 维护执行状态，刷新或关闭页面不会停止后台任务。详细操作见 [工作台交互](docs/workbench.md)。

无交互入口：

```powershell
singularity --print "审查并修复当前仓库中的失败测试"
singularity --json "修复失败测试" --model provider/model#reasoning
```

`--print` 只输出最终 assistant 文本；`--json` 输出逐行事件并以终态 `summary` 行收尾。`--session <id>` 恢复既有 Thread，`--no-session` 禁用本次持久化。Web 参数不能与无交互参数混用。

## 本地数据与权限

默认用户目录为 `%USERPROFILE%\.singularity`，保存模型配置、凭据、已登记项目与会话日志；可用 `SINGULARITY_HOME` 指定独立目录。目录内容和备份方式见 [数据、更新与卸载](docs/INSTALL.md#数据更新与卸载)。

浏览器直接打开 `http://127.0.0.1:<port>/`，不需要登录或 token。程序只监听本机；控制请求要求当前 Host、同源来源，RPC 另要求 JSON 内容类型，没有跨源控制接口。这些校验不认证本机进程身份。

Agent 继承 `singularity.exe` 的本机权限，可读取、编辑文件并运行命令。Workspace 用于项目上下文、Session 分组和文件候选，不是文件系统沙箱。

## 开发与维护

| 文档 | 用途 |
| --- | --- |
| [安装与运行](docs/INSTALL.md) | 发布包安装、源码构建、模型和技能配置、数据维护 |
| [工作台交互](docs/workbench.md) | 项目、任务、输入、轨迹与显示约定 |
| [开发指南](docs/development.md) | 本地运行、相关检查、测试组织和发布流程 |
| [架构说明](docs/singularity.md) | 模块职责、持久事实和执行契约 |
| [宪章](docs/constitution.md) | 产品方向与边界 |

发布包附带同目录的 `INSTALL.md`；上表按源码仓库的文档路径组织。

## 许可证

项目主体使用 [MIT License](LICENSE)。
