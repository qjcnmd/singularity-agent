use serde_json::json;
use sha2::{Digest, Sha256};
use singularity_agent::{
    AgentLoopEvent, AgentObservation, AgentRunStatus, FinalReviewStatus, OccurrenceIdentity,
    OccurrenceLifecycle, PolicyDecisionCause, PolicyDecisionObservation, PolicyDecisionStatus,
    PromptAssemblyStatus, SandboxExecutionOccurrence, SandboxExecutionStatus, ToolCallStatus,
    VerificationStatus,
};
use singularity_core::Timestamp;
use singularity_model::{
    ModelErrorCategory, ProviderApiProtocol, ProviderAttemptOccurrence,
    ProviderAttemptOperationPhase, ProviderAttemptStatus, ProviderCapabilityCacheLookupResult,
};
use singularity_protocol::{
    TraceErrorCategory, TraceErrorProjection, TraceErrorStage, TraceEvent,
    TraceFinalReviewProjection, TraceFinalReviewStatus, TraceMetricSample, TraceMetricSampleKind,
    TracePolicyCause, TracePolicyDecision, TracePolicyProjection, TraceProviderOperationPhase,
    TraceProviderProtocol, TraceSandboxEnforcement, TraceSandboxProjection, TraceSandboxStatus,
    TraceSpanKind, TraceSpanPhase, TraceSpanProjection, TraceSpanStatus, TraceToolProjection,
    TraceToolStatus, TraceUsage, TraceVerificationProjection, TraceVerificationStatus,
    TraceWorkspaceMutation,
};
use singularity_store::{SessionStore, StoreError};
use singularity_tools::{SandboxBackendEnforcement, WorkspaceMutation};

/// Projects already-typed Agent/Provider runtime occurrences into the single Store trace stream.
pub(crate) struct TraceProjector<'a> {
    store: &'a SessionStore,
    run_id: String,
    session_id: String,
    task_id: Option<String>,
    turn_span_id: String,
}

#[derive(Clone)]
struct SpanDescriptor {
    span_id: String,
    parent_span_id: Option<String>,
    kind: TraceSpanKind,
    summary: &'static str,
}

struct ProjectedSpan {
    descriptor: SpanDescriptor,
    phase: TraceSpanPhase,
    timestamp_unix_ms: u64,
    status: Option<TraceSpanStatus>,
    duration_ms: Option<u64>,
    time_to_first_token_ms: Option<u64>,
    projection: TraceSpanProjection,
    metric_samples: Vec<TraceMetricSample>,
    idempotent_start: bool,
}

impl<'a> TraceProjector<'a> {
    /// Bind the projector to the persisted Turn root rather than reconstructing its identity.
    pub(crate) fn new(
        store: &'a SessionStore,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<Self, StoreError> {
        let turn_span_id = store
            .list_trace(thread_id)?
            .into_iter()
            .find(|event| {
                event.session_id == turn_id
                    && event.span_kind == Some(TraceSpanKind::Turn)
                    && event.span_phase == Some(TraceSpanPhase::Start)
            })
            .and_then(|event| event.span_id)
            .ok_or_else(|| {
                StoreError::InvalidState(format!(
                    "turn {turn_id} is missing its persisted typed root"
                ))
            })?;
        Ok(Self {
            store,
            run_id: thread_id.to_string(),
            session_id: turn_id.to_string(),
            task_id: Some(turn_id.to_string()),
            turn_span_id,
        })
    }

    /// Bind the projector to an evaluation run that intentionally has no
    /// ordinary Thread/Turn rows.  The run/session ids remain explicit in the
    /// trace and Store still validates the typed span graph and payload.
    pub(crate) fn new_external(
        store: &'a SessionStore,
        run_id: &str,
        session_id: &str,
        turn_span_id: &str,
    ) -> Self {
        Self {
            store,
            run_id: run_id.to_string(),
            session_id: session_id.to_string(),
            task_id: None,
            turn_span_id: turn_span_id.to_string(),
        }
    }

