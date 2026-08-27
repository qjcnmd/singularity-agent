//! turn 执行线程向事件通道投递进度的共享 sink 与轮询间隔：无交互主循环
//! 与 TUI 事件循环共用同一投递实现。

use std::sync::mpsc;
use std::time::Duration;

use singularity_runtime::events::{TurnEvent, TurnEventSink};

/// 入口主循环无阻塞等待 turn 事件的轮询间隔。
pub(crate) const INTERRUPT_POLL: Duration = Duration::from_millis(100);

/// 把 turn 事件按入口自己的消息形状映射后投递进通道的 sink；执行线程
/// 结束时 drop 它，接收方据此从通道断开感知执行已收敛。
pub(crate) struct EventForward<M> {
    tx: mpsc::Sender<M>,
    project: fn(TurnEvent) -> M,
}

impl<M> EventForward<M> {
    pub(crate) fn new(tx: mpsc::Sender<M>, project: fn(TurnEvent) -> M) -> Self {
        Self { tx, project }
    }
}

impl<M> TurnEventSink for EventForward<M> {
    fn emit(&mut self, event: TurnEvent) {
        let _ = self.tx.send((self.project)(event));
    }
}
