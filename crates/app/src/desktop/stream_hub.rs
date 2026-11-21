//! 后端事件发布中心：维护全局单调 `revision`、当前聚焦会话过滤与背压高水位保护。
//!
//! 从 `AppServer` 独立出来后，`Conversation` 直接持有 `Arc<StreamHub>` 的发布闭包，
//! 无需反向持有 `Weak<AppServer>` 自引用环。

use std::sync::{Arc, Mutex};

use singularity_protocol::{StreamEnvelope, StreamEvent};
use tokio::sync::mpsc;

/// 事件流通道为关键控制与终态事件保留的最小剩余槽位；低于该水位时丢弃高频进度帧避免阻塞或 OOM。
const STREAM_RESERVED_CONTROL_SLOTS: usize = 512;

pub(crate) struct StreamHub {
    revision: Mutex<u64>,
    /// 前端当前正在查看的会话 ID；非聚焦会话的高频流式进度不再跨管道推送。
    focused_session: Mutex<Option<String>>,
    stream: mpsc::Sender<StreamEnvelope>,
}

impl StreamHub {
    pub(crate) fn new(stream: mpsc::Sender<StreamEnvelope>) -> Arc<Self> {
        Arc::new(Self {
            revision: Mutex::new(0),
            focused_session: Mutex::new(None),
            stream,
        })
    }

    pub(crate) fn set_focused_session(&self, session_id: Option<String>) {
        *self.focused_session.lock().expect("focused session lock poisoned") = session_id;
    }

    pub(crate) fn revision(&self) -> u64 {
        *self.revision.lock().expect("stream revision lock poisoned")
    }

    /// 单独推进并返回新序号（用于仅返回 RPC 响应而不广播事件的模型配置读/改）。
    pub(crate) fn next_revision(&self) -> u64 {
        let mut revision = self.revision.lock().expect("stream revision lock poisoned");
        *revision += 1;
        *revision
    }

    /// 发布一个事件：过滤非聚焦会话的高频进度帧与背压时的高频帧，
    /// 仅对实际发出的事件递增全局序号并推送给前端。
    pub(crate) fn emit(&self, event: StreamEvent) -> u64 {
        let mut order = self.revision.lock().expect("stream revision lock poisoned");
        if let Some(target_session) = high_frequency_progress_session(&event) {
            let focused = self.focused_session.lock().expect("focused session lock poisoned");
            if focused.as_deref() != Some(target_session) {
                return *order;
            }
            drop(focused);
            if self.stream.capacity() < STREAM_RESERVED_CONTROL_SLOTS {
                let revision = *order;
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
        match self.stream.try_reserve() {
            Ok(permit) => {
                *order += 1;
                let revision = *order;
                permit.send(StreamEnvelope { revision, event });
                revision
            }
            Err(error) => {
                let revision = *order;
                drop(order);
                tracing::warn!(revision, error = %error, "failed to enqueue stream event");
                revision
            }
        }
    }

    /// 生成交给单个 `Conversation` 使用的发布回调。
    pub(crate) fn publisher(self: &Arc<Self>) -> Arc<dyn Fn(StreamEvent) -> u64 + Send + Sync> {
        let hub = Arc::clone(self);
        Arc::new(move |event| hub.emit(event))
    }
}

/// 识别可按焦点过滤、也可在背压高水位线时让路的高频进度帧，返回其所属会话 ID。
fn high_frequency_progress_session(event: &StreamEvent) -> Option<&str> {
    match event {
        StreamEvent::TurnEvent { session_id, payload }
            if matches!(
                payload.event,
                singularity_protocol::TurnEvent::TextDelta { .. }
                    | singularity_protocol::TurnEvent::ToolOutput { .. }
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
