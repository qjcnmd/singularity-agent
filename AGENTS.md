# Singularity 仓库指令

## 文档

|文档|什么时候读|
|-|-|
|[已确认的取舍](docs/decisions.md)|审查、修复、精简之前。表里的条目不当缺陷报告，也不重复提议修改|
|[架构说明](docs/architecture.md)|定位源码、判断状态由谁维护、确认运行边界|
|[architecture.html](architecture.html)|改执行顺序、条件分支、失败或停止处理之前和之后|
|[directory.html](directory.html)|查看代码分布和主要导出入口；增删源码文件或公开符号后|
|[开发指南](docs/development.md)|构建、检查、打包|
|[安装与配置](docs/INSTALL.md)|改配置文件、数据目录、指令或技能的加载规则|

## 约定

* **先同步索引再搜索**：新任务开始时，先用 `xerj` 技能同步索引（前缀 `harness`，状态目录 `D:\\XERJ\\autoindex\\harness`）。索引故障自行解决；任务中途只在搜不到刚改过的内容时再同步。

  * 遇到 `detected family changed`：移走备份状态目录，删除 `harness-docs`、`harness-apps-desktop` 及 `autoindex-catalog` 中 `prefix=harness` 的记录，再按原参数重建；同步和检索成功后删除备份，不用 `--fresh`，不动其他索引。
