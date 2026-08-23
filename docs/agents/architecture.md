# 架构与扩展边界

本文件只在架构、核心代码或客户端链路任务中读取；当前事实以 docs/singularity.md 与源码为准。

## 模块与依赖方向

```text
crates/cli（入口解析 + TUI/Text/JSONL 渲染）
  └─ crates/runtime（Turn 执行唯一所有者）
       ├─ TurnRunner：单轮管线（会话单写者、项目指令、Agent、typed 事件、终态落盘、fail-stop）
       ├─ Conversation：Thread 长驻协调器（reserve 链窗口、steer 当前轮、followUp FIFO、取消按轮、设置自动应用）
       └─ crates/agent / crates/core / crates/model（AgentLoop、会话 JSONL、工具、Provider）
crates/app-server（stdio JSON-RPC 适配器）
  ├─ crates/runtime（执行全部委托）
  └─ crates/protocol（wire 类型只在适配器这一侧）
```

- cli → runtime → {core, model, agent}；app-server → {runtime, protocol}。
- runtime 与 agent 不依赖 protocol/UI；protocol 类型只存在于 crates/protocol 与 app-server 适配器，runtime 不引用。
- 客户端形态（TUI / headless / app-server worker）一律委托 runtime 的 Conversation/TurnRunner；客户端不复制执行状态。

## 接缝与替换边界

- Tool、Compaction、Context 组装、Provider 和事件各保留一个职责清晰、可替换的接缝和一个默认实现；没有真实消费者时不增加策略层、通用插件协议、多实现注册框架、兼容包装或额外状态。
- Sandbox/Approval 默认不启用；可选模式通过独立接缝提供，不进入核心依赖路径，也不改变无 Sandbox 时的核心行为。
- 工具生命周期事件遵循当前协议合同；改变协议前先更新架构事实、客户端合同、限长/脱敏策略和兼容验证。
- 调查候选简化时搜索生产调用、动态注册、测试/文档消费者，并检查所有权、持久化、并发、安全和失败路径。代码图只用于导航，关键事实用源码、rg、Git 或运行复核。

## 并发与生命周期不变量

- 同一 Thread 至多一个活动 turn 链：`Conversation::reserve_start` 原子预订链窗口；app-server 在 worker 线程启动前同步裁定 turn/start（先到先得，后到立即 invalid-state 响应），TUI/headless 同走该入口。
- 取消与转向按轮独立：interrupt 只取消当前轮；已接受的 followUp 在可信终态后继续执行；steer 只注入当前轮 inbox。
- 设置时序：活动期间 `queue_settings` 合并为单份意图，轮终态收敛后自动校验并持久化，下一轮生效；无公开手工应用接口。
- 终态化：JSONL 落盘（有界重试一次）在前，事件与索引投影在后；无法落盘时 fail-stop（storage_fatal 诊断），不发布虚假终态。