    /// Project one AgentLoop event in callback order; FinalTextDelta has no trace side effect.
    pub(crate) fn project_event(&mut self, event: AgentLoopEvent) -> Result<(), StoreError> {
        match event {
            AgentLoopEvent::FinalTextDelta { .. } => Ok(()),
            AgentLoopEvent::Observation(observation) => self.project_observation(observation),
        }
    }

    /// Project result-only provider occurrences and cache observations after the loop returns.
    pub(crate) fn project_result(&mut self, status: &AgentRunStatus) -> Result<(), StoreError> {
        for occurrence in &status.provider_attempts.occurrences {
            self.project_provider_attempt(occurrence)?;
        }
        if let Some(metadata) = &status.provider_capability_metadata {
            for (index, observation) in metadata.cache_observations.iter().enumerate() {
                let timestamp = Timestamp::from_unix_ms(observation.observed_at_unix_ms)
                    .map(|value| value.to_string())
                    .ok_or_else(|| {
                        StoreError::InvalidState(
                            "provider capability cache observation timestamp is out of range"
                                .to_string(),
                        )
                    })?;
                let kind = match observation.outcome {
                    ProviderCapabilityCacheLookupResult::Hit => {
                        TraceMetricSampleKind::ProviderCapabilityCacheHit
                    }
                    ProviderCapabilityCacheLookupResult::Miss => {
                        TraceMetricSampleKind::ProviderCapabilityCacheMiss
                    }
                };
                self.append_metric_sample(
                    &format!("cache_{index}"),
                    &timestamp,
                    kind,
                    "provider capability cache observation",
                )?;
            }
        }
        Ok(())
    }

