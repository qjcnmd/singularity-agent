//! Terminal state classification, durable failure convergence, and terminal events.

use super::*;

pub(crate) fn turn_failure_cause(error: &AppServerError) -> TurnFailureCause {
    match error {
        AppServerError::Store(_) => TurnFailureCause::Store,
        AppServerError::ProjectInstructions(_) => TurnFailureCause::ProjectInstructions,
        AppServerError::Workspace(_) => TurnFailureCause::Workspace,
        AppServerError::Agent(AgentError::Provider(provider_error)) => {
            TurnFailureCause::Provider(provider_failure_kind(&provider_error.error.kind))
        }
        AppServerError::Agent(_) => TurnFailureCause::Internal,
        AppServerError::InvalidJson(_) => TurnFailureCause::Serialization,
        AppServerError::InvalidParams(_) => TurnFailureCause::Internal,
        AppServerError::Session(_) => TurnFailureCause::Store,
        AppServerError::TurnExecution { cause, .. }
        | AppServerError::TurnTerminalization { cause, .. } => *cause,
    }
}

pub(crate) fn provider_failure_kind(
    kind: &singularity_model::ModelErrorKind,
) -> ProviderFailureKind {
    use singularity_model::ModelErrorKind::*;
    match kind {
        RateLimited => ProviderFailureKind::RateLimited,
        BudgetExceeded => ProviderFailureKind::QuotaExceeded,
        NetworkError => ProviderFailureKind::Network,
        Timeout => ProviderFailureKind::Timeout,
        AuthError => ProviderFailureKind::Auth,
        InvalidRequest | ToolCallParseError | JsonSchemaViolation | ContentFilter => {
            ProviderFailureKind::Validation
        }
        ProviderOverloaded => ProviderFailureKind::Overloaded,
        Cancelled => ProviderFailureKind::Cancelled,
        ContextLengthExceeded => ProviderFailureKind::ContextOverflow,
        UnknownProviderError | UnsupportedCapability => ProviderFailureKind::Unknown,
    }
}

pub(crate) fn turn_failure_from_error(
    error: &AppServerError,
    fallback_stage: TurnFailureStage,
) -> TurnFailure {
    match error {
        AppServerError::TurnExecution {
            stage,
            cause,
            original,
        }
        | AppServerError::TurnTerminalization {
            stage,
            cause,
            original,
            ..
        } => TurnFailure {
            stage: *stage,
            cause: *cause,
            original: original.clone().or_else(|| Some(error.to_string())),
        },
        _ => TurnFailure {
            stage: fallback_stage,
            cause: turn_failure_cause(error),
            original: Some(error.to_string()),
        },
    }
}

pub(crate) fn terminal_metadata_for_status(
    turn_id: &str,
    status: SessionStatus,
) -> Option<singularity_agent::session::SessionMetadata> {
    match status {
        SessionStatus::Completed => Some(
            singularity_agent::session::SessionMetadata::turn_completed(turn_id),
        ),
        SessionStatus::Failed => Some(singularity_agent::session::SessionMetadata::turn_failed(
            turn_id,
            "turn failed",
        )),
        SessionStatus::Interrupted => Some(
            singularity_agent::session::SessionMetadata::turn_interrupted(
                turn_id,
                "turn interrupted",
                false,
            ),
        ),
        SessionStatus::Active => None,
    }
}

