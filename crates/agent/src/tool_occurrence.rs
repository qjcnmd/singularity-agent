//! Typed runtime tool occurrence and event projection helpers.
//!
//! This module owns occurrence identity, lifecycle projection, and tool/provider observation
//! adapters. AgentLoopState remains owned by the parent module.

use sha2::{Digest, Sha256};
use singularity_model::{
    ModelToolCall, ModelToolParseStatus, ProviderAttemptEvent, ProviderAttemptStarted,
    ProviderStreamEvent,
};
use singularity_policy::{
    NetworkAccess, PermissionDecisionCause as PermissionCause, PermissionProfile,
};
use singularity_tools::{
    BoundToolCall, SandboxExecutionBoundary as ToolSandboxExecutionBoundary,
    SandboxExecutionObservation as ToolSandboxExecutionObservation,
    SandboxExecutionStatus as ToolSandboxExecutionStatus, ToolBrokerDecision, ToolExecutor,
    ToolFailureKind, ToolOutput, ToolResult, WorkspaceToolExecutor,
};

use super::observation::OccurrenceTimer;
use super::occurrence::ToolResultOccurrence;
use super::{
    AgentLoopEvent, AgentLoopEventCallback, AgentLoopEventSinkError, AgentLoopInput,
    AgentObservation, OccurrenceIdentity, OccurrenceLifecycle, PolicyDecisionCause,
    PolicyDecisionStatus, PreparedToolCall, PromptAssemblyObservation, PromptAssemblyStatus,
    ProviderAttemptObservation, ProviderAttemptStatus, ProviderAttemptUsageObservation,
    SandboxExecutionOccurrence, SandboxExecutionStatus, ToolCallObservation, ToolCallStatus,
    ToolResultObservation,
};

pub(super) struct ToolOccurrenceContext {
    pub(super) identity: OccurrenceIdentity,
    pub(super) timer: OccurrenceTimer,
    pub(super) model_turn_ordinal: u32,
    pub(super) tool_call_ordinal: u32,
    pub(super) tool_call_id_digest: String,
    pub(super) tool_name: String,
    pub(super) first_attempt: bool,
}

pub(super) struct ModelToolOccurrence {
    pub(super) call: ModelToolCall,
    pub(super) fingerprint: String,
    pub(super) invalid_was_observed: bool,
    pub(super) context: ToolOccurrenceContext,
}

pub(super) struct RuntimeToolResult {
    pub(super) result: ToolResult,
    pub(super) duration_ms: Option<u64>,
    pub(super) event_sink_failed: bool,
}

pub(super) struct WorkspaceToolExecution {
    pub(super) output: ToolOutput,
    pub(super) sandbox_execution: Option<ToolSandboxExecutionObservation>,
    pub(super) event_sink_failed: bool,
}

pub(super) struct WorkspaceToolCallContext<'a> {
    pub(super) bound: &'a BoundToolCall,
    pub(super) decision: &'a ToolBrokerDecision,
    pub(super) profile: &'a PermissionProfile,
    pub(super) occurrence: Option<&'a ToolOccurrenceContext>,
}

pub(super) struct ObservedToolDecision {
    pub(super) decision: ToolBrokerDecision,
    pub(super) cause: PolicyDecisionCause,
}

