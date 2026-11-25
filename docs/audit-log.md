# 项目巡检与审查问题记录

## 全链路巡检进度

- **已巡检至提交**：`暂无`

| 日期 | 覆盖提交范围 | 涉及主要链路 | 结论与处理 |
| --- | --- | --- | --- |

## 审查问题记录（供体检提炼硬规则）

每次 Codex 审查确认并修复阻断项后追加一行；同类问题跨任务出现 ≥ 2 次时，在全链路体检中提议固化为 `check.ps1` 检查或规则并标为 `已固化`。

| 日期 | 提交/任务 | 问题类目 | 一句话摘要 | 状态 |
| --- | --- | --- | --- | --- |
| 2026-11-25 | fix/project-garden-audit-remediation | 快照回滚 | 删除冲突目录时未先登记并恢复空子目录，局部删除失败或回滚后空目录永久丢失 | 待观察 |
| 2026-11-25 | fix/project-garden-audit-remediation | Windows 命令行上限 | 动态排除/长路径分批传参至 `git ls-files` 易超 Windows 32KB 参数上限（error 206） | 待观察 |
| 2026-11-25 | fix/project-garden-audit-remediation | Windows 大小写改名 | 仅大小写改名目录含忽略文件时未在写盘前拦截冲突，导致还原返回成功但留脏差异 | 待观察 |
| 2026-11-25 | fix/project-garden-audit-remediation | Git 索引冲突 | 符号链接回滚 `update-index --cacheinfo` 缺 `--replace`，遇目录/文件索引冲突必败 | 待观察 |
| 2026-11-25 | fix/project-garden-audit-remediation | Windows 符号链接 | 回滚未区分目录/文件符号链接且未延后创建，导致目录符号链接静默退化为文件符号链接 | 待观察 |