    fn project_observation(&mut self, observation: AgentObservation) -> Result<(), StoreError> {
        match observation {
            AgentObservation::PromptAssembly(observation) => {
                let identity = observation.identity.clone();
                let start_projection = TraceSpanProjection {
                    finalization_only: Some(observation.finalization_only),
                    model_turn_ordinal: Some(u64::from(observation.model_turn_ordinal)),
                    ..TraceSpanProjection::default()
                };
                let end_projection = TraceSpanProjection {
                    operation_count: Some(1),
                    message_count: Some(u64::from(observation.message_count)),
                    tool_count: Some(u64::from(observation.tool_count)),
                    request_token_count: Some(u64::from(observation.request_token_count)),
                    request_digest: non_empty(observation.request_digest.clone()),
                    compacted: Some(observation.compacted),
                    finalization_only: Some(observation.finalization_only),
                    model_turn_ordinal: Some(u64::from(observation.model_turn_ordinal)),
                    ..TraceSpanProjection::default()
                };
                self.append_lifecycle(
                    self.observation_span(
                        &identity,
                        TraceSpanKind::PromptAssembly,
                        "prompt assembly",
                    ),
                    &observation.lifecycle,
                    start_projection,
                    |_| end_projection.clone(),
                    prompt_status,
                    |_| Vec::new(),
                )?;
                Ok(())
            }
            AgentObservation::ToolCall(observation) => {
                let start = TraceToolProjection {
                    tool_name: Some(observation.tool_name.clone()),
                    tool_call_id_digest: Some(observation.tool_call_id_digest.clone()),
                    tool_call_ordinal: Some(u64::from(observation.tool_call_ordinal)),
                    ..TraceToolProjection::default()
                };
                self.append_lifecycle(
                    self.observation_span(
                        &observation.identity,
                        TraceSpanKind::ToolCall,
                        "tool call",
                    ),
                    &observation.lifecycle,
                    TraceSpanProjection {
                        tool: Some(start.clone()),
                        ..TraceSpanProjection::default()
                    },
                    |status| TraceSpanProjection {
                        tool: Some(TraceToolProjection {
                            status: Some(tool_status(*status)),
                            ..start.clone()
                        }),
                        ..TraceSpanProjection::default()
                    },
                    tool_span_status,
                    |_| Vec::new(),
                )
            }
            AgentObservation::PolicyDecision(observation) => {
                let start = TracePolicyProjection {
                    operation_count: Some(u64::from(observation.operation_count)),
                    resource_count: Some(u64::from(observation.resource_count)),
                    ..TracePolicyProjection::default()
                };
                self.append_lifecycle(
                    self.observation_span(
                        &observation.identity,
                        TraceSpanKind::PolicyDecision,
                        "policy decision",
                    ),
                    &observation.lifecycle,
                    TraceSpanProjection {
                        policy: Some(start.clone()),
                        ..TraceSpanProjection::default()
                    },
                    |status| TraceSpanProjection {
                        policy: Some(TracePolicyProjection {
                            decision: Some(policy_decision(*status)),
                            cause: observation.lifecycle_cause(),
                            ..start.clone()
                        }),
                        ..TraceSpanProjection::default()
                    },
                    policy_span_status,
                    |_| Vec::new(),
                )
            }
            AgentObservation::SandboxExecution(observation) => self.project_sandbox(observation),
            AgentObservation::Verification(observation) => {
                let start = TraceVerificationProjection {
                    required_command_count: Some(u64::from(observation.required_command_count)),
                    satisfied_command_count: Some(u64::from(observation.satisfied_command_count)),
                    occurrence_count: Some(u64::from(observation.occurrence_count)),
                    ..TraceVerificationProjection::default()
                };
                self.append_lifecycle(
                    self.observation_span(
                        &observation.identity,
                        TraceSpanKind::Verification,
                        "verification",
                    ),
                    &observation.lifecycle,
                    TraceSpanProjection {
                        verification: Some(start.clone()),
                        ..TraceSpanProjection::default()
                    },
                    |status| TraceSpanProjection {
                        verification: Some(TraceVerificationProjection {
                            status: Some(verification_status(*status)),
                            command_duration_ms: observation.command_duration_ms,
                            ..start.clone()
                        }),
                        ..TraceSpanProjection::default()
                    },
                    verification_span_status,
                    verification_metric_samples,
                )
            }
            AgentObservation::FinalReview(observation) => {
                let start = TraceFinalReviewProjection {
                    model_turn_ordinal: Some(u64::from(observation.model_turn_ordinal)),
                    ..TraceFinalReviewProjection::default()
                };
                self.append_lifecycle(
                    self.observation_span(
                        &observation.identity,
                        TraceSpanKind::FinalReview,
                        "final review",
                    ),
                    &observation.lifecycle,
                    TraceSpanProjection {
                        final_review: Some(start.clone()),
                        ..TraceSpanProjection::default()
                    },
                    |status| TraceSpanProjection {
                        final_review: Some(TraceFinalReviewProjection {
                            status: Some(final_review_status(*status)),
                            ..start.clone()
                        }),
                        ..TraceSpanProjection::default()
                    },
                    final_review_span_status,
                    |_| Vec::new(),
                )
            }
        }
    }

    fn project_sandbox(
        &mut self,
        observation: SandboxExecutionOccurrence,
    ) -> Result<(), StoreError> {
        let command_id_digest = digest_identifier(&observation.command_id);
        let start = TraceSandboxProjection {
            command_id_digest: Some(command_id_digest.clone()),
            ..TraceSandboxProjection::default()
        };
        self.append_lifecycle(
            self.observation_span(
                &observation.identity,
                TraceSpanKind::SandboxExecution,
                "sandbox execution",
            ),
            &observation.lifecycle,
            TraceSpanProjection {
                sandbox: Some(start.clone()),
                ..TraceSpanProjection::default()
            },
            |status| TraceSpanProjection {
                sandbox: Some(TraceSandboxProjection {
                    command_id_digest: Some(command_id_digest.clone()),
                    command_id_binding_valid: observation.command_id_binding_valid,
                    status: Some(sandbox_status(*status)),
                    workspace_mutation: observation.workspace_mutation.map(workspace_mutation),
                    enforcement: observation.enforcement.clone().map(sandbox_enforcement),
                }),
                ..TraceSpanProjection::default()
            },
            sandbox_span_status,
            |_| Vec::new(),
        )
    }