pub(super) fn emit_event(
    on_event: &mut Option<&mut AgentLoopEventCallback<'_>>,
    event: AgentLoopEvent,
) -> Result<(), AgentLoopEventSinkError> {
    match on_event.as_deref_mut() {
        Some(callback) => callback(event),
        None => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_prompt_assembly_finished(
    on_event: &mut Option<&mut AgentLoopEventCallback<'_>>,
    identity: OccurrenceIdentity,
    timer: &OccurrenceTimer,
    model_turn_ordinal: u32,
    message_count: u32,
    tool_count: u32,
    request_token_count: u32,
    request_digest: String,
    compacted: bool,
    status: PromptAssemblyStatus,
) -> Result<(), AgentLoopEventSinkError> {
    emit_event(
        on_event,
        AgentLoopEvent::Observation(AgentObservation::PromptAssembly(
            PromptAssemblyObservation {
                identity,
                lifecycle: timer.finished(status),
                model_turn_ordinal,
                message_count,
                tool_count,
                request_token_count,
                request_digest,
                compacted,
            },
        )),
    )
}

enum ProviderAttemptIdentityScope {
    Child(OccurrenceIdentity),
    Root {
        thread_id: String,
        turn_id: String,
        model_turn_ordinal: u32,
        resume_attempt: u32,
    },
}

pub(super) struct ProviderEventBridge<'a, 'callback_ref, 'callback> {
    identity_scope: ProviderAttemptIdentityScope,
    pub(super) on_event: &'a mut Option<&'callback_ref mut AgentLoopEventCallback<'callback>>,
    pub(super) streamed_text: String,
    buffered_text_deltas: Vec<String>,
    pub(super) next_attempt_ordinal: u32,
    pub(super) active_attempt: Option<(ProviderAttemptStarted, OccurrenceIdentity)>,
    pub(super) event_sink_failed: bool,
}

impl<'a, 'callback_ref, 'callback> ProviderEventBridge<'a, 'callback_ref, 'callback> {
    pub(super) fn new(
        prompt_identity: OccurrenceIdentity,
        on_event: &'a mut Option<&'callback_ref mut AgentLoopEventCallback<'callback>>,
    ) -> Self {
        Self {
            identity_scope: ProviderAttemptIdentityScope::Child(prompt_identity),
            on_event,
            streamed_text: String::new(),
            buffered_text_deltas: Vec::new(),
            next_attempt_ordinal: 0,
            active_attempt: None,
            event_sink_failed: false,
        }
    }

    pub(super) fn new_root(
        input: &AgentLoopInput,
        model_turn_ordinal: u32,
        on_event: &'a mut Option<&'callback_ref mut AgentLoopEventCallback<'callback>>,
    ) -> Self {
        Self {
            identity_scope: ProviderAttemptIdentityScope::Root {
                thread_id: input.thread_id.clone(),
                turn_id: input.turn_id.clone(),
                model_turn_ordinal,
                resume_attempt: input.resume_attempt,
            },
            on_event,
            streamed_text: String::new(),
            buffered_text_deltas: Vec::new(),
            next_attempt_ordinal: 0,
            active_attempt: None,
            event_sink_failed: false,
        }
    }

    pub(super) fn on_stream(&mut self, event: ProviderStreamEvent) {
        match event {
            ProviderStreamEvent::OutputTextDelta { delta } => {
                self.streamed_text.push_str(&delta);
                self.buffered_text_deltas.push(delta);
            }
        }
    }

    /// Return deltas only after the caller has validated the complete provider response.
    ///
    /// Terminal response deltas remain buffered until the complete provider response is validated.
    pub(super) fn into_buffered_text_deltas(self) -> Vec<String> {
        self.buffered_text_deltas
    }

    pub(super) fn on_attempt(&mut self, event: ProviderAttemptEvent) -> bool {
        if self.event_sink_failed {
            return false;
        }
        let result = match event {
            ProviderAttemptEvent::Started(started) => self.start_attempt(started),
            ProviderAttemptEvent::Finished(finished) => self.finish_attempt(finished),
        };
        if result.is_err() {
            self.event_sink_failed = true;
            return false;
        }
        true
    }

    fn start_attempt(&mut self, started: ProviderAttemptStarted) -> Result<(), ()> {
        if self.active_attempt.is_some() {
            return Err(());
        }
        let identity = match &self.identity_scope {
            ProviderAttemptIdentityScope::Child(parent) => {
                child_occurrence_identity(parent, "provider_attempt", self.next_attempt_ordinal)
            }
            ProviderAttemptIdentityScope::Root {
                thread_id,
                turn_id,
                model_turn_ordinal,
                resume_attempt,
            } => root_occurrence_identity(
                thread_id,
                turn_id,
                "provider_attempt",
                *model_turn_ordinal,
                self.next_attempt_ordinal,
                *resume_attempt,
            ),
        };
        let observation = ProviderAttemptObservation {
            identity: identity.clone(),
            lifecycle: OccurrenceLifecycle::Started {
                queued_at_unix_ms: started.started_at_unix_ms,
                started_at_unix_ms: started.started_at_unix_ms,
            },
            operation_phase: started.operation_phase,
            provider_name: started.provider_name.clone(),
            model_name: started.model_name.clone(),
            actual_api_protocol: started.actual_api_protocol,
            attempt_index: started.attempt_index,
            retry_count: started.attempt_index.saturating_sub(1),
            request_send_to_headers_ms: None,
            time_to_first_text_delta_ms: None,
            retry_backoff_ms: None,
            error_category: None,
            error_stage: None,
            diagnostic_code: None,
            usage: None,
        };
        emit_event(
            self.on_event,
            AgentLoopEvent::Observation(AgentObservation::ProviderAttempt(Box::new(observation))),
        )
        .map_err(|_| ())?;
        self.active_attempt = Some((started, identity));
        Ok(())
    }

    fn finish_attempt(
        &mut self,
        finished: singularity_model::ProviderAttemptOccurrence,
    ) -> Result<(), ()> {
        let Some((started, identity)) = self.active_attempt.take() else {
            return Err(());
        };
        if started.operation_phase != finished.operation_phase
            || started.provider_name != finished.provider_name
            || started.model_name != finished.model_name
            || started.actual_api_protocol != finished.actual_api_protocol
            || started.attempt_index != finished.attempt_index
            || started.started_at_unix_ms != finished.started_at_unix_ms
        {
            return Err(());
        }
        let status = match finished.terminal_status {
            singularity_model::ProviderAttemptStatus::Ok => ProviderAttemptStatus::Ok,
            singularity_model::ProviderAttemptStatus::Error => ProviderAttemptStatus::Error,
            singularity_model::ProviderAttemptStatus::Cancelled => ProviderAttemptStatus::Cancelled,
        };
        let usage = finished.usage.map(|usage| ProviderAttemptUsageObservation {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        });
        let observation = ProviderAttemptObservation {
            identity,
            lifecycle: OccurrenceLifecycle::Finished {
                queued_at_unix_ms: finished.started_at_unix_ms,
                started_at_unix_ms: finished.started_at_unix_ms,
                ended_at_unix_ms: finished.ended_at_unix_ms,
                duration_ms: finished.attempt_duration_ms,
                status,
            },
            operation_phase: finished.operation_phase,
            provider_name: finished.provider_name,
            model_name: finished.model_name,
            actual_api_protocol: finished.actual_api_protocol,
            attempt_index: finished.attempt_index,
            retry_count: finished.attempt_index.saturating_sub(1),
            request_send_to_headers_ms: finished.request_send_to_headers_ms,
            time_to_first_text_delta_ms: finished.time_to_first_text_delta_ms,
            retry_backoff_ms: finished.retry_backoff_ms,
            error_category: finished.error_category,
            error_stage: finished.error_stage,
            diagnostic_code: finished.diagnostic_code,
            usage,
        };
        emit_event(
            self.on_event,
            AgentLoopEvent::Observation(AgentObservation::ProviderAttempt(Box::new(observation))),
        )
        .map_err(|_| ())?;
        self.next_attempt_ordinal = self.next_attempt_ordinal.saturating_add(1);
        Ok(())
    }
}

pub(super) fn occurrence_identity(
    input: &AgentLoopInput,
    kind: &str,
    model_turn_ordinal: u32,
    ordinal: u32,
    parent_occurrence_id: Option<String>,
) -> OccurrenceIdentity {
    let mut identity = root_occurrence_identity(
        &input.thread_id,
        &input.turn_id,
        kind,
        model_turn_ordinal,
        ordinal,
        input.resume_attempt,
    );
    identity.parent_occurrence_id = parent_occurrence_id;
    identity
}

pub(super) fn root_occurrence_identity(
    thread_id: &str,
    turn_id: &str,
    kind: &str,
    model_turn_ordinal: u32,
    ordinal: u32,
    resume_attempt: u32,
) -> OccurrenceIdentity {
    let encoded = if resume_attempt == 0 {
        format!(
            "{}\u{0}{}\u{0}{kind}\u{0}{model_turn_ordinal}\u{0}{ordinal}",
            thread_id, turn_id
        )
    } else {
        format!(
            "{}\u{0}{}\u{0}{kind}\u{0}{model_turn_ordinal}\u{0}{ordinal}\u{0}{resume_attempt}",
            thread_id, turn_id
        )
    };
    OccurrenceIdentity {
        occurrence_id: format!("sha256:{:x}", Sha256::digest(encoded.as_bytes())),
        parent_occurrence_id: None,
        ordinal,
    }
}

pub(super) fn child_occurrence_identity(
    parent: &OccurrenceIdentity,
    kind: &str,
    ordinal: u32,
) -> OccurrenceIdentity {
    let encoded = format!("{}\u{0}{kind}\u{0}{ordinal}", parent.occurrence_id);
    OccurrenceIdentity {
        occurrence_id: format!("sha256:{:x}", Sha256::digest(encoded.as_bytes())),
        parent_occurrence_id: Some(parent.occurrence_id.clone()),
        ordinal,
    }
}

pub(super) fn tool_occurrence_context(
    input: &AgentLoopInput,
    call: &ModelToolCall,
    model_turn_ordinal: u32,
    tool_call_ordinal: u32,
) -> ToolOccurrenceContext {
    let prompt_parent = occurrence_identity(input, "prompt_assembly", model_turn_ordinal, 0, None);
    ToolOccurrenceContext {
        identity: occurrence_identity(
            input,
            "tool_call",
            model_turn_ordinal,
            tool_call_ordinal,
            Some(prompt_parent.occurrence_id),
        ),
        timer: OccurrenceTimer::start(),
        model_turn_ordinal,
        tool_call_ordinal,
        tool_call_id_digest: format!("sha256:{:x}", Sha256::digest(call.tool_call_id.as_bytes())),
        tool_name: safe_tool_name(call),
        first_attempt: true,
    }
}

fn safe_tool_name(call: &ModelToolCall) -> String {
    if call.parse_status == ModelToolParseStatus::Valid
        && !call.tool_name.is_empty()
        && call.tool_name.len() <= 64
        && call
            .tool_name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        call.tool_name.clone()
    } else {
        "invalid_tool".to_string()
    }
}

