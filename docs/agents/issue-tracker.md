# Issue tracker: GitHub

本仓库的任务需求、缺陷、验收条件与交付记录以 GitHub Issues 为唯一权威来源。所有 Issue 操作使用 `gh` CLI；在本仓库克隆内执行时 `gh` 自动识别仓库（否则用 `git remote -v` 确认）。

不维护本地 Issue 数据库、任务队列或工作流引擎；Issue 管理不进入 `AgentLoop`、`AppServer` 或公共 JSON-RPC 协议。

## 常用操作

| 操作 | 命令 |
| --- | --- |
| 创建（模板） | `gh issue create --template feature.yml --title "..."` |
| 创建（直接写正文） | `gh issue create --title "..." --body-file issue.md --label "task,needs-triage"` |
| 创建为子 Issue | `gh issue create ... --parent <父Issue编号>` |
| 创建时声明阻断 | `gh issue create ... --blocked-by <编号,...>` |
| 读取 | `gh issue view <编号> --comments` |
| 列表 | `gh issue list --state open`，可加 `--label "ready-for-agent"` 过滤 |
| 更新正文/标题 | `gh issue edit <编号> --body-file issue.md` / `--title "..."` |
| 评论 | `gh issue comment <编号> --body "..."` |
| 加/删标签 | `gh issue edit <编号> --add-label "..."` / `--remove-label "..."` |
| 关闭 | `gh issue close <编号> --reason completed --comment "..."`（或 `--reason not_planned`） |
| 查父 Issue | `gh api repos/qjcnmd/singularity-agent/issues/<编号>/parent` |

## Issue 模板

模板位于 `.github/ISSUE_TEMPLATE/`，均为 GitHub Issue Form（YAML）：

| 模板 | 类型标签 | 适用场景 |
| --- | --- | --- |
| `bug_report.yml` | `bug` | 实际行为与预期不符 |
| `feature.yml` | `enhancement` | 新增能力或用户可感知的行为改进 |
| `task.yml` | `task` | 重构、CI、文档、工具链、治理等工程工作 |

字段含义（三类模板共用同一套语义；标注“可选”的字段没有内容时写“无”）：

| 字段 | 含义 |
| --- | --- |
| 问题或目标 / 现象 | 核心意图或缺陷现象，一段话说清 |
| 背景与已确认事实 / 复现步骤与环境 | 已验证的事实与来源；推断和假设必须单独标注，不写成事实 |
| 期望行为（仅缺陷） | 正确行为及其依据（文档、协议、既有约定） |
| 用户价值 | 为维护者或产品解决什么问题；不做会失去什么 |
| 实现范围 | 本次要做什么，可执行的边界 |
| 非目标 | 明确不做什么；相邻改进另开 Issue，不让一个 Issue 无限扩张 |
| 验收标准 | 逐条、可检查的完成条件，是关闭 Issue 的依据 |
| 验证或测试计划 | 如何证明验收标准成立：定向测试、真实调用链验证、运行的检查 |
| 风险与安全边界 | 涉及的安全边界、失败模式与回退方式 |
| 依赖关系 | 前置条件与 blocked by / blocking 的 Issue，用 #编号 引用 |
| 父子 Issue 关系 | 从哪个父 Issue 拆出、计划拆出哪些子 Issue，用 #编号 引用 |

模板会自动打上类型标签和 `needs-triage`。正文之外的元数据（优先级、状态、父子关系）用标签和 GitHub 原生关联表达，不重复写进正文标题。

## 标签体系

标签是唯一的状态与分类事实源。完整标签表：

| 维度 | 标签 | 含义 |
| --- | --- | --- |
| 类型 | `bug` / `enhancement` / `task` | 缺陷 / 功能 / 工程任务，三选一 |
| 类型（辅助） | `documentation` / `question` | 文档改进 / 使用问题 |
| Triage 结果 | `duplicate` / `invalid` / `wontfix` | 重复（引用原 Issue）/ 不成立 / 不实施，配合关闭使用 |
| 优先级 | `P0` / `P1` / `P2` | 紧急（阻断关键路径）/ 高（近期必须完成）/ 普通；最多一个 |
| 准入 | `needs-triage` | 维护者尚需评估 |
| 准入 | `needs-info` | 等待报告者补充信息 |
| 可执行 | `ready-for-agent` | 规格完整，可交由编码代理执行 |
| 可执行 | `ready-for-human` | 规格完整，需要人工实施 |
| 状态 | `in-progress` | 正在实施中 |
| 状态 | `blocked` | 被其他 Issue 或外部因素阻断 |

规则：

