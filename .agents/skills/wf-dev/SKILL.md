---
name: wf-dev
description: Singularity 开发与交付流程：实机 UI 迭代、check.ps1 门禁、独立审查与 .reviews/ 归档、main/PR 分流。
---

# Singularity 研发流程

## 1. UI/UX 迭代

涉及界面或交互改动时，分两段推进：

1. **纯前端实机预览**：直接改 `apps/desktop/src/` 真实前端（依赖新后端时先用最小占位，不动 `crates/`），不用独立 HTML 原型替代真实界面。
2. **构建、自检与 `Ctrl + R` 刷新**：运行 `npm --prefix apps/desktop run build`。
   * 首次预览：本机交互环境直接启动应用；非交互终端请用户运行 `npm --prefix apps/desktop start`。
   * 涉及明显布局、排版或视觉改动时，优先先渲染截图自检对齐、换行与截断，再通知用户在窗口按 `Ctrl + R` 体验交互与动效。
3. **确认闸门**：用户确认界面与交互满意后，再接通 Rust 后端、协议导出与全套检查。

## 2. 本地检查与收口确认

功能完成后，按受影响范围运行 `pwsh -File scripts/check.ps1 -Scope <rust|protocol|desktop|size|all>`。

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
     * **首轮**：先核对 `docs/decisions.md` 与 `docs/architecture.md#运行边界`；本机单用户桌面应用优先简单显式报错，本地研发脚本优先简短直白；仅报单用户正常界面操作或常见本地环境可实际触发的失效路径或明显结构重复，靠人为破坏或叠加极端条件才能构造的理论边角不报；返回 `## Verdict`（`PASS` / `NEEDS_FIX`）+ 按严重度排序的编号问题（严重度、位置、触发场景或维护代价、最小改法或「显式拒收并记入 `docs/decisions.md`」）。
     * **第 2 轮起**：仅列上一轮已修复的**阻断项**与驳回理由，严格核验阻断项是否解决及是否引入新回归；不将次要项带入复审，邻近历史问题仅作备注不阻断。
2. **按选定通道执行并归档 `.reviews/`**：
   * **Codex 通道**：将简报写入 `$env:TEMP` 下唯一 UTF-8 无 BOM 文件 `$promptFile`，运行：
     ```powershell
     pwsh -File scripts/codex-review.ps1 -PromptFile $promptFile -Kind wf-dev -Round <1|2> -DeletePrompt
     # 附多张截图时改用表达式调用（避免 pwsh -File 拆分数组）：
     # & ./scripts/codex-review.ps1 -PromptFile $promptFile -Kind wf-dev -Round <1|2> -Images @('<图1>','<图2>') -DeletePrompt
     ```
     后台等待超时提醒设 10 分钟；脚本自动归档至 `.reviews/`。
   * **空白上下文子代理通道**：以同一份 4 段简报启动全新只读子代理（不携带主对话历史），收回报告后由主代理直接写入 `.reviews/<yyyy-MM-dd_HHmmss>_wf-dev_r<N>_<shortHead>.md`（YAML 元数据含 `ts, reviewer: "subagent", model, kind, round, head, verdict`，正文同用 `# Review Brief` 与 `# Review Report`）。
3. **核实、决策分流与收敛**：
   * 逐条对照源码按「日常可触发 × 破坏半径 × 不增耦合」核实：误报、风格偏好，以及正常操作难以触发、不伤数据不卡死却需新增协议字段或专用防御分支的建议一律直接驳回并留一句理由（宁留简单边界，拒绝防御性熵增）。
   * 不触碰 `AGENTS.md#决策边界` 的明确小实现失误直接修复并在汇报中列明；命中 `AGENTS.md#决策边界` 或存在结论分歧的条目，暂停用白话向用户确认（含「显式拒收并记入 `docs/decisions.md`」选项）。
   * **仅当确认并修复了阻断项时**，重跑受影响 Scope 后执行第 2 轮增量复审（默认上限 2 轮；第 2 轮仍有阻断修复则在检查通过后向用户确认直接入库或追加 1 轮，批准的额外轮次结束后不再追加；涉及跨端时入库前补跑 1 次 `-Scope all`）。

## 4. 入库、PR 分流与打包

* **中小改动**：确认收口后，直接在 `main` 提交并 `git push origin main`。
* **大改动或依赖变更**（超过 5 个代码文件、净增超过 200 行，或改动 `Cargo.toml` / `Cargo.lock` / `apps/desktop/package*.json`）：切临时分支提交推送，运行 `gh pr create` 开 PR，待 GitHub Actions 的 `Supply-chain policy` 与 `Windows` 全绿后执行 `gh pr merge --squash --delete-branch` 并同步本地 `main`。
* **交付摘要**：汇报检查结果、审查摘要（通道、轮次、修复类目、驳回理由与 `.reviews/` 归档路径）及提交/PR 链接。
* **桌面运行包**：本机试用运行包（路径见 `AGENTS.local.md`，若存在）仅在整批用户可见功能最终收口或用户明确要求时按 `docs/development.md §5` 更新一次。