pub(super) fn tool_call_event(
    context: &ToolOccurrenceContext,
    lifecycle: OccurrenceLifecycle<ToolCallStatus>,
) -> AgentLoopEvent {
    AgentLoopEvent::Observation(AgentObservation::ToolCall(ToolCallObservation {
        identity: context.identity.clone(),
        lifecycle,
        model_turn_ordinal: context.model_turn_ordinal,
        tool_call_ordinal: context.tool_call_ordinal,
        tool_call_id_digest: context.tool_call_id_digest.clone(),
        tool_name: context.tool_name.clone(),
        first_attempt: context.first_attempt,
    }))
}

/// Emit the canonical result occurrence after the reducer has appended it to checkpoint state.
pub(super) fn tool_result_event(
    context: &ToolOccurrenceContext,
    status: ToolCallStatus,
    occurrence: &ToolResultOccurrence,
) -> AgentLoopEvent {
    AgentLoopEvent::Observation(AgentObservation::ToolResult(Box::new(
        ToolResultObservation {
            identity: context.identity.clone(),
            tool_call_ordinal: context.tool_call_ordinal,
            tool_call_id_digest: context.tool_call_id_digest.clone(),
            tool_name: context.tool_name.clone(),
            first_attempt: context.first_attempt,
            status,
            visibility: occurrence.visibility(),
            occurrence: Some(occurrence.clone()),
        },
    )))
}