- `ready-for-agent` 与 `ready-for-human` 互斥；`needs-triage` / `needs-info` 与 `ready-for-*` 互斥。
- 进入 `in-progress` 时移除 `ready-for-*`，避免“已可领取”与“已被领取”同时出现。
- 优先级不再写进标题（历史 Issue 的 `P1:` 标题前缀保留原样，不迁移标题）。
- 五类 triage 角色与 `docs/agents/triage-labels.md` 保持一致；技能提到角色名时使用该表中的实际标签。

## 生命周期

```
新建(needs-triage) → 待补充(needs-info) → 已确认(有类型标签，字段完整)
  → 可执行(ready-for-agent / ready-for-human)
  → 进行中(in-progress)
  → （被阻断时加 blocked，解除后移除）
  → 已通过 PR 交付（PR 合并）
  → 已关闭（GitHub closed）
```

| 状态 | 标签组合 | 进入条件 |
| --- | --- | --- |
| 待评估 | `needs-triage` | 新建默认状态 |
| 待补充 | `needs-info` | 信息不足以评估或执行 |
| 已确认 | 类型标签，无 `needs-*` | 事实核实、字段完整 |
| 可执行 | `ready-for-agent` 或 `ready-for-human` | 验收标准与验证计划完整、无未解除阻断 |
| 进行中 | `in-progress` | 有人或代理开始实施 |
| 被阻断 | `blocked` | blocker Issue 未关闭或外部条件不满足；评论记录原因 |
| 已交付 | PR 合并后关闭 | 见下方关闭条件 |

## 关闭条件

只有满足以下条件之一才可关闭：

1. **完成**：验收标准逐条满足；验证已实际执行，证据（测试输出、真实调用链结果、commit）记录在 Issue 评论或关联 PR 中；对应 PR 已合并（PR 正文用 `Fixes #编号` 关联）。关闭用 `--reason completed`。
2. **不实施**：明确决定不做，加 `wontfix` 并说明原因；`--reason not_planned`。
3. **重复或不成立**：加 `duplicate`（引用原 Issue）或 `invalid`（说明不成立的理由）；`--reason not_planned`。

不允许仅根据代理的文字声明关闭 Issue；声明必须附带可复核的证据。发现新的相邻缺陷或后续需求时单独开 Issue，不扩大当前 Issue 的关闭条件（见 `AGENTS.md` 验收边界）。

## 父子 Issue 与依赖

- **何时拆分**：一个 Issue 只在当前 blocker / 当前改动的 regression 范围内收敛；相邻缺陷、后续需求、可独立验收的子目标拆为子 Issue。子 Issue 必须自带完整的验收标准与验证计划，能独立关闭。
- **创建子 Issue**：`gh issue create ... --parent <父编号>`；已存在的 Issue 可用 REST 关联：
  `gh api repos/qjcnmd/singularity-agent/issues/<父编号>/sub_issues -X POST -f sub_issue_id=<子Issue的数据库id>`（子 Issue 的数据库 id 用 `gh api repos/qjcnmd/singularity-agent/issues/<子编号> --jq .id` 获取）。
- **正文约定**：子 Issue 正文的“父子 Issue 关系”写 `从 #N 分出`；父 Issue 评论中列出 `子 Issue：#a #b`。
- **依赖与阻断**：创建时用 `--blocked-by <编号>` 声明硬依赖；运行期被阻断时加 `blocked` 标签并评论说明原因与解除条件，解除后移除。被 `blocked` 的 Issue 不得进入 `in-progress`。

## 编码代理执行前检查（必须全部满足）

1. Issue 带 `ready-for-agent` 标签，或由维护者在评论中明确指派。
2. 没有 `blocked` 标签；`blocked by` 的 Issue 全部已关闭。
3. 验收标准与验证计划完整、可执行；缺失时先补充并等待确认，不凭猜测开工。
4. 已确认基线 commit、工作分支与父 Issue 范围一致。
5. 已阅读 `AGENTS.md` 的安全、验收与记录要求；涉及架构时已阅读 `docs/singularity.md` 相关部分。

不满足任一条件时：记录缺口并停在“待补充 / 被阻断”，不开始实施。

## 完成后的记录

- PR 正文写 `Fixes #编号` 并列出关联 commit。
- 按 `AGENTS.md` 的任务记录规则，在 Issue 上最多追加两次简短评论：确认根因与处理计划一次；完成后记录实际修改、方案理由、验证结果、未验证范围与关联 commit 一次。
- 证据必须是实际运行产物（测试输出、真实调用链结果、Evaluation 记录），不是计划或声明。
- 验收满足且 PR 合并后，用 `gh issue close <编号> --reason completed --comment "..."` 关闭。

## Skill routing

当 skill 说 “publish to the issue tracker”，创建一个 GitHub Issue（选择合适的模板）。
当它说 “fetch the relevant ticket”，运行 `gh issue view <编号> --comments`。
