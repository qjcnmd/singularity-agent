---
name: wf-dev
description: Singularity 开发与交付流程：实机 UI 迭代、check.ps1 门禁、独立审查与 .reviews/ 归档、main/PR 分流。
---

# Singularity 研发流程

## 1. 动工对齐与 UI/UX 迭代

* **干净基线与架构先对齐**：动工前若工作区已有无关未提交改动，先提醒拆分/提交再改；涉及跨模块共享状态的新功能或对标主流桌面端的新交互，动工前先用 3 行讲清主流参考做法与耦合代价，经用户拍板再写代码。

涉及界面或交互改动时，按轻量开发态迭代（**迭代期不反复跑 Release 与 `electron-builder` 打包**）：

1. **真实前端实机预览**：直接改 `apps/desktop/src/` 真实界面（依赖未完成的新后端时先用最小占位，不动 `crates/`），不用独立 HTML 原型替代；涉及明显布局、排版或视觉改动先渲染截图自检对齐、换行与截断。
2. **开发态快启与 `Ctrl + R` 刷新**：运行 `npm --prefix apps/desktop run build`。
   * 首次预览：本机交互环境先退出占锁实例，确认 Debug 后端已就绪后直接拉起开发态窗口（`npm --prefix apps/desktop start`，连真实后端与 `~/.singularity/`，可正常发消息对话；非交互终端请用户运行）。
   * 后续微调：纯前端改动只重跑前端 `build`，通知用户在窗口按 `Ctrl + R` 刷新（不断开后端）；若改了 Rust 后端，增量编 Debug 版 + `prepare:runtime` 重启开发态窗口。
3. **确认闸门**：用户确认界面与交互满意后，再接通剩余后端/协议导出并进入门禁与收口。

## 2. 本地检查与收口确认

功能完成后，按受影响范围运行 `pwsh -File scripts/check.ps1 -Scope <rust|protocol|desktop|size|all>`（局部排错先跑最小 Scope 如 `-Scope rust` / `-Scope desktop -SkipE2E`，全绿后再补跑完整 Scope，避免反复重跑全套 E2E）。

* 门禁全绿后，先向用户简要汇报改动行为与手测项。若用户未在开场指定审查方式，**在同一次收口确认中**请用户选定下一步：
  * **Codex 独立审查**（默认推荐：核心业务代码、大改动、需要跨模型复核时）；
  * **空白上下文子代理审查**（默认推荐：改工作流/脚本/规则自身、当前已在 Codex 会话内、或反过度设计体检时）；
  * **免审直接入库**（纯文档/配置/纯样式微调或明显局部小改时放首位）。

## 3. 独立审查与 `.reviews/` 归档

仓库内统一走本技能的审查与 `.reviews/` 归档流程：

1. **结构化审查简报**（只写仓库外信息，不贴大段源码或复述 `AGENTS.md`）：
   * `## 目标与边界`：1～2 句目标、确认范围及不做范围（除 `docs/decisions.md` 外）；
   * `## 审查范围`：基准（如 `git diff origin/main` 或提交范围）与关键文件；
   * `## 已跑验证`：已通过的 `check.ps1` Scope 与实机/运行验证项；
   * `## 审查要求`：
     * **首轮**：先核对 `docs/decisions.md` 与 `docs/architecture.md#运行边界`；本机单用户桌面应用优先简单显式报错，本地研发脚本优先简短直白；仅报单用户正常界面操作或常见本地环境可实际触发的失效路径或明显结构重复，靠脚本故障注入、人为破坏内部存储或叠加极端前提构造出的“实测”边角一律不报；返回 `## Verdict`（`PASS` / `NEEDS_FIX`）+ 按严重度排序的编号问题（严重度、位置、正常操作触发路径或维护代价、最小改法或「显式拒收并记入 `docs/decisions.md`」）。
     * **第 2 轮起**：**仅列上一轮已修复的阻断项与驳回理由，严禁将非阻断项带入复审以防扩散**；严格核验阻断项是否解决及是否引入新回归，邻近历史问题仅作备注不阻断。
2. **按选定通道执行并归档 `.reviews/`**：
   * **Codex 通道**：将简报写入 `$env:TEMP` 下唯一 UTF-8 无 BOM 文件 `$promptFile`，运行：
     ```powershell
     pwsh -File scripts/codex-review.ps1 -PromptFile $promptFile -Kind wf-dev -Round <1|2> -DeletePrompt
     # 附多张截图时改用表达式调用（避免 pwsh -File 拆分数组）：
     # & ./scripts/codex-review.ps1 -PromptFile $promptFile -Kind wf-dev -Round <1|2> -Images @('<图1>','<图2>') -DeletePrompt
     ```
     后台等待超时提醒设 10 分钟；脚本自动归档至 `.reviews/`。
   * **空白上下文子代理通道**：以同一份 4 段简报启动全新只读子代理（不携带主对话历史），收回报告后由主代理直接写入 `.reviews/<yyyy-MM-dd_HHmmss>_wf-dev_r<N>_<shortHead>.md`（YAML 元数据含 `ts, reviewer: "subagent", model, kind, round, head, verdict`，正文同用 `# Review Brief` 与 `# Review Report`）。
3. **先裁定再动代码、决策分流与收敛**：
   * 收到审查报告后**严禁立刻照单改代码**，必须先对照源码按「正常操作可达 × 破坏半径 × 不增耦合」逐条定性（留 / 驳与理由）：误报、风格偏好、脚本故障注入构造的极端场景，以及正常操作难以触发、不伤数据不卡死却需新增协议 DTO/字段、新锁或专用防御分支的建议，一律直接驳回并留一句理由（宁留简单边界，拒绝防御性熵增）。
   * 不触碰 `AGENTS.md#决策边界` 的明确小实现失误直接修复并在汇报中列明；命中 `AGENTS.md#决策边界`（含需扩协议或改交互来修非阻断项）或存在结论分歧的条目，暂停用白话向用户确认（含「显式拒收并记入 `docs/decisions.md`」选项）。
   * **仅当确认并修复了阻断项时**，重跑受影响 Scope 后执行第 2 轮增量复审（默认上限 2 轮；第 2 轮仍有阻断修复则在检查通过后向用户确认直接入库或追加 1 轮，批准的额外轮次结束后不再追加；涉及跨端时入库前补跑 1 次 `-Scope all`）。

## 4. 入库、PR 分流与打包

* **单主题提交**：不在业务改动 PR 中顺手混入无关工作流/规则修改（确需调整流程另提独立提交或由 `wf-retro` 集中收口）。
* **中小改动**：确认收口后，直接在 `main` 提交并 `git push origin main`。
* **大改动或依赖变更**（超过 5 个代码文件、净增超过 200 行，或改动 `Cargo.toml` / `Cargo.lock` / `apps/desktop/package*.json`）：切临时分支提交推送，运行 `gh pr create` 开 PR，待 GitHub Actions 的 `Supply-chain policy` 与 `Windows` 全绿后执行 `gh pr merge --squash --delete-branch` 并同步本地 `main`。
* **交付摘要**：汇报检查结果、审查摘要（通道、轮次、修复类目、驳回理由与 `.reviews/` 归档路径）及提交/PR 链接。
* **桌面运行包**：本机试用运行包（路径见 `AGENTS.local.md`，若存在）仅在整批用户可见功能最终收口或用户明确要求时按 `docs/development.md §5` 统一打包更新一次。
