# Singularity 仓库指令

Windows 本地单用户桌面 AI 编程工作台：Electron 前端（`apps/desktop/`）+ Rust stdio JSON-RPC 后端（`crates/`）。

## 按需阅读

| 文档 | 什么时候读 |
| --- | --- |
| [已确认的取舍](docs/decisions.md) | 审查、修复、精简前（表内限制不当缺陷，也不重复提议修改） |
| [架构说明](docs/architecture.md) | 定位源码、判断状态归属、确认运行边界 |
| [architecture.html](architecture.html) | 改执行顺序、条件分支、失败或停止处理前后 |
| [directory.html](directory.html) | 查代码分布与导出入口；增删源码文件或公开符号后 |
| [开发指南](docs/development.md) | 构建、测试保留标准、现场验证、Windows 环境排障与打包交付 |
| [安装与配置](docs/INSTALL.md) | 改配置文件、数据目录、指令或技能加载规则 |
| [研发编排流程](.agents/skills/dev-loop/SKILL.md) | 改代码、提 PR、跨模型审查或做项目/系统复盘时 |
| `AGENTS.local.md`（若存在） | 本机全仓检索索引、默认真实模型、本地运行包路径与构建缓存约定 |

## 决策边界

* **先问再做**：
  * **运行边界**：不越出[架构说明#运行边界](docs/architecture.md#运行边界)，不为边界外场景（多实例写同目录、跨平台、网络访问）加同步、恢复或协调机制；要改边界先讨论。
  * **界面改动**：任何用户未明确要求的界面变化都先问（新增按钮、菜单项、提示、设置项、状态显示，或改颜色、间距、动效、文案）。审查、修复、精简和同步文档时先在原设计内解决问题；确实要改，说明问题、最小改法和使用影响。改控件前先核对现有入口和完整操作。
  * **架构图改版**：`architecture.html` 是单画布连通大图（源码位置仅悬停显示）；拆图、分页、加标签页、工具栏或点击跳转须用户明确要求。
  * **测试与取舍**：常驻套件新增独立场景或新测试依赖前先说明覆盖的不变量与代价；用户决定保留设计或接受限制时，询问是否写入 `docs/decisions.md`。
* **直接执行（不用问）**：
  * **版本固定**：项目包版本固定 `0.1.0`，会话日志格式固定 `1`（外部依赖和标准协议按各自版本要求）。改会话格式后直接退出应用并清空 `~/.singularity/sessions/`（含附件），不写旧格式识别或迁移。
  * **验收准备**：真机现场验收前直接退出占锁的 Singularity 实例；对验证有帮助时直接调用真实模型。

## 结构与体量纪律

* **目标**：以“职责清晰、可维护前提下让**项目整体稳态代码量与概念数最少**”为准，不以“单次 diff 最小”在外层叠补丁；若合并重复逻辑或整理局部结构能让整体更小，允许更宽的当次改动。
* **先复用再新增**：先复用现有状态、通知与通道，不为边缘问题新建平行状态机、冗余包装或多余持久化字段；不为躲过 600 行上限机械切碎高内聚文件。
* **大改动收口**：修改超过约 5 个文件或净增超过约 200 行时，进门禁前先通读完整 `git diff` 清理过度设计，并在交付时如实汇报净增行数与新增文件数。

## 注释纪律

* 只随当前改动清理注释，不单独发起全仓扫注释提交。
* 不写复述代码行为的注释；代码不直白时优先改命名或简化结构。
* 能用类型、断言、Lint 或常驻测试锁死的约束，改为代码或测试强制执行，不留口头警告注释。
* 仅保留外部不可控事实的简短说明（Windows / 第三方协议怪癖、安全或 Lint 豁免理由、CI SHA 锁定版本号如 `# v6`）。

## 改动后同步

| 改动 | 同步动作 |
| --- | --- |
| `crates/protocol/src/` 中的 DTO 或 RPC | 运行 `pwsh -File scripts/check.ps1 -Scope protocol`（导出 `apps/desktop/src/protocol.generated.ts` 并确认无漂移），更新前端调用方 |
| 执行顺序、条件分支、模块关系、数据流、失败或停止处理 | 同一次改动中在 `architecture.html` 原图上局部更新对应节点、连线和说明，对照源码核对；调整过布局则检查渲染画面，确保文字和箭头清晰 |
| 模块职责、状态归属、源码入口 | 同步 `docs/architecture.md`、`directory.html` |
| 命令或启动步骤 | 同步 `docs/development.md` |

## 检查与交付

* **红线**：生产代码与功能验收只用真实提供方和模型，进程内不做 Mock；常驻自动化测试隔离 `SINGULARITY_HOME`；增删或修改测试前先读[测试保留与清理标准](docs/development.md#测试保留与清理标准)，临时脚本与非不变量测试验证后即删。
* **分级完成判据**（通过即止，不反复重跑）：
  * **纯文档 / 配置改动**：核对文档内容与源码事实一致即可。
  * **代码修改 / Bug 修复**：按受影响范围执行统一脚本（与 CI 全参门禁一致；`rust` / `desktop` / `all` 均含 600 行上限检查，亦可单独 `-Scope size`）：
    * Rust：`pwsh -File scripts/check.ps1 -Scope rust`（`cargo fmt` + `cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings` + `cargo test --workspace --locked`）
    * 跨端协议：`pwsh -File scripts/check.ps1 -Scope protocol`
    * 前端 / 桌面主进程：`pwsh -File scripts/check.ps1 -Scope desktop`（`lint` + `build` + `cargo build -p app --locked` + `prepare:runtime` + `test:e2e`）
    * 全量：`pwsh -File scripts/check.ps1 -Scope all`
  * **用户可见功能交付**：除上述检查外，按[开发指南 §4–§5](docs/development.md#4-现场验证)及 `AGENTS.local.md`（若存在）在 production 版 Electron 工作台用 Playwright 验收受影响路径（结果、出错、停止、重开历史；布局与动效观感写明手测步骤交用户看），更新本地运行包并按需清理构建缓存。
