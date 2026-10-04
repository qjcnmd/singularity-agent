# 单条排队输入与会话 steer 输入箱

日期：2026-10-03。状态：已采用。

## 决策

每个会话最多一条普通排队消息。主输入框是文字和图片的唯一编辑入口；排队期间内容只读，其他控件遵循各自的操作条件。队列提供编辑、删除、发送三个动作。编辑原子取回完整输入，发送在运行时转为 steer，空闲时开始新一轮。

Conversation 持有 `pending_input: Option<ControlRequest>` 与会话级 `SteeringInbox`。活动执行借用同一个 steer 输入箱，在模型步边界消费输入。当前请求失败会结束执行，尚未消费的 steer 保持原位；继续会话时先消费较早接受的 steer，再纳入新提交。用户明确停止沿用取消当前未消费 steer、保留普通排队消息的语义。

消息与队列动作同时绑定会话身份和消息身份。前端切换会话或播放退场动画时，动作对象保持原有归属。

自动续跑和空闲发送共用回合准备成功后的输入交付点。准备期间持有执行预订，排队输入仍归 Conversation 且不可编辑；模型、指令或写者准备失败时原输入保持原位。这样在运行中修改模型配置后，下一轮准备失败仍可修正配置并继续发送，无需另存输入副本或维护回填流程。

## 依据与边界

DSH 将普通排队和 `next-step` 输入分开，Pi 也分别持有 follow-up 与 steering 队列；两者不会因当前请求失败把尚未消费的 steer 合并成普通排队消息。本项目采用会话持有 steer 的职责划分，同时遵循[宪章](../constitution.md#运行边界)：待处理输入只在进程内存在，重启后由用户手动继续已保存历史。

单条排队限制由后端接受边界维护。排队和取回期间的内容锁由工作台统一派生；队列操作错误复用会话动作反馈。

如果未来需要重启后继续待处理输入，必须重新评估输入接受与持久化的边界；内存箱不能承担该保证。

实现入口：[Conversation](../../crates/runtime/src/conversation.rs)、[SteeringInbox](../../crates/agent/src/agent/inbox.rs)、[Composer](../../apps/desktop/src/components/Composer.tsx)。

参考：[DSH Agent Loop](https://github.com/deepseek-ai/deepseek-harness/tree/master/packages/core/agent-loop)、[Pi Agent](https://github.com/badlogic/pi-mono/blob/main/packages/agent/src/agent.ts)。DSH 行为依据本机安装的 `0.1.7-alpha.2` 源码核对。
