# TUI 人工验证清单

自动化测试覆盖状态、布局、输入、会话合同和真实 PTY 字节流。终端字体、颜色、
鼠标命中与完整桌面交互由真实终端人工确认，每个平台分别记录。

## 前置

- 运行 `cargo build --release -p singularity_cli`。
- 配置至少一个可用 provider。
- 在 Windows Terminal、conhost、macOS Terminal 或 xterm 等真实交互式终端中运行 `sg`。

## 清单

| # | 场景 | 操作 | 预期 |
| --- | --- | --- | --- |
| 1 | 启动 | 运行 `sg` | 进入主会话流，底部显示多行输入框与 footer；footer 显示 thread id、模型与 idle |
| 2 | 启动恢复 | 运行 `sg --session <thread-id>` | 直接恢复指定会话并进入 TUI |
| 3 | 单轮执行 | 输入一句话后回车 | 会话流显示 turn 头、assistant 内容与 completed 终态；footer 回到 idle |
| 4 | 运行中介入 | 生成期间输入文本后回车 | 输入在工具完成后、下一段生成前送达当前 turn |
| 5 | follow-up | 生成期间按 Alt+Enter，再按 Alt+Up | 消息先进入后续队列，再撤回最新的尚未执行消息 |
| 6 | 停止生成 | turn 运行中按 Esc | 当前 turn 收敛为 interrupted，终端继续可用 |
| 7 | 退出 | 输入框有文本时连续两次 Ctrl+C | 第一次清空输入并显示退出确认，第二次正常退出；空输入时 Ctrl+D 也正常退出 |
| 8 | 思考块 | 完成包含公开 reasoning 的 turn，按 Ctrl+T | 思考内容在展开与单行折叠标题之间切换 |
| 9 | 工具块 | 让模型执行一次工具，连续按 Ctrl+O | 工具结果按折叠、截断、完整三档循环；运行中有动画色，成功为常规色，失败为红色，完成短暂闪烁 |
| 10 | 命令菜单 | 在空闲态输入 `/` | 显示 `/model /settings /resume /new /session /compact /name` 及用途提示 |
| 11 | 设置 | 执行 `/model` 或 `/settings`，用 Tab 切换字段并回车应用 | 模型或 provider/model/reasoning 写入当前 Thread；错误 selector 在菜单内显示错误 |
| 12 | 会话管理 | 依次验证 `/session`、`/name <name>`、`/new` 与 `/resume` | 显示完整 id/turn 数/token；名称持久化；新建会话；选择器恢复指定会话 |
| 13 | 手动压缩 | 有会话历史时执行 `/compact` | 不创建 turn，会话上下文通过 runtime 压缩并显示结果 |
| 14 | 滚动跟随 | 上滚后继续执行，再按 End 或下滚到底 | 上滚时保持历史位置并显示新内容计数；回底后恢复跟随 |
| 15 | 鼠标 | 滚动滚轮，再点击输入框内的不同字符 | 滚轮浏览会话流；文本光标移到指定的折行与宽字符位置 |
| 16 | 状态行 | 观察一次带工具的 turn | 显示等待模型、思考中、执行具名工具与终态收口，同时显示本轮计时、token 与队列数 |
| 17 | 终端恢复 | 从任一退出路径离开 TUI | 无 raw mode 或 alternate screen 残留，光标可见，再次运行 `sg` 正常 |

## 自动化命令

- `cargo test -p singularity_cli --bin sg`
- `cargo test -p singularity_cli --test tui_pty -- --nocapture`
- `cargo test -p singularity_cli --test entry_contract`

## 平台记录

| 平台 | 终端 | 结果 | 备注 |
| --- | --- | --- | --- |
| Windows | Windows Terminal | 待人工验证 | 按上表记录字体、颜色、鼠标命中与终端恢复 |
| macOS/Linux | 支持 alternate screen 和鼠标事件的终端 | 待人工验证 | 按上表逐项记录 |