impl AppServer {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_agent_failure(
        &self,
        session: &mut SessionManager,
        turn_id: &str,
        assistant_events: &mut AssistantItemEventState,
        error: &AppServerError,
        usage: &ModelUsage,
        usage_complete: bool,
        emit: &mut impl FnMut(Value),
    ) -> AppServerResult<()> {
        let (usage, usage_complete) = match error {
            AppServerError::Agent(AgentError::RunFailed { outcome, .. }) => {
                (&outcome.usage, outcome.usage_complete)
            }
            _ => (usage, usage_complete),
        };
        let failure = turn_failure_from_error(error, TurnFailureStage::AgentLoop);
        let (metadata_error, durable) =
            self.persist_failure_state(session, turn_id, usage, usage_complete);
        if durable {
            let _ = self.emit_failure_terminal_events(
                turn_id,
                session.session_id(),
                assistant_events,
                &failure,
                emit,
            );
            Ok(())
        } else {
            let message = metadata_error
                .as_deref()
                .unwrap_or("failed to persist terminal failure state");
            let safe_message = if singularity_core::contains_sensitive_text(message) {
                "fatal storage error: failed to persist terminal metadata"
            } else {
                message
            };
            if let Ok(event) = self.event_notification(AppEvent::agent_diagnostic(
                session.session_id(),
                turn_id,
                "error",
                "storage_fatal",
                safe_message,
            )) {
                emit(event);
            }
            Err(AppServerError::TurnTerminalization {
                stage: TurnFailureStage::AgentLoop,
                cause: failure.cause,
                failure: TurnTerminalizationFailure::Store,
                original: metadata_error,
            })
        }
    }

    /// 首次失败记录后最多重试一次，并在必要时降级为 interrupted；返回首次
    /// durable 写失败文本，供 typed terminalization error 保留真实原因。
    pub(crate) fn persist_failure_state(
        &self,
        session: &mut SessionManager,
        turn_id: &str,
        usage: &ModelUsage,
        usage_complete: bool,
    ) -> (Option<String>, bool) {
        let first_error = match self.update_session_status_and_usage(
            session,
            Some(turn_id),
            SessionStatus::Failed,
            usage,
            usage_complete,
        ) {
            Ok(_) => return (None, true),
            Err(error) => error.to_string(),
        };
        if self
            .update_session_status_and_usage(
                session,
                Some(turn_id),
                SessionStatus::Failed,
                usage,
                usage_complete,
            )
            .is_ok()
        {
            return (Some(first_error), true);
        }
        // Do not write a terminal SQLite projection without its JSONL fact. The
        // next reopen will repair an active turn from turn_started, while an
        // index-only fallback would violate the JSONL-first ordering contract.
        let _ = usage;
        (Some(first_error), false)
    }

    /// 尽力发送失败 item 与 turn 级终态事件；一个事件失败不阻断另一个事件，
    /// 返回首个 notification failure 供 RPC 错误分类。
    pub(crate) fn emit_failure_terminal_events(
        &self,
        turn_id: &str,
        thread_id: &str,
        assistant_events: &mut AssistantItemEventState,
        failure: &TurnFailure,
        emit: &mut impl FnMut(Value),
    ) -> Option<TurnFailure> {
        let mut first_failure = None;
        if assistant_events.appeared() {
            match self.realtime_item_failed_event(assistant_events) {
                Ok(Some(event)) => emit(event),
                Ok(None) => {}
                Err(error) => {
                    first_failure = Some(turn_failure_from_error(
                        &error,
                        TurnFailureStage::EventNotification,
                    ));
                }
            }
        }
        for tool_call_id in assistant_events.open_tool_items() {
            match self.realtime_tool_terminal_event(assistant_events, &tool_call_id, true) {
                Ok(Some(event)) => emit(event),
                Ok(None) => {}
                Err(error) if first_failure.is_none() => {
                    first_failure = Some(turn_failure_from_error(
                        &error,
                        TurnFailureStage::EventNotification,
                    ));
                }
                Err(_) => {}
            }
        }
        let message = failure
            .original
            .clone()
            .unwrap_or_else(|| format!("turn failed during {} ({})", failure.stage, failure.cause));
        let message = if singularity_core::contains_sensitive_text(&message) {
            "Internal error".to_string()
        } else {
            message
        };
        match self.event_notification(AppEvent::turn_error(
            turn_id,
            thread_id,
            failure.stage.as_str(),
            failure.cause.as_str(),
            &message,
            false,
        )) {
            Ok(event) => emit(event),
            Err(error) if first_failure.is_none() => {
                first_failure = Some(turn_failure_from_error(
                    &error,
                    TurnFailureStage::EventNotification,
                ));
            }
            Err(_) => {}
        }
        first_failure
    }
}
