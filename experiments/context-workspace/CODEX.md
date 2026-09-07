# Codex 交接入口

继续在 `experiment/context-workspace-designs` 或从它创建的实验分支工作，禁止把实现、评测辅助修改推到main；不要覆盖本机未提交修改。必要时使用独立git worktree。

先读本目录README、DESIGN、INTEGRATION、EVALUATION，运行Python机制测试并审查原型。它是可执行语义参考，不是已经接入的Rust实现，不能直接宣布策略能在WebUI使用。

目标只保留两个综合候选：StateWorkspace（首选）和EvidenceWorkspace（备选）。它们共用原始轨迹、来源、确定性执行事实和预算/缓存控制器；差异是是否依赖持续维护的语义工作状态。不要重新扩散成按每篇论文各做一种方案。现有Summary保留并提供公平对照。

根据最新本地源码把目标接入同一个AgentLoop、SessionWriter和request assembly，补齐创建前选择且会话固定、旧Session兼容、history工具、完整payload、手动整理、overflow和crash recovery。不要从原型复制一个生产Python sidecar或第二套日志。原型以UTF-8字节计量，生产要用最终请求和真实usage。

优先检查：模型管理调用是否增加实际往返；原文被移出前是否已被正确承接；合法JSON是否可能含错误状态；用户约束是否能被错误删除；private replay/tool pairing是否合法；缓存是否真命中。不要强迫模型每步额外调用另一个记忆LLM。

代码中确定性事实、引用与协议合同通过后，再按EVALUATION设计真实模型实验。先展示任务、模型和费用预算并取得同意；所有管理/修复/回读成本计入。固定轨迹和脚本状态不等于任务能力。最终报告说明两候选在何种任务下获益或失败、可复现命令、完整成本和明确默认推荐；有证据时可以否定首选设计。
