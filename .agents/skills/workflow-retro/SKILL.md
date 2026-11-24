---
name: workflow-retro
description: >-
  Manual workflow retrospective for Singularity: review docs/audit-log.md and recent git fix/review
  commits to identify recurring issues (>= 2 tasks), cross-check via local Codex CLI
  (codex/gpt-6.2-sol high), and propose up to 2 hard-rule or workflow improvements for user approval.
---

# 研发流程复盘

默认只读分析，所有规则或检查改动须经用户确认后再实施。

## 执行步骤

1. **收集双来源样本（防漏记）**：
   * 读取 `docs/audit-log.md` 的「审查问题记录」表（重点看状态为 `待观察` 的条目）。
   * 同时扫描近期 Git 历史中因修复缺陷或审查返工产生的提交（补充可能漏写进表格的记录）：
     ```powershell
     git log -n 30 --stat --oneline
     ```
   * 若近阶段真实审查与修复样本不足（有效记录少于 3 条），直接如实汇报样本不足并结束。

2. **识别跨任务高频模式**：
   按“跨不同任务出现 ≥ 2 次”筛选重复问题，分为三类去向：
   * **可机械静态拦截** → 固化进 `scripts/check.ps1`、Clippy 或 oxlint 硬规则；
   * **机器无法扫描的共性盲区** → 补进 `.agents/skills/dev-loop/SKILL.md` 或 `AGENTS.md`；
   * **反复出现的审查误报或已接受取舍** → 补进 `docs/decisions.md`。

3. **Codex 交叉复核（`codex/gpt-6.2-sol` `high`）**：
   将筛出的高频模式与对应提交/证据写入临时 UTF-8 简报，按 `.agents/skills/dev-loop/SKILL.md` 第 3 节的标准 `codex exec` 命令调用一次无痕交叉复核，核对工作区未被改写并清理临时文件。

4. **输出改进建议（最多 2 条）**：
   用白话呈现每条建议（出现次数与来源提交、现在会踩什么坑、固化后的效果与维护代价），通过提问工具请用户决定采纳项；用户确认并实施通过 `scripts/check.ps1` 后，将 `docs/audit-log.md` 对应条目标为 `已固化`。
