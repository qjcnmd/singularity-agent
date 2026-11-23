use std::sync::{Arc, Mutex};

use protocol::{StreamEnvelope, StreamEvent};
use tokio::sync::mpsc;

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

    pub(crate) fn next_revision(&self) -> u64 {
        let mut order = self.order.lock().expect("stream revision lock poisoned");
        order.revision += 1;
        order.revision
    }

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
