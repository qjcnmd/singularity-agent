# 任务、Git 与验证流程

复杂任务或涉及测试、Cargo、worktree、提交、远程操作和 Issue 时读取。

- 中大型任务先明确真实问题、目标、范围、外部合同、完成条件和未知；按调查、裁决、实施、审查、验证、交付推进。
- 运行 Cargo 前执行 cargo metadata --no-deps --format-version 1 --locked 核对 target_directory；本机目标应为 D:/CargoTargets/singularity-agent。具体命令以 README.md 和 docs/INSTALL.md 为准。
- 按最终 diff 的风险选择最小充分检查；不要机械运行全仓测试或昂贵 Evaluation。长任务采用非阻塞观察，结束前检查仅由本任务启动的残留进程。
- 未经明确授权不得 push、发布、创建 PR、merge、rebase、reset、clean 或改写远程历史。Issue 操作遵循 docs/agents/issue-tracker.md。