    fn project_provider_attempt(
        &mut self,
        occurrence: &ProviderAttemptOccurrence,
    ) -> Result<(), StoreError> {
        let span_id = provider_span_id(occurrence);
        let parent_span_id = occurrence
            .parent_occurrence_id
            .clone()
            .unwrap_or_else(|| self.turn_span_id.clone());
        let descriptor = SpanDescriptor {
            span_id,
            parent_span_id: Some(parent_span_id),
            kind: TraceSpanKind::ProviderAttempt,
            summary: "provider attempt",
        };
        let start_projection = provider_projection(occurrence, false);
        let end_projection = provider_projection(occurrence, true);
        self.append_span(ProjectedSpan {
            descriptor: descriptor.clone(),
            phase: TraceSpanPhase::Start,
            timestamp_unix_ms: occurrence.started_at_unix_ms,
            status: None,
            duration_ms: None,
            time_to_first_token_ms: None,
            projection: start_projection,
            metric_samples: Vec::new(),
            idempotent_start: true,
        })?;
        self.append_span(ProjectedSpan {
            descriptor,
            phase: TraceSpanPhase::End,
            timestamp_unix_ms: occurrence.ended_at_unix_ms,
            status: Some(provider_span_status(occurrence.terminal_status)),
            duration_ms: Some(occurrence.attempt_duration_ms),
            time_to_first_token_ms: occurrence.time_to_first_text_delta_ms,
            projection: end_projection,
            metric_samples: Vec::new(),
            idempotent_start: false,
        })
    }

    fn observation_span(
        &self,
        identity: &OccurrenceIdentity,
        kind: TraceSpanKind,
        summary: &'static str,
    ) -> SpanDescriptor {
        SpanDescriptor {
            span_id: identity.occurrence_id.clone(),
            parent_span_id: identity
                .parent_occurrence_id
                .clone()
                .or_else(|| Some(self.turn_span_id.clone())),
            kind,
            summary,
        }
    }

    fn append_lifecycle<S>(
        &mut self,
        descriptor: SpanDescriptor,
        lifecycle: &OccurrenceLifecycle<S>,
        start_projection: TraceSpanProjection,
        end_projection: impl Fn(&S) -> TraceSpanProjection,
        end_status: impl Fn(&S) -> TraceSpanStatus,
        end_metric_samples: impl Fn(&S) -> Vec<TraceMetricSample>,
    ) -> Result<(), StoreError> {
        match lifecycle {
            OccurrenceLifecycle::Started {
                started_at_unix_ms, ..
            } => self.append_span(ProjectedSpan {
                descriptor,
                phase: TraceSpanPhase::Start,
                timestamp_unix_ms: *started_at_unix_ms,
                status: None,
                duration_ms: None,
                time_to_first_token_ms: None,
                projection: start_projection,
                metric_samples: Vec::new(),
                idempotent_start: true,
            }),
            OccurrenceLifecycle::Suspended { .. } => Ok(()),
            OccurrenceLifecycle::Finished {
                ended_at_unix_ms,
                duration_ms,
                status,
                ..
            } => self.append_span(ProjectedSpan {
                descriptor,
                phase: TraceSpanPhase::End,
                timestamp_unix_ms: *ended_at_unix_ms,
                status: Some(end_status(status)),
                duration_ms: Some(*duration_ms),
                time_to_first_token_ms: None,
                projection: end_projection(status),
                metric_samples: end_metric_samples(status),
                idempotent_start: false,
            }),
        }
    }

