# Singularity 仓库指令

Windows 本地单用户桌面 AI 编程工作台：Electron 前端（`apps/desktop/`）+ Rust stdio JSON-RPC 后端（`crates/`）。

## 按需阅读

| 文档 | 什么时候读 |
| --- | --- |
| [已确认的取舍](docs/decisions.md) | 审查、修复、精简前（表内限制不当缺陷，也不再提议修改） |
| [架构说明](docs/architecture.md) | 定位源码、判断状态归属、确认运行边界 |
| [architecture.html](architecture.html) | 改执行顺序、条件分支、失败或停止处理前后 |
| [directory.html](directory.html) | 查代码分布与导出入口；增删源码文件或公开符号后 |
| [开发指南](docs/development.md) | 构建、测试保留标准、现场验证、Windows 环境排障与打包交付 |
| [安装与配置](docs/INSTALL.md) | 改配置文件、数据目录、指令或技能加载规则 |
| [研发执行流程](.agents/skills/wf-dev/SKILL.md) | 改代码（含调研转编码）、UI/UX 迭代、独立审查与交付时 |
| [项目体检与修剪](.agents/skills/wf-audit/SKILL.md) | 手动执行周期体检、代码瘦身与跨模块排查时 |
| [研发流程复盘](.agents/skills/wf-retro/SKILL.md) | 手动复盘 `.reviews/` 审查档案与返工记录、硬化检查或优化工作流时 |
| `AGENTS.local.md`（若存在） | 本机检索索引、默认真实模型、本地运行包路径与构建缓存约定 |

## 决策边界

* **先问再做**：
  * **运行边界与新边角**：不越出[架构说明#运行边界](docs/architecture.md#运行边界)；遇到低概率新边界/异常取舍（含「显式报错拒收并记入 `docs/decisions.md`」选项）先问。
  * **界面改动**：任何用户未明确要求的界面、交互或文案变化都先问；改控件前先核对现有入口和完整操作。
  * **架构图改版**：`architecture.html` 为单画布连通大图（源码位置仅悬停显示）；拆图、分页或加工具栏/标签页须用户明确要求。
  * **测试、结构大改与取舍**：常驻套件新增独立场景或新测试依赖、大块删改/重写既有机制，或引入新状态机/配置项/抽象层前先说明代价；用户接受限制时询问是否写入 `docs/decisions.md`。
* **直接执行**：
  * **版本固定**：包版本固定 `0.1.0`，会话日志格式固定 `1`；改日志格式后直接退出应用并清空 `~/.singularity/sessions/`（含附件），不写旧格式迁移。
  * **验收准备**：真机验收前直接退出占锁的 Singularity 实例；需要时直接调用真实模型。

## 工程约定与改动后同步

* **代码基线**：风格、命名与注释密度对齐周边既有源码，保持稳态代码量与概念数最少；能用类型、Lint 或常驻测试锁死的约束不留口头警告。修改超过 5 个代码文件或净增超过 200 行时，门禁前通读 `git diff` 清理过度设计，交付时汇报净增行数与新增文件数。

| 改动 | 同步动作 |
| --- | --- |
| `crates/protocol/src/` 中的 DTO 或 RPC | 运行 `pwsh -File scripts/check.ps1 -Scope protocol`（导出 `apps/desktop/src/protocol.generated.ts` 并确认无漂移），更新前端调用方 |
| 执行顺序、条件分支、模块关系、失败或停止处理 | 在 `architecture.html` 原图局部更新对应节点、连线和说明；调过布局则看渲染图确认清晰 |
| 模块职责、状态归属、源码入口 | 同步 `docs/architecture.md`（仅记架构边界、数据流与锁序，不写实现与 UI 细节流水账）、`directory.html` |
| 命令或启动步骤 | 同步 `docs/development.md` |

## 检查与交付

* **红线**：生产代码与功能验收只用真实提供方和模型，进程内不做 Mock；常驻测试隔离 `SINGULARITY_HOME`；增删测试先读[测试保留与清理标准](docs/development.md#测试保留与清理标准)，临时测试与脚本验证后即删。
* **分级验收**（通过即止，不反复重跑）：
  * **纯文档 / 配置**：核对与源码事实一致。
  * **代码 / 脚本**：按影响面跑 `pwsh -File scripts/check.ps1 -Scope <rust|protocol|desktop|size|all>`（`rust` / `desktop` / `all` 均含生产源码 ~600 行提醒 / 800 行硬上限及测试增量、巨批/混装黄灯预警；本地 `rust` 自动执行 `cargo fmt`，详情见[开发指南 §2](docs/development.md#2-质量检查与代码格式化)）。
  * **用户可见功能**：按[开发指南 §4–§5](docs/development.md#4-现场验证)及 `AGENTS.local.md` 用 Playwright 验收受影响路径（布局与动效观感写明手测步骤交用户看）；整批功能最终收口或用户要求时更新本地运行包并按需清理构建缓存。
