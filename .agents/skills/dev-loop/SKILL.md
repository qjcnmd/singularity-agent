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
  1. 用文件写入工具将精简审查简报直接写入带唯一后缀的临时 UTF-8（无 BOM）文件 `$promptFile`（**不要嵌套进 shell here-string**，且只写仓库外信息，不贴大段源码或复述 `AGENTS.md`）：
     * 本次目标、已确认边界与不做范围；
     * 审查锚点（如 `git diff origin/main` / 指定提交范围）与已通过的检查结果；
     * 要求：先核对 `docs/decisions.md` 与 `docs/architecture.md#运行边界`；阻断项须给出本机 Windows 单用户边界内可复现失效路径或明显结构重复，次要项不触发返工；返回简洁中文结论与按严重度排序的问题列表（位置、失效场景/维护代价、最小改法）。
     * 第 2 轮复审简报保留精简目标与边界，并补充第 1 轮已修复项与驳回项及理由，聚焦增量复核。
  2. 无痕进程直连执行（禁止用 PowerShell `*> $logFile` 管道重定向，避免子进程继承句柄导致永久等不到 EOF；`codex exec` 将全部进度与报错写入 `stderr`，即下面的 `$logFile`；附图验证可在 `'--'` 前重复加 `'-i', '<图片路径>'`）：
     ```powershell
     $ErrorActionPreference = 'Stop'
     $runId = [guid]::NewGuid().ToString('N')
     $outFile = Join-Path $env:TEMP "codex-review-$runId.md"
     $stdoutFile = Join-Path $env:TEMP "codex-review-$runId.out"
     $logFile = Join-Path $env:TEMP "codex-review-$runId.log"
     $codexBin = (Get-Command codex.cmd, codex.exe -ErrorAction SilentlyContinue | Select-Object -First 1).Source
     if (-not $codexBin) { throw 'codex executable not found in PATH' }
     $before = (git status --porcelain --untracked-files=all) -join "`n"
     $proc = $null
     try {
         $proc = Start-Process -FilePath $codexBin -ArgumentList @(
             'exec', '-m', 'codex/gpt-6.2-sol',
             '-c', 'model_reasoning_effort=high',
             '-c', 'notify=[]',
             '-c', 'features.memories=false',
             '--ephemeral', '--skip-git-repo-check',
             '-o', $outFile, '--', '-'
         ) -RedirectStandardInput $promptFile -RedirectStandardOutput $stdoutFile -RedirectStandardError $logFile -NoNewWindow -PassThru
         Write-Output "OUT_FILE=$outFile`nLOG_FILE=$logFile"
         $stableSince = $null; $lastOutSig = ''; $lastLogSig = ''; $lastActivity = Get-Date
         $recovered = $false; $stalled = $false
         while (-not $proc.HasExited) {
             Start-Sleep -Seconds 1
             if (Test-Path -LiteralPath $logFile) {
                 $li = Get-Item -LiteralPath $logFile
                 $logSig = "$($li.Length)|$($li.LastWriteTimeUtc.Ticks)"
                 if ($logSig -ne $lastLogSig) { $lastLogSig = $logSig; $lastActivity = Get-Date }
             }
             if ((Test-Path -LiteralPath $outFile) -and ($fi = Get-Item -LiteralPath $outFile).Length -gt 0) {
                 $outSig = "$($fi.Length)|$($fi.LastWriteTimeUtc.Ticks)"
                 if ($outSig -ne $lastOutSig) { $lastOutSig = $outSig; $stableSince = Get-Date }
                 elseif (((Get-Date) - $stableSince).TotalSeconds -ge 15) {
                     & taskkill /PID $proc.Id /T /F *> $null
                     $recovered = $true; break
                 }
             } elseif (((Get-Date) - $lastActivity).TotalSeconds -ge 480) {
                 & taskkill /PID $proc.Id /T /F *> $null
                 $stalled = $true; break
             }
         }
         $proc.WaitForExit(3000) | Out-Null
         $code = if ($recovered) { 0 } else { $proc.ExitCode }
         $report = if (Test-Path -LiteralPath $outFile) { Get-Content -LiteralPath $outFile -Raw -Encoding UTF8 } else { '' }
         if ($stalled) {
             Get-Content -LiteralPath $logFile -Tail 60 -Encoding UTF8 -ErrorAction SilentlyContinue
             throw 'codex exec stalled (no stderr progress for 480s)'
         }
         if ($code -ne 0 -or [string]::IsNullOrWhiteSpace($report)) {
             Get-Content -LiteralPath $logFile -Tail 60 -Encoding UTF8 -ErrorAction SilentlyContinue
             throw "codex exec failed (exit=$code)"
         }
         $after = (git status --porcelain --untracked-files=all) -join "`n"
         if ($before -ne $after) { Write-Warning "Workspace changed during review.`n$after" }
         $report
     } finally {
         if ($proc -and -not $proc.HasExited) { & taskkill /PID $proc.Id /T /F *> $null; $proc.WaitForExit(3000) | Out-Null }
         Remove-Item -LiteralPath $promptFile, $outFile, $stdoutFile, $logFile -Force -ErrorAction SilentlyContinue
     }
     ```
     后台执行时设置合理超时提醒（如 10 分钟）；若超时仍未结束，按启动时回显的 `LOG_FILE`（`stderr` 进度日志）与 `OUT_FILE` 核查：日志仍在增长则继续等（不误杀），已落盘或长时间停滞再查日志尾部并终止回收。
  3. **独立核实与收敛**：
     * 逐条对照源码核实：属实直接修复，误报/风格偏好忽略并留一句理由，涉及需求取舍交用户拍板。
     * 确认并修复的阻断项，在 `docs/audit-log.md` 的「审查问题记录」表各追加一行（状态标 `待观察`）。
     * 第 1 轮无阻断项 → 直接入库交付；确认并修复了阻断项 → 一次性修完、重跑 `scripts/check.ps1` 后执行第 2 轮增量复审（默认上限 2 轮：第 2 轮无阻断项则直接交付；有未决分歧交用户拍板；若第 2 轮仍确认并修复了阻断项，跑完检查后用提问工具询问用户直接入库还是追加 1 轮复审）。

## 4. 入库、PR 分流与打包

* **中小改动**：审查收敛并确认后，直接在 `main` 提交并 `git push origin main`。
* **大改动或依赖变更**（超过 5 个代码文件，或改动 `Cargo.toml` / `Cargo.lock` / `apps/desktop/package*.json`）：切临时分支提交推送，运行 `gh pr create` 开 PR，待 GitHub Actions 的 `Supply-chain policy` 与 `Windows` 全绿后执行 `gh pr merge --squash --delete-branch` 并切回同步本地 `main`。
* **交付摘要**：汇报检查结果、3～5 行审查摘要（模型档位、轮次、修复类目及全部驳回理由）与入库提交/PR 链接。
* **桌面运行包**：本机试用运行包（路径见 `AGENTS.local.md`，若存在）仅在整批用户可见功能最终收口或用户明确要求时按 `docs/development.md §5` 更新一次。
