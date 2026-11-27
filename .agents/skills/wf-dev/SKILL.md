---
name: wf-dev
description: Singularity 开发与交付流程：开工对齐与子代理分工、实机 UI 迭代、check.ps1 门禁、独立审查与 .reviews/ 归档、main/PR 分流。
---

# Singularity 研发流程

## 1. 开工对齐、子代理分工与 UI/UX 迭代

* **干净基线与范围对齐**：动工前工作区有无关未提交改动先提醒拆分；从开放调研转入写代码前，先用 5 行列清「做什么 / 复用哪些现有模块 / 哪些平行入口要同步 / 不做什么」；涉及跨模块共享状态或对标主流的新交互，先用 3 行讲清参考做法与耦合代价再写代码。
* **子代理「读派写收」**：只读调研/排查鼓励派子代理；同一批写代码最多配 1 个专职实现子代理（提示词必带 `docs/decisions.md`、测试保留标准与已查出的复用/平行入口清单），几十秒小改由主代理直改；送审前主代理先扫 `git diff --stat` 与新增函数，核对复用、平行入口与测试增量。
* **阶段收口清零**：独立后端能力全绿后，若接下来要大改界面或改工作流，先提醒用户“先审/先合入清零，再换主题或开新会话”，避免跨主题滚雪球与长会话反复压缩。

涉及界面或交互改动时，按轻量开发态迭代（**迭代期不反复跑 Release 与 `electron-builder` 打包**）：

1. **视觉比选与真实前端预览**：多方案视觉/大布局比选优先用轻量内联 HTML 交互对比（1 轮拍板）；定案后一律直接改 `apps/desktop/src/` 真实前端（后端未完先最小占位，不动 `crates/`），明显布局/视觉改动先渲染截图自检对齐、换行与截断。
2. **开发态启动与 `Ctrl + R` 刷新**：代理退出占锁实例、确认 Debug 后端就绪并运行 `npm --prefix apps/desktop run build`。
   * 首次预览：**请用户在本机终端运行 `npm --prefix apps/desktop start`**（连真实后端与 `~/.singularity/`；Antigravity 无交互终端勿盲拉 GUI 窗口以免不可见并占锁）。
   * 后续微调：纯前端改动只重跑 `build` 通知用户在窗口按 `Ctrl + R` 刷新；改了 Rust 后端则增量编 Debug + `prepare:runtime` 请用户重启。
3. **确认闸门**：用户确认界面与交互满意后，再接通剩余后端/协议导出并进入门禁。

## 2. 本地检查与收口确认

按受影响范围运行 `pwsh -File scripts/check.ps1 -Scope <rust|protocol|desktop|size|all>`（排错先跑最小 Scope 如 `-Scope rust` / `-Scope desktop -SkipE2E`，全绿后补跑完整 Scope）；若 `size` 亮出**测试增量、巨批或业务/规则混装黄灯**，先向用户说明原因。

* **用户可见改动自动实机 QA**：涉及用户可见功能且门禁全绿后，按 [wf-qa](../wf-qa/SKILL.md) 模式 A 派 1 个独立空白上下文 QA 子代理做 1 轮真实模型隔离探索验收（覆盖本次改动、关联老路径、控制台/日志与关键截图），修完确认的功能缺陷后再进入收口确认。
* 门禁与实机验收通过后，简要汇报改动行为、实测发现与手测项。若用户未在开场指定审查方式，**在同一次收口确认中**请用户选定：
  * **Codex 独立审查**（默认推荐：核心业务代码、大改动、需要跨模型复核时）；
  * **空白上下文子代理审查**（默认推荐：改工作流/脚本/规则自身、当前已在 Codex 会话内、或反过度设计体检时）；
  * **免审直接入库**（纯文档/配置/样式微调或明显局部小改时放首位）。

## 3. 独立审查与 `.reviews/` 归档

