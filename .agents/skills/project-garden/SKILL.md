---
name: project-garden
description: >-
  Incremental full-chain repository audit for Singularity: read un-audited commits since the last
  commit recorded in docs/audit-log.md, trace end-to-end call/data chains across 5 dimensions with
  on-demand specialist routing, cross-check via local Codex CLI (codex/gpt-6.2-sol high), and
  update docs/audit-log.md after user approval.
---

# 项目增量全链路体检

默认只读排查，确认清单经用户拍板后再修改代码或更新 `docs/audit-log.md`。

## 执行步骤

1. **读取巡检进度与未巡检提交**：
   * 读取 `docs/audit-log.md` 的「已巡检至提交」（设为 `<last_sha>`）。
   * 若存在 `<last_sha>` 且 `git merge-base --is-ancestor <last_sha> HEAD` 成立，则先列出完整提交窗口 `<last_sha>..HEAD`，再按单提交真实改动跳过仅修改 `docs/audit-log.md` 的纯记录提交（不要直接对 `git log` 加路径过滤以免漏掉合并/回退提交）；尚无记录（或旧记录不在当前历史上）时默认取最近 10 个提交（或用户指定范围）：
     ```powershell
     git log --reverse --oneline <last_sha>..HEAD
     git log --diff-merges=first-parent --name-only --no-renames --pretty=format: <last_sha>..HEAD | Where-Object { $_ -and $_ -ne 'docs/audit-log.md' } | Sort-Object -Unique
     pwsh -File scripts/check.ps1 -Scope size
     ```
   * 排除纯 `docs/audit-log.md` 记录提交后无新增有效提交时，直接汇报并结束。
   * **强制拆批红线**：未巡检提交超过 20 个或涉及文件超过 50 个时，**必须**按连续提交时间窗口拆成多批逐批推进（功能域仅用于批内分工，禁止跳提交推进游标），每批收口后仅将 `docs/audit-log.md` 更新至该连续已巡检范围的末提交。

2. **追踪关联全链路**：
   以本次提交触及的全部文件为起点，向上下游追踪涉及的完整端到端链路（配置/持久化 → `crates/protocol` → `crates/model` / `crates/agent` → `crates/app` → `apps/desktop` → 文档与测试）。

3. **5 维全面体检与按需专项路由**：
   先核对 `docs/decisions.md` 与 `docs/architecture.md#运行边界`（表内已接受取舍与运行边界外场景一律不报），检查关联链路的 5 个维度：
   1. **端到端正确性与边界**：跨模块契约、状态同步、取消/超时/异常恢复、并发与单向锁序、密钥脱敏与安全边界；
   2. **架构一致性与结构重塑**：平行状态机、重复真相源、分层越界，以及能否通过调整数据结构或归属边界让一整片特殊分支/包装层直接消失；
   3. **体量与精简（删除 > 简化 > 重构）**：逐层追问“删掉它，用户可见行为或真实复杂度隔离会变吗”，清理死代码、单实现接口、纯转发包装、内部重复防御及逼近 600 行上限的膨胀点；
   4. **文档与架构图同步**：`architecture.html`、`directory.html`、`docs/architecture.md`、`docs/INSTALL.md`、`docs/decisions.md` 与源码一致；
   5. **不变量测试覆盖**：核心系统底线是否缺黑盒/属性不变量测试，或存在绑死内部实现的脆弱测试。

   **按触及文件按需加载专项清单**（全局技能位于 `~/.agents/skills/`，仅当本次提交触及对应范围时读取）：

   | 触及范围 | 加载技能 | 专项核查重点 |
   | --- | --- | --- |
   | `crates/**/*.rs` | `rust-review`（应用模式） | `unsafe`/UB、外部输入可达 panic、持锁跨 `.await`、`Eq/Hash/Ord` 契约、吞错误 |
   | CSS 动效/过渡或滑块/弹窗交互 | `review-animations` | `transition: all`、`ease-in`、`scale(0)`、非 GPU 属性、`prefers-reduced-motion` |
   | 密钥、网络、命令执行、Markdown 渲染或 Electron preload/IPC | `security-best-practices`（仅前端/TS） | XSS/注入、IPC 越权、敏感信息泄露 |

   * **路由约束**：只取上述技能的检查标准，不执行其独立开场白、输出模板或落盘文件动作；仓库 `docs/decisions.md` 与本机单用户边界始终优先。
   * **上报底线**：每条问题必须给出具体触发案例或维护代价（输入/场景 → 错误结果或明确冗余），无具体案例与纯风格偏好一律不报。

4. **Codex 交叉复核（`codex/gpt-6.2-sol` `high`）**：
   将提交范围、关联链路、初查可疑点（含文件与行号）写入临时 UTF-8 简报 `$promptFile`，运行 `pwsh -File scripts/codex-review.ps1 -PromptFile $promptFile -DeletePrompt` 执行一次默认禁用 Codex 内置联网搜索的无痕交叉复核。

5. **白话汇报、风险标注与交接 `dev-loop`**：
   * **无确认问题**：汇报覆盖的提交范围、链路结论与 `未查范围`，经用户确认后将 `docs/audit-log.md` 的「已巡检至提交」更新为本次连续已巡检范围的末提交并追加一行记录（纯日志提交附 `[skip ci]`）。
   * **有确认问题**：按严重度分组并**全局连续编号**（方便用户回复“修 1 和 3”），每条用 4 句白话写清：**这是哪块（含文件与行号）/ 问题是什么 / 最小改法 / 不修会怎样**，末尾附 `未查范围`，并给命中下列特征的条目显式加标签：
     * `[含界面改动]`：用户可见界面/交互或文案变化；
     * `[需增常驻测试]`：逐条说明拟覆盖的系统不变量与维护代价（经用户同意再加）；
     * `[高复杂度/待取舍]`：单问题预计净增 > 100 行生产代码或需新建状态机时，**同步给出「完整支持」与「显式拒绝该边界并记入 `docs/decisions.md`」两个选项**。
   * 若拟修问题跨多域或同时包含常规项与高复杂度/界面项，向用户建议拆批推进，避免单次巨型 PR 引发连环返工。
   * **交接 `dev-loop` 落地**：用户拍板修复项后，后续实现、UI `Ctrl + R` 预览与验收（命中 `[含界面改动]` 时不得跳过）、局部/全量 `scripts/check.ps1`、Codex 复审收敛与入库统一按 `.agents/skills/dev-loop/SKILL.md` 执行；确认修复的重要跨层根因按类目追加到 `docs/audit-log.md` 问题表。合入后仅将 `docs/audit-log.md` 的「已巡检至提交」更新至**已完整核验的连续提交窗口末提交**（全量批次无后续待查提交时推进至本次修复合入提交；纯日志直推提交附 `[skip ci]`）；中途取消或仍有已确认项未落盘时，不更新 `docs/audit-log.md`。
