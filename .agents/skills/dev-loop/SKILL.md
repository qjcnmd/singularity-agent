---
name: dev-loop
description: >-
  Single-repo automated development workflow for Singularity: async read-only subagents,
  single-writer serial commit queue, Pro-first model routing, cross-model code review,
  one-page human approval card, PR merge, and periodic project gardening / meta-system review.
---

# 自动化研发编排流程（`handoff_version: v1`）

## 1. 协同流程

改动统一沿以下路径推进：**主会话调研与讨论对齐需求 → 单目录写码队列逐项提交 → 队列落盘后审查 → 一页卡拍板并经 PR 合入受保护的 `main`**。

* **只读并发**：调研（`role="research"`）、双盲方案（`role="design"`）、写队列落盘后的审查（`role="review"`）均通过 `delegate_task(isolation="shared", mode="async")` 并行执行。
* **单写者串行队列**：写代码只在主仓库当前特性分支进行，同一时间仅运行 1 个写任务（改动前执行 `git switch -c <分支名>`，不在本地 `main` 累积未合入提交）。
* **写完即提交、不停等审查**：任务完成并通过 `pwsh -File scripts/workflow.ps1 gate -Scope auto` 后直接 `git commit` 落成独立提交，释放写席位继续下一个排队任务；待队列全部落盘后再统一审查。
* **强依赖任务续接**：后续任务直接依赖前序任务刚改动的协议或核心结构时，通过 `t3_thread_send` 续交给同一写码子代理。
* **UI/UX 迭代**：直接在当前特性分支修改 `apps/desktop/src/` 并提供真实应用预览或截图；用户确认效果后再提交进入审查收口。

## 2. 模型与账号路由

* **主前台**：`gemini-4-neon`（`high` / `xhigh`）或 `claude-opus-5-5`（`high`）。
* **后台子代理与外环**：
  * `gemini-4-neon` 统一 `thought_level: "high"`，优先在 `wbxczncx` 与 `yyx9850` 两个 Pro 实例间轮换；任一限流或中断时切换另一 Pro，均受限时切换 Ultra（`antigravity`）并在原分支改动上续跑。
  * `claude-opus-5-5` 统一 `thought_level: "high"`，仅调用 Ultra（`providerInstanceId: "antigravity"`）。
* **大结构改动背靠背双盲**（涉及 `crates/protocol`、会话格式、核心执行环或新增依赖）：
  * 盲派互补模型独立给方案（前台为 `gemini-4-neon` 时派 `claude-opus-5-5` `high` / `antigravity`，为 `claude-opus-5-5` 时派 `gemini-4-neon` `high` / 优先 `wbxczncx` 或 `yyx9850`），不传入主前台方案。
  * 合并时先取共识，仅将实质分歧整理为 1～3 项白话选择交用户拍板，并排列出各方案的预计改动文件数、新增概念数与最简替代做法；定案后精简多余抽象。

## 3. 派单简报模板（`[handoff:v1]`）

派发子代理时仅包含仓库外的必要信息：
1. `[handoff:v1]` 目标、已确认边界与不做范围；
2. 已有调研结论或关键线索（如有）；
3. 写码完成后运行 `pwsh -File scripts/workflow.ps1 gate -Scope auto` 拿绿票并 `git commit`。

## 4. 审查、拍板卡与 PR 合入

* **触发时机**：纯文档或纯配置改动免审直接开 PR；生产代码在写队列全部提交且工作区干净后审查（用户指定免审的除外）。
* **合审与并行分审**：待审总改动量 ≤ 1000 行时派 1 个审查代理统一审查；超过 1000 行时按提交拆分，并行派多个只读审查代理各审 `git show <sha>`。
* **审查调用**：通过 `delegate_task(role="review", isolation="shared", mode="async")` 执行。常规/局部改动优先 `codex/gpt-6.2-sol`（`providerInstanceId: "codex"`，常规 `high` / 大改 `xhigh`）；跨多模块大重构或 `6.2-Sol` 受限时派空白上下文 `gemini-4-neon`（`thought_level: "high"`，优先 `wbxczncx` / `yyx9850`），大重构可互补双审。
* **审查要求**：简报中注明先核对 `docs/decisions.md` 与 `docs/architecture.md#运行边界`；`blocker` 须给出本机 Windows 单用户边界内可复现失效路径或明显结构重复，`nit` 不触发返工；返回 JSON 问题清单（每项含 `id`、`severity`、`pattern`、`file`、`summary`）。
* **核实、入账与收敛**：执行侧逐条核实源码并标记 `fixed` / `rebutted`（须附一句白话理由）/ `escalated`（`nit` 可标 `ignored`），运行 `pwsh -File scripts/workflow.ps1 record-review -InputPath <json> -Reviewer <model> -HandoffVersion v1 -Round <n>` 写入 `~/.singularity-dev/ledger.jsonl`；所有确认的 `blocker` 一次性修复并重跑完整 `gate`；最多 2 轮，未决分歧交用户拍板。
* **一页拍板卡与合入**：
  1. 运行 `pwsh -File scripts/workflow.ps1 radar` 获取敏感区与体量摘要；
  2. 向用户呈现中文拍板卡：① 行为变化、实机截图与手测项；② 门禁与雷达结果（含审后净增 > 50 行提醒）；③ 审查汇总与全部 `rebutted` 条目及理由；④ PR 链接与云端 CI 状态；
  3. 用户确认后推送分支，`pull_request_create` 开 PR 并 `watch_pull_request` 等待 `Supply-chain policy` 与 `Windows` 全绿后合入 `main`，随后 `git switch main && git pull`。

## 5. 周期外环（定时与手动共用）

* **项目园艺巡检（目标：仓库代码）**：基于近 14 天 `git log` 热改文件排查死代码、冗余抽象、`architecture.html` 与 `directory.html` 漂移，并补审跳过即时审查的近期提交；当前执行代理完成初查后，调用一次 `codex/gpt-6.2-sol`（`high`）交叉复核，仅对确认项切分支产出孤立清理 PR 交用户拍板。
* **外层系统日志复盘（目标：研发流程）**：运行 `pwsh -File scripts/workflow.ps1 digest -Days 14` 汇总 `~/.singularity-dev/ledger.jsonl` 并细读近期原始样本，由当前执行代理初判后调用一次 `codex/gpt-6.2-sol`（`high`）交叉复核，输出高置信度改进建议（高频机械错误 ≥ 2 次固化为确定性检查；共性语义盲区优化本文件派单模板并升级 `handoff_version`；高频审查误报校准 `docs/decisions.md` 或审查简报），经用户确认后实施。