1. **结构化审查简报**（只写仓库外信息，不贴大段源码或复述 `AGENTS.md`）：
   * `## 目标与边界`：1～2 句目标、确认范围及不做范围（除 `docs/decisions.md` 外）；
   * `## 审查范围`：基准（如 `git diff origin/main`）与关键文件；
   * `## 已跑验证`：已通过的 `check.ps1` Scope 与实机/运行验证项；
   * `## 审查要求`：
     * **首轮**：核对 `docs/decisions.md` 与 `docs/architecture.md#运行边界`；单用户桌面应用优先简单显式报错，本地脚本优先直白；仅报正常界面操作或常见本地环境可达的失效路径或明显结构重复，脚本故障注入、破坏内部存储或叠加极端前提构造的边角一律不报；返回 `## Verdict`（`PASS` / `NEEDS_FIX`）+ 按严重度排序的编号问题（严重度、位置、触发路径或维护代价、最小改法或「显式拒收并记入 `docs/decisions.md`」）。
     * **第 2 轮**：**仅列 R1 已修复的阻断项与驳回理由，严禁将非阻断项带入复审**；核验阻断项是否解决及有无新回归，邻近问题仅备注不阻断。
2. **执行并归档 `.reviews/`**：
   * **Codex 通道**：简报写 `$env:TEMP` 唯一 UTF-8 无 BOM 文件 `$promptFile`，运行（默认 `-Effort high` 且禁桌面 hooks，自动记真实 model/effort 并归档 `.reviews/`；仅需深度核查时显式传 `-Effort xhigh`）：
     ```powershell
     pwsh -File scripts/codex-review.ps1 -PromptFile $promptFile -Kind wf-dev -Round <1|2> -DeletePrompt
     # 附多图时改用表达式调用：& ./scripts/codex-review.ps1 -PromptFile $promptFile -Kind wf-dev -Round <1|2> -Images @('<图1>','<图2>') -DeletePrompt
     ```
     后台等待超时提醒设 10 分钟。
   * **空白上下文子代理通道**：以同一 4 段简报启动全新只读子代理，收回后主代理写入 `.reviews/<yyyy-MM-dd_HHmmss>_wf-dev_r<N>_<shortHead>.md`（YAML 含 `ts, reviewer: "subagent", model, kind, round, head, verdict`，正文同用 `# Review Brief` 与 `# Review Report`）。
3. **去黑盒、客观分流与 2 轮熔断**：
   * 收到报告**严禁照单盲修**；**每轮审查后必须在回复里贴出极简白话清单**（修了哪几条现象、驳回哪几条过度建议及理由、`.reviews/` 归档链接），绝不做黑盒。
   * **硬尺子分流**：≤ 30 行且不加新机制、不改既有体验的明确真 Bug 直接修；单条预计 > 30 行、需新写解析器/拦截器/协议/状态或属罕见边角，按 `docs/decisions.md` 与运行边界优先直接驳回，确有取舍则翻译成白话（触发场景 + 行数代价 + 拒收记入 `docs/decisions.md` 选项）交用户拍板。
   * **2 轮硬上限与同特性熔断**：仅确认并修复了阻断项才跑第 2 轮（重跑受影响 Scope 后复审；硬上限 2 轮，不再自动追加第 3 轮；涉及跨端入库前补跑 1 次 `-Scope all`）。若**同一子特性连续 2 轮触发阻断或重要修补**，立即停手并首选推荐砍掉或降级为简单版（记入 `docs/decisions.md`）。

## 4. 入库、PR 分流与打包

* **单主题提交**：不在业务 PR 中混入无关工作流/规则修改（流程优化另提独立提交或由 `wf-retro` 集中收口）。
* **中小改动**：确认收口后直接提交 `main` 并 `git push origin main`。
* **大改动或依赖变更**（> 5 个代码文件、净增 > 200 行或改 `Cargo.toml`/`Cargo.lock`/`apps/desktop/package*.json`）：切分支提交推送并 `gh pr create`，待 `Supply-chain policy` 与 `Windows` 全绿后 `gh pr merge --squash --delete-branch` 并同步本地 `main`。
* **交付摘要与运行包**：汇报检查结果、白话审查摘要（通道、轮次、修复/驳回类目与 `.reviews/` 路径）及提交/PR 链接；桌面运行包（见 `AGENTS.local.md`）仅在整批用户可见功能最终收口或用户要求时按 `docs/development.md §5` 更新一次。