* **版本号不动**：项目包版本固定 `0.1.0`，会话日志格式版本固定 `1`。改了会话格式就退出应用，直接清空 `\~/.singularity/sessions/`（含附件），不用问；程序里不写旧格式识别或迁移。对外发布前再定版本和兼容策略。外部依赖和标准协议按各自的版本要求。
* **不越出运行边界**：边界见[架构说明](docs/architecture.md#运行边界)。不为边界外的场景（多个实例写同一目录、其他平台、网络访问）加同步、恢复或协调机制；要改边界先和用户讨论。
* **界面改动先问**：用户没明确要求的界面变化都先问，包括新增按钮、菜单项、提示、设置项、状态显示，以及修改颜色、间距、动效和文案。审查、修复、精简和同步文档时，已确认的界面和交互保持原样，先在原设计里解决问题；确实要改，说明问题、最小改法和对使用的影响。改控件前先核对它现有的入口和完整操作。
* **架构图在原图上改**：`architecture.html` 是一张连通的大图，在同一画布里拖动和缩放，源码位置只在悬停时显示。更新时在原文件上改相关的节点、连线和说明，不重新生成或重排整图。拆图、分页、加标签页、工具栏或点击跳转，都要用户明确要求才做。
* **记录取舍**：用户决定保留某个设计或接受某个限制时，问一句是否写进 `docs/decisions.md`，同意后再写。
* **提交**：只在用户要求时提交。

## 改动后的同步

|改动|同步|
|-|-|
|`crates/protocol/src/` 中的 DTO 或 RPC|运行 `cargo run -p protocol --features typescript --example export\_types`，更新前端调用方|
|Rust 公共函数或类型|受影响的调用方|
|执行顺序、条件分支、模块关系、数据流、失败或停止处理|同一改动中更新 `architecture.html` 对应的节点、连线和说明，对照源码核对；调整过布局就看渲染画面，文字和箭头要清楚|
|模块职责、状态由谁维护、源码入口|`docs/architecture.md`、`directory.html`|
|命令或启动步骤|`docs/development.md`|

## 检查

* **每次改动**：改了 Rust 跑受影响 crate 的 `cargo clippy`、`cargo fmt --all` 和 `cargo test --workspace --locked`（含属性测试与后端黑盒不变量测试）；改了 `crates/protocol/src/` 额外确认 `export_types` 输出无漂移；改了前端或桌面主进程跑 `npm --prefix apps/desktop run lint`、`npm --prefix apps/desktop run build`（含严格类型检查）和桌面 E2E 冒烟（`npm --prefix apps/desktop run test:e2e`）。完整命令见[开发指南](docs/development.md#2-质量检查与代码格式化)。
* **交付前**：确认受影响部分的格式、静态检查、构建、常驻测试和现场验证通过，通过就结束，不反复跑。
* **测试保留与清理标准**：
  * **允许长期保留的测试**（`crates/app/tests/`、`apps/desktop/e2e/` 与 `crates/{model,agent}/src/property_tests.rs`）：必须满足其一——① 从外部边界进入的黑盒测试（真实 `singularity.exe` JSON-RPC + 磁盘，或真实 Electron 用户可见流程），只断言**系统级不变量与跨模块契约**（协议合法性、超窗收敛、并发停止/删除不死锁、持久化重启一致、不串流、故障不污染上下文等）；② 针对高组合复杂度纯逻辑的**属性测试（`proptest`）**，用随机输入验证通用铁律（如分帧边界无关、编码角色/工具配对不变量、压缩切块不孤立工具、尾行截断自愈）。两类均**不绑定内部实现拆分、具体策略阈值或参数常数**——后续重构压缩策略、工具实现或内部模块时，测试应基本无需修改且仍能发现真问题。
  * **通过后必须清理的测试**：针对局部函数细节、具体参数/阈值（如固定占比阈值、固定重试次数、固定缓存上限）、单次 bug 复现或特定样式动效的临时测试与脚本，验证通过后立即删除，不留在仓库形成常绿维护负担。
  * **新增常驻测试前先确认**：在常驻套件里新增独立场景或引入新测试依赖前，先说明覆盖哪条系统不变量及代价，用户同意后再加。
* **现场实测**：功能做完后，在 production 版 Electron 工作台里用 Playwright 操作改到的功能，检查结果、出错、停止和重新打开任务后的表现，只测受影响部分。布局和动效的观感由用户看：交付时说明要看哪个界面、怎么操作、看什么。

## 验证环境与交付

* **数据目录**：常驻自动化测试一律使用临时隔离的 `SINGULARITY_HOME`（测试结束自动清理临时目录及派生的 Electron `userData`），不碰用户日常数据。针对真实模型的现场验收直接用用户的 `~/.singularity`，启动工作台前直接退出占用它的实例（单实例锁），不用问；验证结束后清理本次创建的项目、任务及附件，保留用户原有数据。
* **Windows 自动化环境**：Playwright / Electron 自动化从普通 PowerShell 启动 Node 验证脚本，让 Electron 和 Rust 继承完整的 Windows 环境，不要沿用 `node_repl` 这类工具裁剪过的环境。装了 Git Bash 却报找不到时，先查验证进程的 `ProgramFiles`、`PATH` 和实际 `bash.exe` 路径，改好启动方式再重跑。
* **模型调用与测试桩边界**：生产代码与功能现场验收只用真实的提供方和模型，不写模拟提供方（用户没指定时用 codearts/deepseek-v4.1-flash-1017 high，对验证有帮助就直接调用，不用问）。仅在测试目录内（`crates/app/tests/`、`apps/desktop/e2e/`）允许使用本地 HTTP SSE 回放/故障注入桩与测试用 MCP stdio 脚本，进程内部不做 Mock。
* **临时文件**：一次性验证脚本、调查脚本、不符合保留标准的临时测试和临时数据用完就删，不留在仓库。
* **试用包**：每次功能做完、检查通过后，按[开发指南](docs/development.md#5-打包构建与发布)更新 `Desktop/Singularity`。日常只替换改动涉及的应用归档和 Rust 后端；首次交付、Electron 或运行依赖、打包配置变化时才完整打包。替换前退出 Singularity，更新后从桌面目录验证。只保留这一份成品，临时打包文件用完删除；用户数据在 `\~/.singularity`。
* **构建缓存**：本机 Rust 构建目录放在 D 盘，沿用 `cargo metadata` 返回的位置。功能交付后检查大小，超过 10 GiB 就运行 `cargo clean`；构建期间可以临时超过阈值，清理后下次构建需要重新编译。

