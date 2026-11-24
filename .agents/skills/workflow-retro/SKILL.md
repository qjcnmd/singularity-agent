---
name: workflow-retro
description: >-
  Manual workflow retrospective for Singularity: review docs/audit-log.md and recent git fix/review
  commits to identify recurring issues (>= 2 tasks), cross-check via local Codex CLI
  (codex/gpt-6.2-sol high), and propose up to 2 highest-level structural or check improvements for
  user approval.
---

# 研发流程复盘

默认只读分析，所有规则或检查改动须经用户确认后再实施。

## 执行步骤

1. **收集双来源样本（防漏记）**：
   * 读取 `docs/audit-log.md` 的「审查问题记录」表（重点看状态为 `待观察` 的条目）。
   * 扫描近期 Git 历史中因修复缺陷、回滚或审查返工产生的提交：
     ```powershell
     git log -n 30 --stat --oneline
     ```
   * 有效审查与修复样本少于 3 条时，直接汇报样本不足并结束。

2. **归类高频错并按四级优先级选最高可行解法**：
   筛选跨不同任务出现 ≥ 2 次的同类问题，按从强到弱的顺序选**最高能根治的层级**（每条说明为何更高一层不适用）：
   1. **架构消除**：统一单一来源、收紧模块可见性、删除易被抄错的旧路径或死代码，使错误结构上写不出来；
   2. **类型 / Lint / 检查脚本拦截**：用 Rust/TS 类型约束、Clippy、oxlint 或 `scripts/check.ps1` 拦截，报错信息直接指出正确做法；
   3. **行为不变量测试**：补充外部黑盒或 `proptest` 不变量测试（遵守 `docs/development.md` 测试保留标准）；
   4. **文档与规则最后**：仅用于机器无法判定的取舍或流程盲区（写入 `AGENTS.md`、`.agents/skills/dev-loop/SKILL.md` 或 `docs/decisions.md`）；若某条旧文字规则已被前三级彻底封死，一并提议删除。

3. **Codex 交叉复核（`codex/gpt-6.2-sol` `high`）**：
   将筛出的高频模式、证据提交与拟选层级写入临时 UTF-8 简报，按 `.agents/skills/dev-loop/SKILL.md` 第 3 节的标准 `codex exec` 命令调用一次无痕交叉复核，核对工作区未被改写并清理临时文件。

4. **汇报拍板与验证落地（最多 2 条建议）**：
   * 用白话呈现每条建议（出现次数与来源提交、所选层级及理由、实施效果与维护代价），通过提问工具请用户决定采纳项。
   * 用户确认实施后：**新增的检查或测试必须先证明在对应历史错例上能真实报红拦截**，再确保当前仓库通过 `scripts/check.ps1` 与 CI 同一命令；完成后将 `docs/audit-log.md` 对应条目标为 `已固化`。