pub(super) fn emit_rejected_tool_calls(
    on_event: &mut Option<&mut AgentLoopEventCallback<'_>>,
    input: &AgentLoopInput,
    calls: &[ModelToolCall],
    model_turn_ordinal: u32,
    first_attempts: &[bool],
) -> Result<(), AgentLoopEventSinkError> {
    if first_attempts.len() != calls.len() {
        return Err(AgentLoopEventSinkError);
    }
    for (ordinal, (call, first_attempt)) in calls.iter().zip(first_attempts).enumerate() {
        let context = tool_occurrence_context(
            input,
            call,
            model_turn_ordinal,
            u32::try_from(ordinal).unwrap_or(u32::MAX),
        );
        let mut context = context;
        context.first_attempt = *first_attempt;
        emit_event(on_event, tool_call_event(&context, context.timer.started()))?;
        emit_event(
            on_event,
            tool_call_event(&context, context.timer.finished(ToolCallStatus::Rejected)),
        )?;
    }
    Ok(())
}

pub(super) fn tool_result_status(
    prepared: &PreparedToolCall,
    result: &ToolResult,
    batch_rejected: bool,
) -> ToolCallStatus {
    if batch_rejected {
        ToolCallStatus::BatchRejected
    } else if result.failure_kind == Some(ToolFailureKind::Cancelled) {
        ToolCallStatus::Cancelled
    } else if matches!(prepared.decision, Some(ToolBrokerDecision::Deny { .. })) {
        ToolCallStatus::PolicyDenied
    } else if prepared.rejection.is_some() {
        ToolCallStatus::Rejected
    } else if result.ok {
        ToolCallStatus::Succeeded
    } else {
        ToolCallStatus::Failed
    }
}

