---
name: wf-audit
description: Singularity 定期项目体检与代码瘦身：基于近 10 次提交蔓延区或指定范围，动静结合排查臃肿、跨模块缺陷与文档漂移。
---

# 项目体检与修剪

## 1. 确定范围

* 未指定范围时，默认从最近 10 个提交出发，沿触及文件的上下游调用链、共享状态与对应文档向外蔓延至完整关联区域，并顺带检查源码体量上限：
  ```powershell
  git log -n 10 --oneline
  git log -n 10 --diff-merges=first-parent --name-only --no-renames --pretty=format: | Where-Object { $_ } | Sort-Object -Unique
  pwsh -File scripts/check.ps1 -Scope size
  ```
* 用户明确指定模块、链路或全仓时，按用户指定范围执行。

## 2. 动静结合体检（瘦身与健康并重）

先核对 `docs/decisions.md` 与 `docs/architecture.md#运行边界`（表内已接受取舍与运行边界外场景不报）。除静态读源码外，按需运行 `scripts/check.ps1`、已有测试、临时最小复现或实机/Playwright 抽查，用实测证实可疑问题，避免纸上谈兵。

从两条主轴同步检查：

1. **减法瘦身与结构重塑（删除 > 简化 > 重构）**
   * 沿入口逐层追问“去掉它，用户可见行为或真实复杂度隔离会变吗”，清理死代码、单实现接口、纯转发包装、内部重复防御、补丁堆积与逼近 600 行上限的膨胀点；上游删减后复扫变空壳的下游。
   * 检查结构重塑机会：能否调整数据结构、单一来源或归属边界，让一整片特殊分支、平行状态或包装层直接消失。
2. **端到端健康与地图对齐**
   * 跨模块契约、状态同步、取消/超时/异常恢复、并发与单向锁序、密钥脱敏与安全边界。
   * `architecture.html`、`directory.html`、`docs/architecture.md`、`docs/INSTALL.md`、`docs/decisions.md` 与源码一致。
   * 核心系统不变量是否缺黑盒/属性测试，或存在绑死内部实现的脆弱测试。

**按触及文件按需加载专项清单**（全局技能位于 `~/.agents/skills/`，只取检查标准，不执行其独立开场白、输出模板或落盘动作；`docs/decisions.md` 与单用户边界优先）：

| 触及范围 | 加载技能 | 专项核查重点 |
| --- | --- | --- |
| `crates/**/*.rs` | `rust-review`（应用模式） | `unsafe`/UB、外部输入可达 panic、持锁跨 `.await`、`Eq/Hash/Ord` 契约、吞错误 |
| CSS 动效/过渡或滑块/弹窗交互 | `review-animations` | `transition: all`、`ease-in`、`scale(0)`、非 GPU 属性、`prefers-reduced-motion` |
| 密钥、网络、命令执行、Markdown 渲染或 Electron preload/IPC | `security-best-practices`（仅前端/TS） | XSS/注入、IPC 越权、敏感信息泄露 |

* **上报底线**：每条问题必须给出具体触发案例或明确维护代价，无依据与纯风格偏好不报；**区域本身健康精干时直接汇报“本次检查健康，无需修改”并附 `未查范围` 结束，不硬找问题**。

## 3. 分流处理与分批验证

* **直接清理/修复**：不触碰 `AGENTS.md#决策边界` 的明确小实现失误、死代码、明显冗余或文档/图表漏同步，直接实施并验证，汇报中简明列明（附 `未查范围`）。
* **暂停交用户拍板**：凡命中 `AGENTS.md#决策边界` 的条目，用白话讲清位置、现状、改法、收益与代价后向用户确认。
* **分批落地防巨型 PR**：涉及界面改动按 `.agents/skills/wf-dev/SKILL.md` §1 做实机预览；互不相关的多模块改动按主题分批实施并验证受影响 `scripts/check.ps1`，待每批完成第 4 步收口确认后再分别提交（单批满足 `wf-dev` §4 大改动或依赖变更条件时走 PR 流程）。

## 4. 收口确认（可选外部复审）

* 每批改动通过 `scripts/check.ps1` 后，向用户确认 3 选 1：**直接提交入库**、**补跑 1 次 Codex 复审**（`-Kind wf-audit -Round 1`）或**补跑 1 次空白上下文子代理复审**（改动涉及快照、压缩、锁序、协议或安全边界时推荐补审；改动涉及工作流/脚本时优先推荐空白子代理）。
* 若选择补跑复审，复用 `wf-dev` §3 的 4 段简报、统一 `.reviews/` 归档与问题分流规则，处理完确认项并重跑受影响检查后即提交入库，不再自动追加第 2 轮（除非用户主动要求）。
