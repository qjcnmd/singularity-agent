# Singularity 仓库指令

## 文档

| 文档 | 什么时候读 |
| --- | --- |
| [已确认的取舍](docs/decisions.md) | 审查、修复、精简之前。表里的条目不当缺陷报告，也不重复提议修改 |
| [架构说明](docs/architecture.md) | 定位源码、判断状态由谁维护、确认运行边界 |
| [architecture.html](architecture.html) | 改执行顺序、条件分支、失败或停止处理之前和之后 |
| [工作台交互](docs/desktop-ui.md) | 改工作台界面之前 |
| [开发指南](docs/development.md) | 构建、检查、打包 |
| [安装与配置](docs/INSTALL.md) | 改配置文件、数据目录、指令或技能的加载规则 |

## 约定

- **先同步索引再搜索**：新任务开始时，先用 `xerj` 技能同步索引（前缀 `harness`，状态目录 `D:\XERJ\autoindex\harness`）。任务中途只在搜不到刚改过的内容时再同步。
- **版本号不动**：项目包版本固定 `0.1.0`，会话日志格式版本固定 `1`。改了会话格式就退出应用，直接清空 `~/.singularity/sessions/`（含附件），不用问；程序里不写旧格式识别或迁移。对外发布前再定版本和兼容策略。外部依赖和标准协议按各自的版本要求。
- **不越出运行边界**：边界见[架构说明](docs/architecture.md#运行边界)。不为边界外的场景（多个实例写同一目录、其他平台、网络访问）加同步、恢复或协调机制；要改边界先和用户讨论。
- **界面改动先问**：用户没明确要求的界面变化都先问，包括新增按钮、菜单项、提示、设置项、状态显示，以及修改颜色、间距、动效和文案。审查、修复、精简和同步文档时，已确认的界面和交互保持原样，先在原设计里解决问题；确实要改，说明问题、最小改法和对使用的影响。改控件前先核对它现有的入口和完整操作。
- **架构图在原图上改**：`architecture.html` 是一张连通的大图，在同一画布里拖动和缩放，源码位置只在悬停时显示。更新时在原文件上改相关的节点、连线和说明，不重新生成或重排整图。拆图、分页、加标签页、工具栏或点击跳转，都要用户明确要求才做。
- **记录取舍**：用户决定保留某个设计或接受某个限制时，问一句是否写进 `docs/decisions.md`，同意后再写。
- **提交**：只在用户要求时提交。

## 改动后的同步

| 改动 | 同步 |
| --- | --- |
| `crates/protocol/src/` 中的 DTO 或 RPC | 运行 `cargo run -p singularity_protocol --features typescript --example export_types`，更新前端调用方 |
| Rust 公共函数或类型 | 受影响的调用方 |
| 执行顺序、条件分支、模块关系、数据流、失败或停止处理 | 同一改动中更新 `architecture.html` 对应的节点、连线和说明，对照源码核对；调整过布局就看渲染画面，文字和箭头要清楚 |
| 模块职责、状态由谁维护、源码入口 | `docs/architecture.md` |
| 控件的操作方式 | `docs/desktop-ui.md` |
| 命令或启动步骤 | `docs/development.md` |

## 检查

- **每次改动**：改了 Rust 跑受影响 crate 的 `cargo clippy` 和 `cargo fmt --all`；改了前端跑 `npm --prefix apps/desktop run build`（含类型检查）。完整命令见[开发指南](docs/development.md#2-质量检查与代码格式化)。
- **交付前**：确认受影响部分的格式、静态检查、构建和现场验证通过，通过就结束，不反复跑。
- **新增测试先问**：默认不写新测试。遇到现场实测碰不到的问题（比如事件先后顺序、时机），先说明为什么测不到，用户同意再写。
- **现场实测**：功能做完后，在 production 版 Electron 工作台里用 Playwright 操作改到的功能，检查结果、出错、停止和重新打开任务后的表现，只测受影响部分。布局和动效的观感由用户看：交付时说明要看哪个界面、怎么操作、看什么。

## 验证环境与交付

- **数据目录**：验证直接用用户的 `~/.singularity`。启动工作台前直接退出占用它的实例（单实例锁），不用问；验证结束后清理本次创建的项目、任务及附件，保留用户原有数据。
- **Windows 自动化环境**：Playwright / Electron 自动化从普通 PowerShell 启动 Node 验证脚本，让 Electron 和 Rust 继承完整的 Windows 环境，不要沿用 `node_repl` 这类工具裁剪过的环境。装了 Git Bash 却报找不到时，先查验证进程的 `ProgramFiles`、`PATH` 和实际 `bash.exe` 路径，改好启动方式再重跑。
- **模型调用**：只用真实的提供方和模型，不写模拟提供方。对验证有帮助就直接调用，不用问。用户没指定时用 DeepSeek 官方的 `deepseek/deepseek-flash`（已配置在 `~/.singularity/config.json`，地址 `https://api.deepseek.com`）。
- **临时文件**：验证脚本、调查脚本和临时数据用完就删，不留在仓库。
- **试用包**：每次功能做完、检查通过后，按[开发指南](docs/development.md#5-打包构建与发布)打包，退出正在运行的 Singularity，用 `apps/desktop/release/win-unpacked/` 的内容整体替换 `Desktop/Singularity`。桌面只保留这一份，不另建“新版”、日期或版本号目录；用户数据在 `~/.singularity`，不受影响。