pub(super) fn policy_status(decision: &ToolBrokerDecision) -> PolicyDecisionStatus {
    match decision {
        ToolBrokerDecision::Allow | ToolBrokerDecision::Approved { .. } => {
            PolicyDecisionStatus::Allow
        }
        ToolBrokerDecision::Ask { .. } => PolicyDecisionStatus::Ask,
        ToolBrokerDecision::Deny { .. } => PolicyDecisionStatus::Deny,
    }
}

pub(super) fn tool_operation_count(bound: &BoundToolCall, profile: &PermissionProfile) -> u32 {
    if matches!(
        bound.executor,
        ToolExecutor::Workspace(WorkspaceToolExecutor::Command)
    ) && profile.network_access == NetworkAccess::Allowed
    {
        2
    } else {
        1
    }
}

pub(super) fn safe_policy_cause(cause: &PermissionCause) -> PolicyDecisionCause {
    match cause {
        PermissionCause::Explicit => PolicyDecisionCause::Explicit,
        PermissionCause::Rule => PolicyDecisionCause::Rule,
        PermissionCause::FilesystemProfile => PolicyDecisionCause::FilesystemProfile,
        PermissionCause::NetworkProfile => PolicyDecisionCause::NetworkProfile,
        PermissionCause::ProtectedResource => PolicyDecisionCause::ProtectedResource,
        PermissionCause::NoMatchingRule => PolicyDecisionCause::NoMatchingRule,
        PermissionCause::ApprovalPolicy => PolicyDecisionCause::ApprovalPolicy,
    }
}

pub(super) fn sandbox_status(status: ToolSandboxExecutionStatus) -> SandboxExecutionStatus {
    match status {
        ToolSandboxExecutionStatus::Ok => SandboxExecutionStatus::Ok,
        ToolSandboxExecutionStatus::Error => SandboxExecutionStatus::Error,
        ToolSandboxExecutionStatus::TimedOut => SandboxExecutionStatus::TimedOut,
        ToolSandboxExecutionStatus::Cancelled => SandboxExecutionStatus::Cancelled,
    }
}

pub(super) fn sandbox_boundary_event(
    occurrence: &ToolOccurrenceContext,
    boundary: ToolSandboxExecutionBoundary,
) -> AgentLoopEvent {
    let identity = child_occurrence_identity(&occurrence.identity, "sandbox_execution", 0);
    let observation = match boundary {
        ToolSandboxExecutionBoundary::Started {
            command_id,
            started_at_unix_ms,
        } => SandboxExecutionOccurrence {
            identity,
            lifecycle: OccurrenceLifecycle::Started {
                queued_at_unix_ms: started_at_unix_ms,
                started_at_unix_ms,
            },
            command_id,
            command_id_binding_valid: None,
            workspace_mutation: None,
            enforcement: None,
        },
        ToolSandboxExecutionBoundary::Finished(sandbox) => SandboxExecutionOccurrence {
            identity,
            lifecycle: OccurrenceLifecycle::Finished {
                queued_at_unix_ms: sandbox.started_at_unix_ms,
                started_at_unix_ms: sandbox.started_at_unix_ms,
                ended_at_unix_ms: sandbox.ended_at_unix_ms,
                duration_ms: sandbox.duration_ms,
                status: sandbox_status(sandbox.status),
            },
            command_id: sandbox.command_id,
            command_id_binding_valid: Some(sandbox.command_id_binding_valid),
            workspace_mutation: Some(sandbox.workspace_mutation),
            enforcement: Some(sandbox.enforcement),
        },
    };
    AgentLoopEvent::Observation(AgentObservation::SandboxExecution(observation))
}
