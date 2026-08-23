# TUI 最小人工验证清单

自动化测试覆盖投影纯函数与会话合同（`cargo test -p singularity_cli --bin sg`
与 `crates/cli/tests/entry_contract.rs`）；终端交互语义必须在真实 TTY 中按本
清单逐项观察。每个平台单独记录实际结果。

## 前置

- 构建并安装：`cargo build --release -p singularity_cli`（二进制 `sg`）。
- 配置至少一个可用 provider（用户配置或进程环境层）。

## 清单

| # | 场景 | 操作 | 预期 |
| --- | --- | --- | --- |
| 1 | 启动 | 无参数运行 `sg` | 进入 alternate screen 的主会话流 + 底部输入框 + footer；footer 显示 thread id、模型、idle 与当前 steer/followUp 模式 |
| 2 | 单轮执行 | 输入一句话回车 | 会话流出现 `── turn …` 头、assistant 段落合并为一段、`✔ completed` 行；footer 回到 idle |
| 3 | 工具事件 | 让模型执行一次 read/bash | 显示 `▸ <tool> {args}` 与结果预览；错误工具带 ✖ 标记 |
| 4 | steer | 执行期间切换 Ctrl+T 至 steer 并回车输入 | footer 模式可见；会话流出现 `↳ steer:`；当前轮内注入生效 |
| 5 | followUp | 执行期间切至 followUp 并回车输入 | 出现 `↳ followUp:`；本轮结束后自动启动下一轮执行该输入 |
| 6 | 设置菜单 | Ctrl+S 打开，Tab 切字段，Enter 应用 | 弹出临时菜单；空闲时立即生效提示；turn 运行中显示 queued 提示且下一轮生效 |
| 7 | 设置校验 | 菜单中填入不可解析的模型并应用 | 菜单内红色错误文本，不崩溃 |
| 8 | Ctrl+C 一级 | turn 运行中按一次 Ctrl+C | footer 显示 interrupting…；turn 以 interrupted 收尾回到 idle |
| 9 | Ctrl+C 二级 | 运行中连续两次 Ctrl+C | 立即退出（130），终端状态完整恢复 |
| 10 | 空闲退出 | 空闲时连续两次 Ctrl+C | 正常退出（0） |
| 11 | 终端恢复 | 以上任一退出路径后 | 无残留 raw mode / alternate screen；光标可见 |
| 12 | resize | 执行与空闲时拖动窗口大小 | 重绘无残影 |

## 平台记录

### Windows（已观察）

- 场景 12 之外的全部条目在开发过程中以非 TTY 自动化与代码走查核对；
- 非 TTY 下裸 `sg` 以明确诊断退出（`interactive mode requires a
  terminal…`），不落入 clap 用法输出 —— 由 `entry_contract::intera
ctive_mode_requires_a_terminal` 固化。
- 待真实 Windows 终端（Windows Terminal / conhost）逐项人工复核后在此登记。

### Unix（待观察）

- 未在本任务环境中执行；需在 Linux/macOS 终端按清单复核后登记。