    fn append_span(&self, span: ProjectedSpan) -> Result<(), StoreError> {
        let timestamp = Timestamp::from_unix_ms(span.timestamp_unix_ms).ok_or_else(|| {
            StoreError::InvalidState("trace occurrence timestamp is out of range".to_string())
        })?;
        let mut event = self.new_trace_event(
            trace_event_id(&self.session_id, &span.descriptor.span_id, span.phase),
            span.descriptor.summary,
        );
        event.timestamp = Some(timestamp.to_string());
        event.span_id = Some(span.descriptor.span_id);
        event.parent_span_id = span.descriptor.parent_span_id;
        event.span_kind = Some(span.descriptor.kind);
        event.span_phase = Some(span.phase);
        event.span_status = span.status;
        event.duration_ms = span.duration_ms;
        event.time_to_first_token_ms = span.time_to_first_token_ms;
        event.span_projection = Some(span.projection);
        event.metric_samples = span.metric_samples;
        event.payload = json!({"observation": span.descriptor.kind.as_storage_text()});
        if span.idempotent_start {
            self.store.append_trace_idempotent(&event).map(|_| ())
        } else {
            self.store.append_trace(&event)
        }
    }

    fn append_metric_sample(
        &self,
        identity: &str,
        timestamp: &str,
        kind: TraceMetricSampleKind,
        summary: &str,
    ) -> Result<(), StoreError> {
        let mut event = self.new_trace_event(
            trace_event_id(&self.session_id, identity, TraceSpanPhase::End),
            summary,
        );
        event.timestamp = Some(timestamp.to_string());
        event.payload = json!({"observation": "provider_capability_cache"});
        event.metric_samples = vec![TraceMetricSample { kind, count: 1 }];
        self.store.append_trace(&event)
    }

    fn new_trace_event(&self, event_id: String, summary: &str) -> TraceEvent {
        if self.task_id.is_some() {
            TraceEvent::for_turn(
                event_id,
                self.run_id.clone(),
                self.session_id.clone(),
                "observability",
                summary,
            )
        } else {
            TraceEvent::new(
                event_id,
                self.run_id.clone(),
                self.session_id.clone(),
                "observability",
                summary,
            )
        }
    }
}

fn trace_event_id(turn_id: &str, identity: &str, phase: TraceSpanPhase) -> String {
    let material = format!("{turn_id}\u{0}{identity}\u{0}{}", phase.as_storage_text());
    format!("trace_obs_{:x}", Sha256::digest(material.as_bytes()))
}

fn provider_span_id(occurrence: &ProviderAttemptOccurrence) -> String {
    let material = format!(
        "{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}",
        occurrence.provider_name,
        occurrence.model_name,
        provider_phase_key(occurrence.operation_phase),
        provider_protocol_key(occurrence.actual_api_protocol),
        occurrence.attempt_index,
        occurrence.started_at_unix_ms,
    );
    format!("sha256:{:x}", Sha256::digest(material.as_bytes()))
}

fn digest_identifier(value: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(value.as_bytes()))
}

fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn prompt_status(status: &PromptAssemblyStatus) -> TraceSpanStatus {
    match status {
        PromptAssemblyStatus::Ready => TraceSpanStatus::Ok,
        PromptAssemblyStatus::ToolViewRejected
        | PromptAssemblyStatus::ContextOverflow
        | PromptAssemblyStatus::ValidationFailed => TraceSpanStatus::Error,
    }
}

fn tool_status(status: ToolCallStatus) -> TraceToolStatus {
    match status {
        ToolCallStatus::Succeeded => TraceToolStatus::Succeeded,
        ToolCallStatus::Failed => TraceToolStatus::Failed,
        ToolCallStatus::Cancelled => TraceToolStatus::Cancelled,
        ToolCallStatus::Rejected => TraceToolStatus::Rejected,
        ToolCallStatus::PolicyDenied => TraceToolStatus::PolicyDenied,
        ToolCallStatus::ApprovalRequired => TraceToolStatus::ApprovalRequired,
        ToolCallStatus::BatchRejected => TraceToolStatus::BatchRejected,
    }
}

fn tool_span_status(status: &ToolCallStatus) -> TraceSpanStatus {
    match status {
        ToolCallStatus::Cancelled => TraceSpanStatus::Cancelled,
        ToolCallStatus::Succeeded => TraceSpanStatus::Ok,
        _ => TraceSpanStatus::Error,
    }
}

