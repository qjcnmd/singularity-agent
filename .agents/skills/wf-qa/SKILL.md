---
name: wf-qa
description: Singularity 真实模型探索式实机 QA：派独立空白上下文 QA 子代理在隔离 SINGULARITY_HOME 下启动 Electron，通过 playwright-cli 实时读界面像真人一样操作并调用真实模型，支持用户可见改动自动验收与手动 /wf-qa 全应用摸底。
---

# 真实模型探索式实机 QA

## 1. 调度方式：独立 QA 子代理

* 由主代理启动 **1 个独立空白上下文的 `self` 子代理**（`Role: "Exploratory QA Tester"`）执行实机操作，避免携带开发者的先入为主，也不让快照与截图占满主对话上下文；常规探索验收可选用轻量 `flash` 模型提速降本，复杂排错再用默认模型。
* **派发前准备**：主代理确认 `apps/desktop/dist/index.html` 与 `apps/desktop/desktop-runtime/singularity.exe` 已对应当前源码（缺失或过期则先跑 `npm --prefix apps/desktop run build`；改了 Rust 则补 `cargo build -p app --bins && npm --prefix apps/desktop run prepare:runtime -- debug`）。
* **简报内容**：只给子代理传入 ① 执行模式与用户视角目标（**模式 A** 写 2～3 句本次可见改动与关联主流程；**模式 B** 写全应用摸底或用户指定重点，不附源码实现细节）；② 要求按本技能与 `AGENTS.local.md` 默认真实模型执行并返回白话报告。

## 2. 隔离启动与清理（零污染真实数据）

1. **搭建临时环境**：在 `$env:TEMP\singularity-qa-<id>` 下建 `home/` 与 `workspace/`；在 `workspace/` 初始化极简 Git 仓库（放 1～2 个含可测小缺陷的示例文件）；**仅复制** `~/.singularity/config.json` 到 `$qaHome\config.json`；写入 `$qaHome\workspaces.json`（`{"workspaces":[{"root":"<斜杠路径>"}]}`）以直接载入项目、避开系统文件夹弹窗。
2. **后台常驻拉起 Electron 并落盘状态**：清除 `ELECTRON_RUN_AS_NODE`，设 `SINGULARITY_HOME=$qaHome`，在 `run_command` 中设 **`IsDaemon: true`** 启动 `apps/desktop/node_modules/electron/dist/electron.exe "$desktopDir" --remote-debugging-port=<随机空闲端口>`（Windows 非常驻命令退出会自动杀子进程；隔离 `SINGULARITY_HOME` 会派生独立单实例锁，无需关闭用户日常实例）。轮询 `http://127.0.0.1:<port>/json/list` 确认出现 `singularity://app/` 页面后，将 `{ pid, port, qaRoot, qaHome, qaWs, userDataDir }` 写入 `$env:TEMP\singularity-qa-state.json`（`$userDataDir = "$env:APPDATA\Singularity\<sha256(FullPath(qaHome).ToLower()).Substring(0,16)>"`），再 `Wait-Process`。
3. **收尾必做（完成判据）**：清理前先用 `read_file` 看完关键截图；随后用 `manage_task(kill)` 结束 Electron 常驻任务，强制删除 `$qaRoot`、`$userDataDir` 与 `singularity-qa-state.json`，最后检查仓库 `git status -s` 无 `.playwright-cli` 或临时残留。

## 3. 实时界面探索（泛化不写死选择器）

* **操作纪律**：每次执行 `playwright-cli` 前先 `Set-Location $qaRoot`，让 `.playwright-cli\` 快照落在临时目录内。Windows 上非 daemon 的 `run_command` 退出时会顺带结束 `playwright-cli` 守护进程，因此**每批命令开头都先执行** `playwright-cli -s=qa attach --cdp="http://127.0.0.1:$port"`，再按 `--raw snapshot` 实时发现控件与新入口（同名按钮加 `exact: true`，切换类控件先看 `[expanded]`，保留输出不吞错）。PowerShell 下执行多行或含引号的 `run-code` 时，将脚本写入 `$qaRoot\*.js` 用 `run-code --filename=...` 调用，并用 `return` 返回结果（`console.log` 不回显）。Electron 页面的实操要点：`playwright-cli screenshot` 可能挂起，截图统一在 `run-code` 中用 `page.screenshot({ path, timeout })`；`attach` 后快照 `ref` 不稳定，控件优先用 `role` / `text` 定位，无障碍名较长的按钮（如 `Confirm compact context`）用正则匹配；`resize` 视口模拟在下次 `attach` 后失效且 Electron 不支持原生窗口缩放，窄窗检查须在同一个 `run-code` 内 `setViewportSize` 后立即检查并截图。
* **真实模型与等待**：在模型选择器选中 `AGENTS.local.md` 指定的真实模型（默认 `codearts/deepseek-v4.1-flash-1017 high`）；等待模型回复结束一律以停止按钮出现后消失（`getByRole('button', { name: /停止/ })`）为准，不写死固定睡眠。
* **模式 A（`wf-dev` 自动验收）**：围绕本次用户可见改动与受影响的关联老流程，像挑刺的真实用户一样覆盖正常操作、取消/中断、重复点击、异常边界与 `playwright-cli reload` 刷新持久化。
* **模式 B（手动 `/wf-qa` 全应用摸底）**：按真实使用旅程逐项探索——新建任务与模型/推理档位切换 → 真实工具读写改文件与 `Changes` diff → `RunPanel`（Context 展开、`Compact → Confirm` 取消及短对话触发“无需压缩”提示，无需人工灌长上下文）→ 编辑历史消息回退并核对磁盘与对话同步 → `设置` 面板 → 刷新恢复、窄窗（`760x600`）与默认窗口（`1440x960`）布局，以及快照中新出现的所有可见入口（用户指定重点时优先深挖该区）。

## 4. 实证核查与收口

* **三项必查实证**：① `playwright-cli -s=qa console`（核查控制台错误与警告）；② `$qaHome\logs\singularity.log`（核查后端异常日志）；③ 关键状态与窄窗截图通过 `read_file` 亲自看图核查裁切、重叠或错位。
* **子代理回报格式**：返回简明中文清单——已测路径、确认的功能缺陷（触发步骤 + 实际/预期 + 日志或控制台证据）、视觉/布局观感提醒、临时环境已清空确认。
* **主代理处理**：功能缺陷直接修复并复测受影响路径；若暴露系统级不变量缺口，按 [测试保留与清理标准](../../../docs/development.md#测试保留与清理标准) 评估并询问用户是否补入 `apps/desktop/e2e/smoke.mjs`；纯视觉/动效观感项写明手测步骤交用户判断。
