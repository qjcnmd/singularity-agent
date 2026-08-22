# Domain Docs

后续工程技能探索此仓库时，按以下优先级读取资料：

1. 根目录的 `AGENTS.md`。
2. `docs/singularity.md`：本仓库唯一的当前架构事实文档。
3. 若存在且与当前任务相关，再读取根目录 `CONTEXT.md`、`CONTEXT-MAP.md` 与 `docs/adr/`。

这是单上下文（single-context）仓库。缺少可选的 `CONTEXT.md` 或 ADR 时静默继续；不要主动创建它们。

`docs/singularity.md` 与现行源码优先于历史 ADR、报告或迁移文档。若输出与仍适用的 ADR 冲突，需明确指出冲突，不得静默覆盖。

命名应沿用当前代码、`CONTEXT.md`（如存在）和 `docs/singularity.md` 中定义的领域词汇。