fn policy_decision(status: PolicyDecisionStatus) -> TracePolicyDecision {
    match status {
        PolicyDecisionStatus::Allow => TracePolicyDecision::Allow,
        PolicyDecisionStatus::Ask => TracePolicyDecision::Ask,
        PolicyDecisionStatus::Deny => TracePolicyDecision::Deny,
    }
}

fn policy_span_status(status: &PolicyDecisionStatus) -> TraceSpanStatus {
    match status {
        PolicyDecisionStatus::Deny => TraceSpanStatus::Error,
        PolicyDecisionStatus::Allow | PolicyDecisionStatus::Ask => TraceSpanStatus::Ok,
    }
}

fn verification_status(status: VerificationStatus) -> TraceVerificationStatus {
    match status {
        VerificationStatus::CommandPassed => TraceVerificationStatus::CommandPassed,
        VerificationStatus::CommandFailed => TraceVerificationStatus::CommandFailed,
        VerificationStatus::GatePassed => TraceVerificationStatus::GatePassed,
        VerificationStatus::GateRejected => TraceVerificationStatus::GateRejected,
        VerificationStatus::RepairRequested => TraceVerificationStatus::RepairRequested,
    }
}

fn verification_span_status(status: &VerificationStatus) -> TraceSpanStatus {
    match status {
        VerificationStatus::CommandPassed | VerificationStatus::GatePassed => TraceSpanStatus::Ok,
        VerificationStatus::CommandFailed
        | VerificationStatus::GateRejected
        | VerificationStatus::RepairRequested => TraceSpanStatus::Error,
    }
}

fn verification_metric_samples(status: &VerificationStatus) -> Vec<TraceMetricSample> {
    match status {
        VerificationStatus::GateRejected => vec![TraceMetricSample {
            kind: TraceMetricSampleKind::CompletionRejection,
            count: 1,
        }],
        VerificationStatus::RepairRequested => vec![TraceMetricSample {
            kind: TraceMetricSampleKind::CompletionRepair,
            count: 1,
        }],
        _ => Vec::new(),
    }
}

fn sandbox_status(status: SandboxExecutionStatus) -> TraceSandboxStatus {
    match status {
        SandboxExecutionStatus::Ok => TraceSandboxStatus::Ok,
        SandboxExecutionStatus::Error => TraceSandboxStatus::Error,
        SandboxExecutionStatus::TimedOut => TraceSandboxStatus::TimedOut,
        SandboxExecutionStatus::Cancelled => TraceSandboxStatus::Cancelled,
    }
}

fn sandbox_span_status(status: &SandboxExecutionStatus) -> TraceSpanStatus {
    match status {
        SandboxExecutionStatus::Ok => TraceSpanStatus::Ok,
        SandboxExecutionStatus::Cancelled => TraceSpanStatus::Cancelled,
        SandboxExecutionStatus::Error | SandboxExecutionStatus::TimedOut => TraceSpanStatus::Error,
    }
}

fn workspace_mutation(value: WorkspaceMutation) -> TraceWorkspaceMutation {
    match value {
        WorkspaceMutation::Unchanged => TraceWorkspaceMutation::Unchanged,
        WorkspaceMutation::Changed => TraceWorkspaceMutation::Changed,
        WorkspaceMutation::Unknown => TraceWorkspaceMutation::Unknown,
    }
}

fn sandbox_enforcement(value: SandboxBackendEnforcement) -> TraceSandboxEnforcement {
    match value {
        SandboxBackendEnforcement::Strict => TraceSandboxEnforcement::Strict,
        SandboxBackendEnforcement::RestrictedToken => TraceSandboxEnforcement::RestrictedToken,
        SandboxBackendEnforcement::Unavailable => TraceSandboxEnforcement::Unavailable,
    }
}

