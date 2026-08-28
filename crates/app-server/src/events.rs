//! 生命周期事件投影。
//!
//! stdio 单连接传输下事件随请求响应全量发送；协议中不存在 `event/subscribe`
//! 或订阅状态，客户端把 matching response 之前的 notification 关联到本次请求。
//! 执行期事件（`TurnEvent` 两面之一）的 JSON-RPC 投影由
//! [`singularity_protocol::turn_event_notification`] 单点完成；本模块只拥有
//! 桌面端局部的生命周期通知；thread/read 的公开历史投影
//! （`project_turn_history`）已收进 [`singularity_runtime`] 的 store 层。

use serde_json::json;
use singularity_protocol::Thread;

use super::*;

impl AppServer {
    /// `thread/started`：桌面端局部生命周期通知，`thread/start` 成功后由本
    /// crate 发出。它不描述任何 turn 执行事实，因此不是 `TurnEvent` 变体；
    /// 其 wire 形状只定义在此处。
    pub(super) fn thread_started_notification(&self, thread: &Thread) -> AppServerResult<Value> {
        let message = JsonRpcMessage::notification("thread/started", json!({"thread": thread}))?;
        Ok(message.to_wire_value())
    }
}
