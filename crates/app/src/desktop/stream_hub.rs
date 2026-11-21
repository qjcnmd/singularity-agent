//! 后端事件发布中心：维护全局单调 `revision`、当前聚焦会话过滤与背压高水位保护。
//!
//! 从 `AppServer` 独立出来后，`Conversation` 直接持有 `Arc<StreamHub>` 的发布闭包，
//! 无需反向持有 `Weak<AppServer>` 自引用环。

use std::sync::{Arc, Mutex};

use protocol::{StreamEnvelope, StreamEvent};
use tokio::sync::mpsc;

/// 事件流通道为关键控制与终态事件保留的最小剩余槽位；低于该水位时丢弃高频进度帧避免阻塞或 OOM。
const STREAM_RESERVED_CONTROL_SLOTS: usize = 512;

struct StreamOrder {
    revision: u64,
    resync_queued: bool,
}

struct FocusState {
    epoch: u64,
    session_id: Option<String>,
}

pub(crate) struct StreamHub {
    order: Mutex<StreamOrder>,
    /// 前端当前正在查看的会话 ID 及请求代次；非聚焦会话的高频流式进度不再跨管道推送。
    focused_session: Mutex<FocusState>,
    stream: mpsc::Sender<StreamEnvelope>,
}

impl StreamHub {
    pub(crate) fn new(stream: mpsc::Sender<StreamEnvelope>) -> Arc<Self> {
        Arc::new(Self {
            order: Mutex::new(StreamOrder { revision: 0, resync_queued: false }),
            focused_session: Mutex::new(FocusState { epoch: 0, session_id: None }),
            stream,
        })
    }

    pub(crate) fn set_focused_session(&self, epoch: u64, session_id: Option<String>) {
        let mut focus = self.focused_session.lock().expect("focused session lock poisoned");
        if epoch >= focus.epoch {
            focus.epoch = epoch;
            focus.session_id = session_id;
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.order.lock().expect("stream revision lock poisoned").revision
    }

    /// 单独推进并返回新序号（用于仅返回 RPC 响应而不广播事件的模型配置读/改）。
    pub(crate) fn next_revision(&self) -> u64 {
        let mut order = self.order.lock().expect("stream revision lock poisoned");
        order.revision += 1;
        order.revision
    }

    /// 发布一个事件：过滤非聚焦会话的高频进度帧与背压时的高频帧，
    /// 仅对实际发出的事件递增全局序号并推送给前端；通道饱和时预留最后 1 个槽位发送合并后的 `Ready` 重同步信号。
    pub(crate) fn emit(&self, event: StreamEvent) -> u64 {
        let mut order = self.order.lock().expect("stream revision lock poisoned");
        if self.stream.is_closed() {
            return order.revision;
        }
        if let Some(target_session) = high_frequency_progress_session(&event) {
            let focused = self.focused_session.lock().expect("focused session lock poisoned");
            if focused.session_id.as_deref() != Some(target_session) {
                return order.revision;
            }
            drop(focused);
            if self.stream.capacity() < STREAM_RESERVED_CONTROL_SLOTS {
                let revision = order.revision;
                let capacity = self.stream.capacity();
                drop(order);
                tracing::warn!(
                    session_id = target_session,
                    capacity,
                    "dropping high-frequency progress frame under backpressure"
                );
                return revision;
            }
        }
        if self.stream.capacity() > 1 {
            match self.stream.try_reserve() {
                Ok(permit) => {
                    order.revision += 1;
                    order.resync_queued = false;
                    let revision = order.revision;
                    permit.send(StreamEnvelope { revision, event });
                    return revision;
                }
                Err(mpsc::error::TrySendError::Closed(())) => return order.revision,
                Err(mpsc::error::TrySendError::Full(())) => {}
            }
        }
        if !order.resync_queued {
            match self.stream.try_reserve() {
                Ok(permit) => {
                    order.revision += 1;
                    order.resync_queued = true;
                    let revision = order.revision;
                    permit.send(StreamEnvelope { revision, event: StreamEvent::Ready });
                    drop(order);
                    tracing::warn!(revision, "stream channel saturated; queued Ready resync sentinel");
                    return revision;
                }
                Err(mpsc::error::TrySendError::Closed(())) => return order.revision,
                Err(mpsc::error::TrySendError::Full(())) => {}
            }
        }
        let revision = order.revision;
        drop(order);
        tracing::warn!(revision, "stream channel saturated; dropping stream event");
        revision
    }
}

/// 识别可按焦点过滤、也可在背压高水位线时让路的高频进度帧，返回其所属会话 ID。
fn high_frequency_progress_session(event: &StreamEvent) -> Option<&str> {
    match event {
        StreamEvent::TurnEvent { session_id, payload }
            if matches!(
                payload.event,
                protocol::TurnEvent::TextDelta { .. } | protocol::TurnEvent::ToolOutput { .. }
            ) =>
        {
            Some(session_id.as_str())
        }
        StreamEvent::BackgroundTaskUpdated { session_id, payload } if payload.task.state.is_running() => {
            Some(session_id.as_str())
        }
        _ => None,
    }
}