fn final_review_status(status: FinalReviewStatus) -> TraceFinalReviewStatus {
    match status {
        FinalReviewStatus::Succeeded => TraceFinalReviewStatus::Succeeded,
        FinalReviewStatus::Failed => TraceFinalReviewStatus::Failed,
        FinalReviewStatus::Cancelled => TraceFinalReviewStatus::Cancelled,
    }
}

fn final_review_span_status(status: &FinalReviewStatus) -> TraceSpanStatus {
    match status {
        FinalReviewStatus::Succeeded => TraceSpanStatus::Ok,
        FinalReviewStatus::Failed => TraceSpanStatus::Error,
        FinalReviewStatus::Cancelled => TraceSpanStatus::Cancelled,
    }
}

fn provider_span_status(status: ProviderAttemptStatus) -> TraceSpanStatus {
    match status {
        ProviderAttemptStatus::Ok => TraceSpanStatus::Ok,
        ProviderAttemptStatus::Error => TraceSpanStatus::Error,
        ProviderAttemptStatus::Cancelled => TraceSpanStatus::Cancelled,
    }
}

fn provider_projection(
    occurrence: &ProviderAttemptOccurrence,
    terminal: bool,
) -> TraceSpanProjection {
    TraceSpanProjection {
        provider_name: non_empty(occurrence.provider_name.clone()),
        model_name: non_empty(occurrence.model_name.clone()),
        protocol: Some(provider_protocol(occurrence.actual_api_protocol)),
        operation_phase: Some(provider_operation_phase(occurrence.operation_phase)),
        attempt_index: Some(u64::from(occurrence.attempt_index)),
        retry_count: terminal.then_some(u64::from(occurrence.retry_scheduled)),
        queue_duration_ms: terminal.then_some(occurrence.queue_duration_ms).flatten(),
        request_send_to_headers_ms: terminal
            .then_some(occurrence.request_send_to_headers_ms)
            .flatten(),
        retry_backoff_ms: terminal.then_some(occurrence.retry_backoff_ms).flatten(),
        usage: terminal
            .then(|| occurrence.usage.as_ref().map(trace_usage))
            .flatten(),
        error: terminal
            .then(|| {
                (occurrence.terminal_status != ProviderAttemptStatus::Ok).then(|| {
                    TraceErrorProjection {
                        category: occurrence
                            .error_category
                            .as_ref()
                            .map(error_category)
                            .unwrap_or(TraceErrorCategory::UnknownProviderError),
                        stage: occurrence.error_stage.as_ref().map(error_stage),
                        code: occurrence.diagnostic_code.clone().and_then(stable_code),
                    }
                })
            })
            .flatten(),
        ..TraceSpanProjection::default()
    }
}

fn provider_protocol(value: ProviderApiProtocol) -> TraceProviderProtocol {
    match value {
        ProviderApiProtocol::Declared => TraceProviderProtocol::Declared,
        ProviderApiProtocol::OpenAiResponses => TraceProviderProtocol::OpenAiResponses,
        ProviderApiProtocol::OpenAiChatCompletions => TraceProviderProtocol::OpenAiChatCompletions,
    }
}

fn provider_operation_phase(value: ProviderAttemptOperationPhase) -> TraceProviderOperationPhase {
    match value {
        ProviderAttemptOperationPhase::CapabilityProbe => {
            TraceProviderOperationPhase::CapabilityProbe
        }
        ProviderAttemptOperationPhase::Completion => TraceProviderOperationPhase::Completion,
    }
}

fn provider_phase_key(value: ProviderAttemptOperationPhase) -> &'static str {
    match value {
        ProviderAttemptOperationPhase::CapabilityProbe => "capability_probe",
        ProviderAttemptOperationPhase::Completion => "completion",
    }
}

fn provider_protocol_key(value: ProviderApiProtocol) -> &'static str {
    match value {
        ProviderApiProtocol::Declared => "declared",
        ProviderApiProtocol::OpenAiResponses => "openai_responses",
        ProviderApiProtocol::OpenAiChatCompletions => "openai_chat_completions",
    }
}

