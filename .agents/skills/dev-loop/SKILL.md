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
* **自动执行本地 Codex 审查**：其余代码改动一律调用本地 `codex exec`（用户主动指定免审或换子代理时按用户要求执行）：
  1. 将精简审查简报写入临时 UTF-8 文件 `$promptFile`（只写仓库外信息，不贴大段源码或复述 `AGENTS.md`）：
     * 本次目标、已确认边界与不做范围；
     * 审查锚点（如 `git diff origin/main` / 指定提交范围）与已通过的检查结果；
     * 要求：先核对 `docs/decisions.md` 与 `docs/architecture.md#运行边界`；阻断项须给出本机 Windows 单用户边界内可复现失效路径或明显结构重复，次要项不触发返工；返回简洁中文结论与按严重度排序的问题列表（位置、失效场景/维护代价、最小改法）。
     * 第 2 轮复审简报保留精简目标与边界，并补充第 1 轮已修复项与驳回项及理由，聚焦增量复核。
  2. 无痕执行（附图验证可在 `-- -` 前重复加 `-i "<图片路径>"`）：
     ```powershell
     $utf8 = [System.Text.UTF8Encoding]::new($false)
     $prev = $OutputEncoding; $OutputEncoding = $utf8
     $before = (git status --porcelain --untracked-files=all) -join "`n"
     try {
         [System.IO.File]::ReadAllText($promptFile, $utf8) | codex exec `
             -m "codex/gpt-6.2-sol" `
             -c 'model_reasoning_effort="high"' `
             -c 'notify=[]' `
             -c 'features.memories=false' `
             --ephemeral --skip-git-repo-check `
             -o $outFile -- - *> $logFile
     } finally { $OutputEncoding = $prev }
     ```
     正常完成时仅读取 `$outFile` 正文并核对 `git status` 未被改写；退出码非 0 或 `$outFile` 为空时输出 `$logFile` 尾部并如实汇报；结束后删除全部临时文件。
  3. **独立核实与收敛**：
     * 逐条对照源码核实：属实直接修复，误报/风格偏好忽略并留一句理由，涉及需求取舍交用户拍板。
     * 确认并修复的阻断项，在 `docs/audit-log.md` 的「审查问题记录」表各追加一行（状态标 `待观察`）。
     * 第 1 轮无阻断项 → 直接入库交付；确认并修复了阻断项 → 一次性修完、重跑 `scripts/check.ps1` 后执行第 2 轮增量复审（最多 2 轮；未决分歧交用户拍板）。

## 4. 入库、PR 分流与打包

* **中小改动**：审查收敛并确认后，直接在 `main` 提交并 `git push origin main`。
* **大改动或依赖变更**（超过 5 个代码文件，或改动 `Cargo.toml` / `Cargo.lock` / `apps/desktop/package*.json`）：切临时分支提交推送，运行 `gh pr create` 开 PR，待 GitHub Actions 的 `Supply-chain policy` 与 `Windows` 全绿后执行 `gh pr merge --squash --delete-branch` 并切回同步本地 `main`。
* **交付摘要**：汇报检查结果、3～5 行审查摘要（模型档位、轮次、修复类目及全部驳回理由）与入库提交/PR 链接。
* **桌面运行包**：`~/Desktop/Singularity` 仅在整批用户可见功能最终收口或用户明确要求时按 `docs/development.md §5` 更新一次。
