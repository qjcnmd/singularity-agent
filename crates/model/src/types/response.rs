use super::ModelUsage;
use super::message::Message;
use crate::ProviderError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelStopReason {
    Stop,
    Length,
    Failed(ProviderError),
}

#[must_use]
#[derive(Debug, Clone, PartialEq)]
pub struct ModelTurnResponse {
    pub assistant_message: Message,
    pub usage: Option<ModelUsage>,
    pub(crate) stop_reason: ModelStopReason,
    pub(crate) refusal: bool,
}

impl ModelTurnResponse {
    pub(crate) fn failed(error: ProviderError, assistant_message: Message) -> Self {
        Self {
            assistant_message,
            usage: None,
            stop_reason: ModelStopReason::Failed(error),
            refusal: false,
        }
    }

    pub fn error(&self) -> Option<&ProviderError> {
        match &self.stop_reason {
            ModelStopReason::Failed(error) => Some(error),
            ModelStopReason::Stop | ModelStopReason::Length => None,
        }
    }

    pub fn is_length_truncated(&self) -> bool {
        self.stop_reason == ModelStopReason::Length
    }

    pub fn is_refusal(&self) -> bool {
        self.refusal
    }
}
