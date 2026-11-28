use std::sync::{Arc, Mutex};

use protocol::{StreamEnvelope, StreamEvent};
use tokio::sync::mpsc;

pub(crate) struct StreamHub {
    revision: Mutex<u64>,
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

    pub(crate) fn next_revision(&self) -> u64 {
        let mut revision = self.revision.lock().expect("stream revision lock poisoned");
        *revision += 1;
        *revision
    }

    pub(crate) fn emit(&self, event: StreamEvent) -> u64 {
        let mut revision = self.revision.lock().expect("stream revision lock poisoned");
        if self.stream.is_closed() {
            return *revision;
        }
        if let Some(target_session) = high_frequency_session(&event) {
            let focused = self.focused_session.lock().expect("focused session lock poisoned");
            if focused.as_deref() != Some(target_session) {
                return *revision;
            }
        }
        match self.stream.try_reserve() {
            Ok(permit) => {
                *revision += 1;
                let current = *revision;
                permit.send(StreamEnvelope { revision: current, event });
                current
            }
            Err(mpsc::error::TrySendError::Closed(())) => *revision,
            Err(mpsc::error::TrySendError::Full(())) => {
                let current = *revision;
                drop(revision);
                tracing::warn!(revision = current, "stream channel saturated; dropping stream event");
                current
            }
        }
    }
}

fn high_frequency_session(event: &StreamEvent) -> Option<&str> {
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
