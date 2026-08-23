# TUI 最小人工验证清单

自动化测试覆盖投影纯函数与会话合同（`cargo test -p singularity_cli --bin sg`、
`crates/cli/tests/entry_contract.rs`）与状态/布局/输入行为（scoll/editor/transcript/
app 各模块单测）；终端交互语义必须在真实 TTY 中按本清单逐项观察，并在此登记
实际结果。每个平台单独记录。

## 前置

- 构建并安装：`cargo build --release -p singularity_cli`（二进制 `sg`）。
- 配置至少一个可用 provider（用户配置或进程环境层）。
- 在交互式终端（Windows Terminal / conhost / macOS Terminal / xterm 等）中运行。

## 清单

| # | 场景 | 操作 | 预期 |
| --- | --- | --- | --- |
| 1 | 启动 | 无参数运行 `sg` | 进入 alternate screen 的主会话流 + 底部输入框 + footer；footer 显示 thread id、模型、idle 与当前 steer/followUp 模式 |
| 2 | 单轮执行 | 输入一句话回车 | 会话流出现 `── turn …` 头、assistant 段落合并为一段、`✔ completed` 行；footer 回到 idle |
| 3 | 工具事件 | 让模型执行一次 read/bash | 显示 `▸ <tool> {args}` 与结果预览；错误工具带 ✖ 标记 |
| 4 | 工具展开 | 有完成工具的轮次按 Alt+O | 最近完成工具结果在 3 行预览与全量（上限 100 行）间切换；再次 Alt+O 收起 |
| 5 | steer | 执行期间按 Ctrl+T 切至 steer 并回车输入 | footer 模式可见；当前轮内注入生效 |
| 6 | followUp | 执行期间切至 followUp 并回车输入 | 本轮结束后自动启动下一轮执行该输入；两条输入各恰好执行一次 |
| 7 | 设置菜单 | Ctrl+S 打开，Tab 切字段，Enter 应用 | 弹出临时菜单；空闲时立即生效；turn 运行中下一轮生效；Esc 关闭后滚动与编辑内容不变 |
| 8 | 设置校验 | 菜单中填入不可解析的模型并应用 | 菜单内红色错误文本，不崩溃 |
| 9 | Ctrl+C 一级 | turn 运行中按一次 Ctrl+C | footer 显示 interrupting…；turn 以 interrupted 收尾回到 idle；已接受的 followUp 仍执行 |
| 10 | Ctrl+C 二级 | 运行中连续两次 Ctrl+C | 立即退出（130），终端状态完整恢复 |
| 11 | 空闲退出 | 空闲时连续两次 Ctrl+C | 正常退出（0） |
| 12 | Esc 阶梯 | 浏览历史时按 Esc | 先回底跟随；再按 Esc 清空非空草稿；空输入时无操作（无死端） |
| 13 | 浏览指示 | 上滚后继续执行 | 不强制跳回底部；footer 显示 viewing history 与 ↓N new；Ctrl+End 或触底回跟随 |
| 14 | 等待指示 | 执行期间观察状态行 | waiting 后显示具名对象（model / tool name / terminal convergence）与等待秒数 |
| 15 | 滚轮 | 会话流上拨/下拨滚轮 | 上拨脱离跟随浏览历史；下拨触底恢复跟随；无崩溃/残影 |
| 16 | 终端恢复 | 以上任一退出路径后 | 无残留 raw mode / alternate screen；光标可见；再次运行 `sg` 正常 |

## 自动化覆盖矩阵（无需人工）

- 状态/布局/输入：`cargo test -p singularity_cli --bin sg`（follow/detach 双态、
  resize 锚定、unicode 折行、编辑器、工具展开、Esc 阶梯、footer 内容、设置模态、
  followUp 恰一次入队）。
- 无交互合同：`crates/cli/tests/entry_contract.rs`（交互模式无终端时明确诊断退出）。
- 真实 PTY e2e：`crates/cli/tests/tui_pty.rs`（启动渲染、键入回显、Esc 清空、滚轮、
  两级 Ctrl+C 退出码）。该测试在宿主具备可工作的伪控制台渲染转发层时执行：
  `cargo test -p singularity_cli --test tui_pty -- --ignored`。Windows 上要求完整
  交互式桌面会话中的 ConPTY；在伪控制台转发层不可用的受限环境（如沙箱宿主）中
  该层无输出，测试保持跳过并应在此登记。

## 平台记录

### Windows（部分自动化观察，待人工复核）

- 场景 1 之外的全部条目在开发过程中以状态/布局单测与端口级合同测试核对；
- 非 TTY 下裸 `sg` 以明确诊断退出（`interactive mode requires a
  terminal…`），由 `entry_contract::interactive_mode_requires_a_terminal` 固化；
- 真实 PTY e2e（清单 1/12/13/15/16 对应的启动、Esc、浏览保持、滚轮、干净退出）
  以 `tui_pty.rs` 编写完毕；在当前受限执行环境下伪控制台渲染转发层不工作
  （子进程文本无法返回 master 流），测试标记 `#[ignore]` 并留待完整交互桌面
  运行；人工复核时请在第 13/14/15 项登记观察结果。

### Unix（待观察）

- 未在本任务环境中执行；需在 Linux/macOS 终端按清单复核后登记。