---
name: dev-loop
description: >-
  Singularity development workflow: real-app Ctrl+R UI/UX iteration, scripts/check.ps1 gate,
  post-acceptance local Codex CLI review (codex/gpt-6.2-sol high), and tiered main/PR delivery.
---

# Singularity 研发流程

## 1. UI/UX 迭代

涉及 Singularity 界面或交互改动时，分两段推进：

1. **纯前端实机预览**：直接改 `apps/desktop/src/` 真实前端（依赖新后端时先用最小占位，不动 `crates/`），不用独立 HTML 原型替代真实界面。
2. **构建与 `Ctrl + R` 刷新**：运行 `npm --prefix apps/desktop run build`。
   * 首次预览：本机交互环境直接启动应用；非交互终端给用户运行 `npm --prefix apps/desktop start`（必要时附关键截图）。
   * 后续微调：改完只跑 `npm --prefix apps/desktop run build`，通知用户在窗口按 `Ctrl + R` 刷新。
3. **确认闸门**：用户明确确认界面与交互满意后，再接通 Rust 后端、协议导出与全套检查。

## 2. 本地检查与用户验收（审查之前）

功能实现完成后，按受影响范围运行 `pwsh -File scripts/check.ps1 -Scope <rust|protocol|desktop|size|all>`。

* 门禁全绿后，先向用户简要汇报改动行为与手测项，**等待用户确认效果满意、准备收口入库**后再触发代码审查，避免未定稿前反复重复审查（用户明确要求直接审完收口时除外）。

## 3. 收口审查（`codex/gpt-6.2-sol` `high`）

用户确认准备收口后，按客观门槛执行：

* **跳过审查**：纯文档、配置、纯 CSS 样式或文案微调，或局部修改 ≤ 2 个代码文件且增删合计 ≤ 30 行，且未触及 `crates/(protocol|model|agent|mcp)`、`crates/app/src/runtime/`、测试、依赖或 `.github/` / `scripts/`。
* **自动执行本地 Codex 审查**：其余代码改动一律调用 [scripts/codex-review.ps1](../../../scripts/codex-review.ps1)（用户主动指定免审或换子代理时按用户要求执行）：
  1. 用文件写入工具将精简中文审查简报写入 `$env:TEMP` 下带唯一后缀的 UTF-8（无 BOM）文件 `$promptFile`（只写仓库外信息，不贴大段源码或复述 `AGENTS.md`）：
     * 本次目标、已确认边界与不做范围、审查锚点（如 `git diff origin/main` / 指定提交范围）与已通过检查；
     * **首轮要求**：先核对 `docs/decisions.md` 与 `docs/architecture.md#运行边界`；阻断项须给出本机 Windows 单用户边界内可复现失效路径或明显结构重复，次要项不触发返工；返回简洁中文结论与按严重度排序的问题列表（位置、失效场景/维护代价、最小改法）。
     * **复审要求（第 2 轮起）**：保留精简目标与边界，补充上一轮已修复项与驳回项及理由，**严格限定核验两件事：① 上一轮确认项是否已彻底解决；② 本轮修复本身是否引入新回归**——新发现的邻近历史问题仅作备注留待下次体检，不作为阻断打回。
  2. 单行调用（默认无痕、`codex/gpt-6.2-sol` `high`、禁用联网搜索、防挂起并自动清理临时文件）：
     ```powershell
     pwsh -File scripts/codex-review.ps1 -PromptFile $promptFile -DeletePrompt
     # 附截图验收改用表达式调用（避免 pwsh -File 拆分数组）：
     # & ./scripts/codex-review.ps1 -PromptFile $promptFile -Images @('<图1>','<图2>') -DeletePrompt
     # 后台等待超时提醒设 10 分钟，超时先查回显的 LOG_FILE/OUT_FILE
     ```
  3. **独立核实、复杂度止损与收敛**：
     * 逐条对照源码核实：属实修复，误报/风格偏好忽略并留一句理由，涉及需求取舍交用户拍板。
     * **复杂度止损**：若单个边角问题的修复预计净增 > 100 行生产代码或需新建复杂状态机，**先暂停硬写**，用提问工具向用户给出「完整支持」与「显式报错拒绝该边界并记入 `docs/decisions.md`」二选一拍板。
     * **按影响面重跑验证**：中间修补轮次只重跑受影响 Scope（如仅改 Rust 时跑 `-Scope rust`），收敛后最终入库前再按改动面补跑 1 次 `-Scope all`（涉及跨端时）。
     * 确认并修复的阻断项，在 `docs/audit-log.md` 的「审查问题记录」表各追加一行（状态标 `待观察`）。
     * **严格轮次上限**：第 1 轮无阻断项 → 直接入库；确认并修复了阻断项 → 一次性修完并重跑受影响检查后执行第 2 轮增量复审（默认上限 2 轮：第 2 轮无阻断项则直接交付；有未决分歧交用户拍板；若第 2 轮仍确认并修复了阻断项，跑完检查后用提问工具询问用户直接入库还是追加 1 轮复审；**用户批准的额外轮次结束后不再擅自追加确认轮**）。

## 4. 入库、PR 分流与打包

* **中小改动**：审查收敛并确认后，直接在 `main` 提交并 `git push origin main`。
* **大改动或依赖变更**（超过 5 个代码文件，或改动 `Cargo.toml` / `Cargo.lock` / `apps/desktop/package*.json`）：切临时分支提交推送，运行 `gh pr create` 开 PR，待 GitHub Actions 的 `Supply-chain policy` 与 `Windows` 全绿后执行 `gh pr merge --squash --delete-branch` 并切回同步本地 `main`。
* **纯巡检日志补提交**：PR 合入后如仅更新 `docs/audit-log.md`，直推 `main` 的提交信息附 `[skip ci]` 避免空跑 CI。
* **交付摘要**：汇报检查结果、3～5 行审查摘要（模型档位、轮次、修复类目及全部驳回理由）与入库提交/PR 链接。
* **桌面运行包**：本机试用运行包（路径见 `AGENTS.local.md`，若存在）仅在整批用户可见功能最终收口或用户明确要求时按 `docs/development.md §5` 更新一次。