fn trace_usage(value: &singularity_model::ModelUsage) -> TraceUsage {
    TraceUsage {
        input_tokens: value.input_tokens,
        output_tokens: value.output_tokens,
        total_tokens: value.total_tokens,
        cached_input_tokens: value.cached_input_tokens,
        reasoning_tokens: value.reasoning_tokens,
    }
}

fn error_category(value: &ModelErrorCategory) -> TraceErrorCategory {
    match value {
        ModelErrorCategory::Cancelled => TraceErrorCategory::Cancelled,
        ModelErrorCategory::Authentication => TraceErrorCategory::Authentication,
        ModelErrorCategory::Network => TraceErrorCategory::Network,
        ModelErrorCategory::ModelConfiguration => TraceErrorCategory::ModelConfiguration,
        ModelErrorCategory::InvalidRequest => TraceErrorCategory::InvalidRequest,
        ModelErrorCategory::ContextLengthExceeded => TraceErrorCategory::ContextLengthExceeded,
        ModelErrorCategory::BudgetExceeded => TraceErrorCategory::BudgetExceeded,
        ModelErrorCategory::ToolCallParse => TraceErrorCategory::ToolCallParse,
        ModelErrorCategory::JsonSchema => TraceErrorCategory::JsonSchema,
        ModelErrorCategory::ContentFilter => TraceErrorCategory::ContentFilter,
        ModelErrorCategory::UnsupportedCapability => TraceErrorCategory::UnsupportedCapability,
        ModelErrorCategory::ProviderUnavailable => TraceErrorCategory::ProviderUnavailable,
        ModelErrorCategory::UnknownProviderError => TraceErrorCategory::UnknownProviderError,
    }
}

fn error_stage(value: &singularity_model::ProviderErrorStage) -> TraceErrorStage {
    match value {
        singularity_model::ProviderErrorStage::ClientInitialization => {
            TraceErrorStage::ClientInitialization
        }
        singularity_model::ProviderErrorStage::RequestSend => TraceErrorStage::RequestSend,
        singularity_model::ProviderErrorStage::ResponseStatus => TraceErrorStage::ResponseStatus,
        singularity_model::ProviderErrorStage::ResponseBodyRead => {
            TraceErrorStage::ResponseBodyRead
        }
        singularity_model::ProviderErrorStage::ResponseJsonDecode => {
            TraceErrorStage::ResponseJsonDecode
        }
        singularity_model::ProviderErrorStage::ResponseValidation => {
            TraceErrorStage::ResponseValidation
        }
        singularity_model::ProviderErrorStage::Cancelled => TraceErrorStage::Cancelled,
    }
}

fn stable_code(value: String) -> Option<String> {
    (value.len() <= 64
        && !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        }))
    .then_some(value)
}

// The Agent observation does not expose a policy cause on the identity itself; only the
// finished observation carries it. Keep this helper separate so the projector never infers it.
trait PolicyDecisionCauseProjection {
    fn lifecycle_cause(&self) -> Option<TracePolicyCause>;
}

impl PolicyDecisionCauseProjection for PolicyDecisionObservation {
    fn lifecycle_cause(&self) -> Option<TracePolicyCause> {
        self.cause.map(|cause| match cause {
            PolicyDecisionCause::Explicit => TracePolicyCause::Explicit,
            PolicyDecisionCause::Rule => TracePolicyCause::Rule,
            PolicyDecisionCause::FilesystemProfile => TracePolicyCause::FilesystemProfile,
            PolicyDecisionCause::NetworkProfile => TracePolicyCause::NetworkProfile,
            PolicyDecisionCause::ProtectedResource => TracePolicyCause::ProtectedResource,
            PolicyDecisionCause::NoMatchingRule => TracePolicyCause::NoMatchingRule,
            PolicyDecisionCause::ApprovalPolicy => TracePolicyCause::ApprovalPolicy,
            PolicyDecisionCause::ApprovalGrant => TracePolicyCause::ApprovalGrant,
            PolicyDecisionCause::ApprovalState => TracePolicyCause::ApprovalState,
        })
    }
}
