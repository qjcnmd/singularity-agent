//! 生命周期事件投影。
//!
//! stdio 单连接传输下事件随请求响应全量发送；协议中不存在 `event/subscribe`
//! 或订阅状态，客户端把 matching response 之前的 notification 关联到本次请求。
//! 执行期事件的逐条映射在 [`crate::lifecycle`] 投影适配器中完成，本模块只
//! 保留通知包装；thread/read 的公开历史投影（`project_turn_history`）已收进
//! [`singularity_runtime`] 的 store 层。

use super::*;

impl AppServer {
    /// 将应用事件包装为 JSON-RPC notification。
    pub(super) fn event_notification(&self, event: AppEvent) -> AppServerResult<Value> {
        Ok(event.to_notification().to_wire_value())
    }
}
