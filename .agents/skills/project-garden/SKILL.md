---
name: project-garden
description: >-
  Incremental full-chain repository audit for Singularity: read un-audited commits since the last
  commit recorded in docs/audit-log.md, trace end-to-end call/data chains across 5 dimensions,
  cross-check via local Codex CLI (codex/gpt-6.2-sol high), and update docs/audit-log.md after
  user approval.
---

# 项目增量全链路体检

默认只读排查，确认清单经用户拍板后再修改代码或更新 [docs/audit-log.md](file:///C:/Users/Lenovo/Desktop/Harness/docs/audit-log.md)。

## 执行步骤

1. **读取巡检进度与未巡检提交**：
   * 读取 `docs/audit-log.md` 的「已巡检至提交」（设为 `<last_sha>`）。
   * 若存在 `<last_sha>` 且 `git merge-base --is-ancestor <last_sha> HEAD` 成立，则取 `<last_sha>..HEAD`；尚无记录（或旧记录不在当前历史上）时默认取最近 10 个提交（或用户指定范围）：
     ```powershell
     git log --reverse --oneline <last_sha>..HEAD
     git log --name-only --no-renames --pretty=format: <last_sha>..HEAD
     pwsh -File scripts/check.ps1 -Scope size
     ```
   * 无新增提交时直接汇报并结束。
   * 未巡检提交超过 20 个或涉及文件超过 50 个时，按时间或功能域分批推进，每批收口后仅将 `docs/audit-log.md` 更新至该批末提交。

2. **追踪关联全链路**：
   以本次提交触及的全部文件为起点，向上下游追踪涉及的完整端到端链路（配置/持久化 → `crates/protocol` → `crates/model` / `crates/agent` → `crates/app` → `apps/desktop` → 文档与测试）。

3. **5 维全面体检**：
   核对 `docs/decisions.md` 与 `docs/architecture.md#运行边界`（表内已接受取舍与运行边界外场景不报），检查关联链路的 5 个维度：
   1. **端到端正确性与边界**：跨模块契约、状态同步、取消/超时/异常恢复、并发与单向锁序、密钥脱敏与安全边界；
   2. **架构一致性与单一来源**：平行状态机、重复真相源、分层越界、与周边惯例冲突；
   3. **体量、死代码与冗余抽象**：热链路死代码、未用导出、冗余包装、逼近 600 行上限的膨胀点；
   4. **文档与架构图同步**：`architecture.html`、`directory.html`、`docs/architecture.md`、`docs/INSTALL.md`、`docs/decisions.md` 与源码一致；
   5. **不变量测试覆盖**：核心系统底线是否缺黑盒/属性不变量测试，或存在绑死内部实现的脆弱测试。

4. **Codex 交叉复核（`codex/gpt-6.2-sol` `high`）**：
   将提交范围、关联链路与初查可疑点（含文件与行号）写入临时 UTF-8 简报，按 `.agents/skills/dev-loop/SKILL.md` 第 3 节的标准 `codex exec` 命令调用一次无痕交叉复核，核对工作区未被改写并清理临时文件。

5. **汇报与更新巡检进度**：
   * **无确认问题**：汇报覆盖的提交范围与链路结论，经用户确认后将 `docs/audit-log.md` 的「已巡检至提交」更新为本次末提交并追加一行记录。
   * **有确认问题**：按严重度输出简洁中文清单（维度、位置、风险/代价、最小改法），用提问工具请用户决定修复项；完成修复、通过 `pwsh -File scripts/check.ps1` 并提交后，再更新 `docs/audit-log.md` 至最新提交。
   * 中途取消或仍有已确认项未落盘时，不更新 `docs/audit-log.md`。
