//! Trace and artifact persistence, redaction, integrity, and retrieval.

use super::support::*;
use super::*;

/// 注册脱敏、内容寻址产物引用的输入。
pub struct RegisterArtifactRefParams<'a> {
    /// artifact 所属 run。
    pub run_id: &'a str,
    /// 可选的来源 item。
    pub item_id: Option<&'a str>,
    /// artifact 类型。
    pub kind: &'a str,
    /// artifact URI。
    pub uri: &'a str,
    /// 内容摘要。
    pub content_digest: &'a str,
    /// 面向用户的摘要。
    pub summary: &'a str,
    /// 需要持久化并按规则脱敏的 metadata。
    pub metadata: Value,
}

impl SessionStore {
    /// 脱敏并追加一条带完整性校验的 trace event。
    pub fn append_trace(&self, event: &TraceEvent) -> StoreResult<()> {
        self.append_trace_batch(std::slice::from_ref(event))
    }

    /// Append one trace event, treating an identical event id and payload as an idempotent retry.
    pub fn append_trace_idempotent(&self, event: &TraceEvent) -> StoreResult<TraceEvent> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let sanitized = sanitize_trace_event(event);
        validate_public_trace_binding(&transaction, &sanitized)?;
        validate_trace_batch_input(std::slice::from_ref(&sanitized))?;
        validate_trace_storage_values(std::slice::from_ref(&sanitized))?;

        let existing = transaction
            .query_row(
                "select event_id, run_id, session_id, payload, span_id, parent_span_id,
                        span_kind, span_phase, span_status, duration_ms, time_to_first_token_ms,
                        span_projection, metric_samples
                 from trace_events where event_id = ?1",
                params![sanitized.event_id],
                stored_trace_row,
            )
            .optional()?;
        if let Some(row) = existing {
            let existing = decode_stored_trace_row(row)?;
            if existing == sanitized {
                transaction.commit()?;
                return Ok(existing);
            }
            if same_typed_start_identity(&existing, &sanitized) {
                transaction.commit()?;
                return Ok(existing);
            }
            return Err(StoreError::AlreadyExists(format!(
                "trace event {} has different content",
                sanitized.event_id
            )));
        }

        if sanitized.span_phase == Some(TraceSpanPhase::Start)
            && let Some(span_id) = sanitized.span_id.as_deref()
        {
            let existing = transaction
                .query_row(
                    "select event_id, run_id, session_id, payload, span_id, parent_span_id,
                            span_kind, span_phase, span_status, duration_ms, time_to_first_token_ms,
                            span_projection, metric_samples
                     from trace_events
                     where run_id = ?1 and span_id = ?2 and span_phase = 'start'",
                    params![sanitized.run_id, span_id],
                    stored_trace_row,
                )
                .optional()?;
            if let Some(row) = existing {
                let existing = decode_stored_trace_row(row)?;
                if same_typed_start_identity(&existing, &sanitized) {
                    transaction.commit()?;
                    return Ok(existing);
                }
                return Err(StoreError::AlreadyExists(format!(
                    "typed trace span {span_id} has different start identity"
                )));
            }
        }

        let mut run_ids = BTreeSet::new();
        run_ids.insert(sanitized.run_id.clone());
        let mut all_events = load_trace_events(&transaction, &run_ids)?;
        all_events.push(sanitized.clone());
        validate_trace_span_batch(&all_events)?;
        let stored = Self::insert_trace(&transaction, &sanitized)?;
        transaction.commit()?;
        Ok(stored)
    }

    /// 先整体验证、再在一个 BEGIN IMMEDIATE 中追加 trace batch。
    pub fn append_trace_batch(&self, events: &[TraceEvent]) -> StoreResult<()> {
        if events.is_empty() {
            return Err(StoreError::InvalidState(
                "trace batch must not be empty".to_string(),
            ));
        }
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        validate_public_trace_bindings(&transaction, events)?;
        validate_trace_batch_input(events)?;
        let sanitized = events.iter().map(sanitize_trace_event).collect::<Vec<_>>();
        validate_trace_storage_values(&sanitized)?;
        for event in &sanitized {
            let existing = transaction
                .query_row(
                    "select 1 from trace_events where event_id = ?1",
                    params![event.event_id],
                    |_| Ok(()),
                )
                .optional()?;
            if existing.is_some() {
                return Err(StoreError::AlreadyExists(format!(
                    "trace event {}",
                    event.event_id
                )));
            }
        }

        let run_ids = sanitized
            .iter()
            .map(|event| event.run_id.clone())
            .collect::<BTreeSet<_>>();
        let mut all_events = load_trace_events(&transaction, &run_ids)?;
        all_events.extend(sanitized.iter().cloned());
        validate_trace_span_batch(&all_events)?;

        for event in events {
            let _ = Self::insert_trace(&transaction, event)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// 读取 run 的全部 trace，并校验每条事件完整性。
    pub fn list_trace(&self, run_id: &str) -> StoreResult<Vec<TraceEvent>> {
        self.list_trace_page(run_id, None, None)
    }

    /// 按 rowid 游标分页读取 run trace。
    pub fn list_trace_page(
        &self,
        run_id: &str,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> StoreResult<Vec<TraceEvent>> {
        let limit = limit.unwrap_or(usize::MAX);
        let offset = offset.unwrap_or(0);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let offset = i64::try_from(offset).unwrap_or(i64::MAX);
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let mut statement = transaction.prepare(
            "select event_id, run_id, session_id, payload, span_id, parent_span_id,
                    span_kind, span_phase, span_status, duration_ms, time_to_first_token_ms,
                    span_projection, metric_samples
             from trace_events where run_id = ?1 order by rowid limit ?2 offset ?3",
        )?;
        let rows = statement.query_map(params![run_id, limit, offset], stored_trace_row)?;
        let raw_events = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut events = Vec::new();
        for row in raw_events {
            let event = decode_stored_trace_row(row)?;
            events.push(event);
        }
        validate_public_trace_bindings(&transaction, &events)?;
        if events.is_empty() {
            if Self::exists_in_transaction(
                &transaction,
                "select 1 from trace_events where run_id = ?1",
                run_id,
            )? {
                transaction.commit()?;
                return Ok(events);
            }
            return Err(StoreError::NotFound(format!("trace run {run_id}")));
        }
        transaction.commit()?;
        Ok(events)
    }

    /// 读取 run trace 的有界最新窗口并恢复时间顺序。
    pub fn tail_trace(
        &self,
        run_id: &str,
        limit: usize,
        offset: Option<usize>,
    ) -> StoreResult<Vec<TraceEvent>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let offset = i64::try_from(offset.unwrap_or(0)).unwrap_or(i64::MAX);
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let mut statement = transaction.prepare(
            "select event_id, run_id, session_id, payload, span_id, parent_span_id,
                    span_kind, span_phase, span_status, duration_ms, time_to_first_token_ms,
                    span_projection, metric_samples
             from trace_events where run_id = ?1 order by rowid desc limit ?2 offset ?3",
        )?;
        let rows = statement.query_map(params![run_id, limit, offset], stored_trace_row)?;
        let raw_events = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut events = Vec::new();
        for row in raw_events {
            let event = decode_stored_trace_row(row)?;
            events.push(event);
        }
        validate_public_trace_bindings(&transaction, &events)?;
        if events.is_empty()
            && !Self::exists_in_transaction(
                &transaction,
                "select 1 from trace_events where run_id = ?1",
                run_id,
            )?
        {
            return Err(StoreError::NotFound(format!("trace run {run_id}")));
        }
        events.reverse();
        transaction.commit()?;
        Ok(events)
    }

    /// 读取单条 trace event 并校验完整性。
    pub fn show_trace(&self, event_id: &str) -> StoreResult<TraceEvent> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let row = transaction
            .query_row(
                "select event_id, run_id, session_id, payload, span_id, parent_span_id,
                        span_kind, span_phase, span_status, duration_ms, time_to_first_token_ms,
                        span_projection, metric_samples
                 from trace_events where event_id = ?1",
                params![event_id],
                stored_trace_row,
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("trace event {event_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        let event = decode_stored_trace_row(row)?;
        validate_public_trace_binding(&transaction, &event)?;
        transaction.commit()?;
        Ok(event)
    }

    /// 从唯一 trace_events 表派生固定 metric 集合。
    pub fn trace_metrics(&self, run_id: &str) -> StoreResult<TraceMetrics> {
        let events = self.list_trace(run_id)?;
        validate_trace_span_batch(&events)?;
        derive_trace_metrics(run_id, &events)
    }

    /// 直接按 run/session/kind 查找已持久化的 typed span start，不扫描整条 trace。
    pub fn find_span_start(
        &self,
        run_id: &str,
        session_id: &str,
        span_kind: TraceSpanKind,
    ) -> StoreResult<Option<TraceEvent>> {
        find_trace_span_start(&self.connection, run_id, session_id, span_kind)
    }
}

fn same_typed_start_identity(existing: &TraceEvent, incoming: &TraceEvent) -> bool {
    let Some(kind) = existing.span_kind else {
        return false;
    };
    let existing_projection = existing.span_projection.clone().unwrap_or_default();
    let incoming_projection = incoming.span_projection.clone().unwrap_or_default();
    existing.span_phase == Some(TraceSpanPhase::Start)
        && incoming.span_phase == Some(TraceSpanPhase::Start)
        && existing.run_id == incoming.run_id
        && existing.session_id == incoming.session_id
        && existing.span_id == incoming.span_id
        && existing.parent_span_id == incoming.parent_span_id
        && incoming.span_kind == Some(kind)
        && existing_projection.same_identity_attributes(kind, &incoming_projection)
}

/// Read one persisted typed span start without creating a second trace source.
pub(crate) fn find_trace_span_start(
    connection: &Connection,
    run_id: &str,
    session_id: &str,
    span_kind: TraceSpanKind,
) -> StoreResult<Option<TraceEvent>> {
    let row = connection
        .query_row(
            "select event_id, run_id, session_id, payload, span_id, parent_span_id,
                    span_kind, span_phase, span_status, duration_ms, time_to_first_token_ms,
                    span_projection, metric_samples
             from trace_events
             where run_id = ?1 and session_id = ?2 and span_kind = ?3 and span_phase = 'start'",
            params![run_id, session_id, span_kind.as_storage_text()],
            stored_trace_row,
        )
        .optional()?;
    row.map(decode_stored_trace_row).transpose()
}

/// Read one persisted typed span start by its exact stable span identity.
pub(crate) fn find_trace_span_start_by_id(
    connection: &Connection,
    run_id: &str,
    session_id: &str,
    span_kind: TraceSpanKind,
    span_id: &str,
) -> StoreResult<Option<TraceEvent>> {
    let row = connection
        .query_row(
            "select event_id, run_id, session_id, payload, span_id, parent_span_id,
                    span_kind, span_phase, span_status, duration_ms, time_to_first_token_ms,
                    span_projection, metric_samples
             from trace_events
             where run_id = ?1 and session_id = ?2 and span_kind = ?3
               and span_phase = 'start' and span_id = ?4",
            params![run_id, session_id, span_kind.as_storage_text(), span_id],
            stored_trace_row,
        )
        .optional()?;
    row.map(decode_stored_trace_row).transpose()
}

fn validate_trace_batch_input(events: &[TraceEvent]) -> StoreResult<()> {
    let mut event_ids = BTreeSet::new();
    for event in events {
        if event.event_id.trim().is_empty() || !event_ids.insert(event.event_id.as_str()) {
            return Err(StoreError::InvalidState(
                "trace batch contains an empty or duplicate event_id".to_string(),
            ));
        }
        if event.run_id.trim().is_empty() {
            return Err(StoreError::InvalidState(
                "trace run_id must not be empty".to_string(),
            ));
        }
        event
            .validate_span_lifecycle()
            .map_err(|error| StoreError::InvalidState(format!("trace span is invalid: {error}")))?;
    }
    Ok(())
}

fn validate_trace_storage_values(events: &[TraceEvent]) -> StoreResult<()> {
    for event in events {
        validate_trace_u64(event.duration_ms, "trace duration")?;
        validate_trace_u64(event.time_to_first_token_ms, "trace time to first token")?;
        for sample in &event.metric_samples {
            validate_trace_u64(Some(sample.count), "trace metric sample count")?;
        }
        if let Some(projection) = &event.span_projection {
            validate_projection_storage_values(projection)?;
        }
    }
    Ok(())
}

fn validate_trace_u64(value: Option<u64>, label: &str) -> StoreResult<()> {
    if let Some(value) = value {
        i64::try_from(value).map_err(|_| {
            StoreError::InvalidState(format!("{label} exceeds sqlite integer range"))
        })?;
    }
    Ok(())
}

fn validate_projection_storage_values(projection: &TraceSpanProjection) -> StoreResult<()> {
    for (value, label) in [
        (projection.operation_count, "trace operation count"),
        (projection.message_count, "trace message count"),
        (projection.tool_count, "trace tool count"),
        (projection.request_token_count, "trace request token count"),
        (projection.model_turn_ordinal, "trace model turn ordinal"),
        (projection.attempt_index, "trace attempt index"),
        (projection.retry_count, "trace retry count"),
        (projection.queue_duration_ms, "trace queue duration"),
        (
            projection.request_send_to_headers_ms,
            "trace send to headers duration",
        ),
        (projection.retry_backoff_ms, "trace retry backoff"),
    ] {
        validate_trace_u64(value, label)?;
    }
    if let Some(usage) = projection.usage {
        for (value, label) in [
            (usage.input_tokens, "trace input tokens"),
            (usage.output_tokens, "trace output tokens"),
            (usage.total_tokens, "trace total tokens"),
            (usage.cached_input_tokens, "trace cached input tokens"),
            (usage.reasoning_tokens, "trace reasoning tokens"),
        ] {
            validate_trace_u64(Some(value), label)?;
        }
    }
    if let Some(tool) = &projection.tool {
        validate_trace_u64(tool.tool_call_ordinal, "trace tool call ordinal")?;
    }
    if let Some(policy) = &projection.policy {
        validate_trace_u64(policy.operation_count, "trace policy operation count")?;
        validate_trace_u64(policy.resource_count, "trace policy resource count")?;
    }
    if let Some(approval) = &projection.approval {
        validate_trace_u64(approval.request_count, "trace approval request count")?;
    }
    if let Some(verification) = &projection.verification {
        validate_trace_u64(verification.revision, "trace verification revision")?;
        validate_trace_u64(
            verification.required_command_count,
            "trace required command count",
        )?;
        validate_trace_u64(
            verification.satisfied_command_count,
            "trace satisfied command count",
        )?;
        validate_trace_u64(verification.occurrence_count, "trace occurrence count")?;
        validate_trace_u64(verification.command_duration_ms, "trace command duration")?;
        validate_trace_u64(verification.attempt, "trace repair attempt")?;
        validate_trace_u64(verification.max_attempts, "trace repair max attempts")?;
        validate_trace_u64(
            verification.required_revision,
            "trace repair required revision",
        )?;
    }
    if let Some(review) = &projection.final_review {
        validate_trace_u64(review.model_turn_ordinal, "trace model turn ordinal")?;
    }
    Ok(())
}

fn load_trace_events(
    connection: &Connection,
    run_ids: &BTreeSet<String>,
) -> StoreResult<Vec<TraceEvent>> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat_n("?", run_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!(
        "select event_id, run_id, session_id, payload, span_id, parent_span_id, \
                span_kind, span_phase, span_status, duration_ms, time_to_first_token_ms, \
                span_projection, metric_samples \
         from trace_events where run_id in ({placeholders}) order by rowid"
    );
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map(params_from_iter(run_ids.iter()), stored_trace_row)?;
    let raw_rows = rows.collect::<Result<Vec<_>, _>>()?;
    raw_rows.into_iter().map(decode_stored_trace_row).collect()
}

const TRACE_METRIC_NAMES: &[TraceMetricName] = &[
    TraceMetricName::TaskDurationMs,
    TraceMetricName::TurnDurationMs,
    TraceMetricName::ProviderAttemptDurationMs,
    TraceMetricName::ProviderSendToHeadersMs,
    TraceMetricName::ProviderTimeToFirstTokenMs,
    TraceMetricName::ProviderQueueDurationMs,
    TraceMetricName::ProviderRetryCount,
    TraceMetricName::ProviderRetryBackoffMs,
    TraceMetricName::ProviderErrorCount,
    TraceMetricName::ProviderInputTokens,
    TraceMetricName::ProviderCachedInputTokens,
    TraceMetricName::ProviderOutputTokens,
    TraceMetricName::ProviderTotalTokens,
    TraceMetricName::ProviderCapabilityCacheHitCount,
    TraceMetricName::ProviderCapabilityCacheMissCount,
    TraceMetricName::ProviderCapabilityCacheHitRateBps,
    TraceMetricName::ToolFrequency,
    TraceMetricName::ToolSuccessCount,
    TraceMetricName::ToolSuccessRateBps,
    TraceMetricName::ToolDurationMs,
    TraceMetricName::ApprovalWaitDurationMs,
    TraceMetricName::SandboxExecutionDurationMs,
    TraceMetricName::VerificationDurationMs,
    TraceMetricName::FinalReviewDurationMs,
    TraceMetricName::CompletionRejectionCount,
    TraceMetricName::CompletionRepairCount,
    TraceMetricName::EventQueueDropCount,
    TraceMetricName::EventGapCount,
    TraceMetricName::WriterVisibleCount,
];

fn derive_trace_metrics(run_id: &str, events: &[TraceEvent]) -> StoreResult<TraceMetrics> {
    let legacy_only = events.iter().all(|event| {
        event.span_id.is_none()
            && event.span_phase.is_none()
            && event.span_projection.is_none()
            && event.metric_samples.is_empty()
    });
    let metrics = TRACE_METRIC_NAMES
        .iter()
        .copied()
        .map(|name| derive_trace_metric(name, events, legacy_only))
        .collect::<StoreResult<Vec<_>>>()?;
    Ok(TraceMetrics {
        run_id: run_id.to_string(),
        metrics,
    })
}

fn derive_trace_metric(
    name: TraceMetricName,
    events: &[TraceEvent],
    legacy_only: bool,
) -> StoreResult<TraceMetric> {
    let (availability, values) = match name {
        TraceMetricName::TaskDurationMs => duration_values(
            TraceSpanKind::Task,
            events,
            legacy_only,
            TraceMetricUnavailableReason::MissingTrueTiming,
        ),
        TraceMetricName::TurnDurationMs => duration_values(
            TraceSpanKind::Turn,
            events,
            legacy_only,
            TraceMetricUnavailableReason::MissingTrueTiming,
        ),
        TraceMetricName::ProviderAttemptDurationMs => duration_values(
            TraceSpanKind::ProviderAttempt,
            events,
            legacy_only,
            TraceMetricUnavailableReason::MissingTrueTiming,
        ),
        TraceMetricName::ProviderSendToHeadersMs => projection_values(
            TraceSpanKind::ProviderAttempt,
            events,
            legacy_only,
            |projection| projection.request_send_to_headers_ms,
            TraceMetricUnavailableReason::MissingTrueTiming,
        ),
        TraceMetricName::ProviderTimeToFirstTokenMs => ttft_values(events, legacy_only),
        TraceMetricName::ProviderQueueDurationMs => projection_values(
            TraceSpanKind::ProviderAttempt,
            events,
            legacy_only,
            |projection| projection.queue_duration_ms,
            TraceMetricUnavailableReason::MissingTrueTiming,
        ),
        TraceMetricName::ProviderRetryCount => projection_values(
            TraceSpanKind::ProviderAttempt,
            events,
            legacy_only,
            |projection| projection.retry_count,
            TraceMetricUnavailableReason::MissingMetricValue,
        ),
        TraceMetricName::ProviderRetryBackoffMs => projection_values(
            TraceSpanKind::ProviderAttempt,
            events,
            legacy_only,
            |projection| projection.retry_backoff_ms,
            TraceMetricUnavailableReason::MissingMetricValue,
        ),
        TraceMetricName::ProviderErrorCount => provider_error_values(events, legacy_only)?,
        TraceMetricName::ProviderInputTokens => {
            usage_values(events, legacy_only, |usage| usage.input_tokens)
        }
        TraceMetricName::ProviderCachedInputTokens => {
            usage_values(events, legacy_only, |usage| usage.cached_input_tokens)
        }
        TraceMetricName::ProviderOutputTokens => {
            usage_values(events, legacy_only, |usage| usage.output_tokens)
        }
        TraceMetricName::ProviderTotalTokens => {
            usage_values(events, legacy_only, |usage| usage.total_tokens)
        }
        TraceMetricName::ProviderCapabilityCacheHitCount => capability_cache_values(
            TraceMetricSampleKind::ProviderCapabilityCacheHit,
            TraceMetricSampleKind::ProviderCapabilityCacheMiss,
            events,
            legacy_only,
        ),
        TraceMetricName::ProviderCapabilityCacheMissCount => capability_cache_values(
            TraceMetricSampleKind::ProviderCapabilityCacheMiss,
            TraceMetricSampleKind::ProviderCapabilityCacheHit,
            events,
            legacy_only,
        ),
        TraceMetricName::ProviderCapabilityCacheHitRateBps => sample_ratio_values(
            TraceMetricSampleKind::ProviderCapabilityCacheHit,
            TraceMetricSampleKind::ProviderCapabilityCacheMiss,
            events,
            legacy_only,
        )?,
        TraceMetricName::ToolFrequency => {
            count_values(TraceSpanKind::ToolCall, events, legacy_only)
        }
        TraceMetricName::ToolSuccessCount => tool_success_values(events, legacy_only)?,
        TraceMetricName::ToolSuccessRateBps => tool_success_rate_values(events, legacy_only)?,
        TraceMetricName::ToolDurationMs => duration_values(
            TraceSpanKind::ToolCall,
            events,
            legacy_only,
            TraceMetricUnavailableReason::MissingTrueTiming,
        ),
        TraceMetricName::ApprovalWaitDurationMs => duration_values(
            TraceSpanKind::ApprovalWait,
            events,
            legacy_only,
            TraceMetricUnavailableReason::MissingTrueTiming,
        ),
        TraceMetricName::SandboxExecutionDurationMs => duration_values(
            TraceSpanKind::SandboxExecution,
            events,
            legacy_only,
            TraceMetricUnavailableReason::MissingTrueTiming,
        ),
        TraceMetricName::VerificationDurationMs => duration_values(
            TraceSpanKind::Verification,
            events,
            legacy_only,
            TraceMetricUnavailableReason::MissingTrueTiming,
        ),
        TraceMetricName::FinalReviewDurationMs => duration_values(
            TraceSpanKind::FinalReview,
            events,
            legacy_only,
            TraceMetricUnavailableReason::MissingTrueTiming,
        ),
        TraceMetricName::CompletionRejectionCount => sample_values(
            TraceMetricSampleKind::CompletionRejection,
            events,
            legacy_only,
        ),
        TraceMetricName::CompletionRepairCount => {
            sample_values(TraceMetricSampleKind::CompletionRepair, events, legacy_only)
        }
        TraceMetricName::EventQueueDropCount => {
            sample_values(TraceMetricSampleKind::EventQueueDrop, events, legacy_only)
        }
        TraceMetricName::EventGapCount => {
            sample_values(TraceMetricSampleKind::EventGap, events, legacy_only)
        }
        TraceMetricName::WriterVisibleCount => {
            sample_values(TraceMetricSampleKind::WriterVisible, events, legacy_only)
        }
    };
    let distribution = distribution(values)?;
    if matches!(availability, TraceMetricAvailability::Available) && distribution.is_none() {
        return Err(StoreError::InvalidState(format!(
            "available trace metric {} is missing a value",
            name.as_storage_text()
        )));
    }
    Ok(TraceMetric {
        name,
        availability,
        distribution,
    })
}

/// Only expose terminal samples from an identity-complete span population.
///
/// A duration or end-derived metric must not report a partial distribution while another
/// occurrence of the same span kind is still open (or has no matching start). Pairing by
/// `span_id` also catches equal start/end counts with different identities.
fn span_ends(
    kind: TraceSpanKind,
    events: &[TraceEvent],
) -> Result<Vec<&TraceEvent>, TraceMetricUnavailableReason> {
    let mut starts = BTreeSet::new();
    let mut ends = BTreeSet::new();
    let mut end_events = Vec::new();
    let mut saw_kind = false;
    let mut malformed = false;

    for event in events.iter().filter(|event| event.span_kind == Some(kind)) {
        saw_kind = true;
        let Some(span_id) = event.span_id.as_deref() else {
            malformed = true;
            continue;
        };
        match event.span_phase {
            Some(TraceSpanPhase::Start) => {
                if !starts.insert(span_id) {
                    malformed = true;
                }
            }
            Some(TraceSpanPhase::End) => {
                if !ends.insert(span_id) {
                    malformed = true;
                }
                end_events.push(event);
            }
            None => malformed = true,
        }
    }

    if !saw_kind {
        return Err(TraceMetricUnavailableReason::NoProducer);
    }
    if malformed || starts != ends {
        return Err(TraceMetricUnavailableReason::IncompleteStartEnd);
    }
    Ok(end_events)
}

fn duration_values(
    kind: TraceSpanKind,
    events: &[TraceEvent],
    legacy_only: bool,
    missing_reason: TraceMetricUnavailableReason,
) -> (TraceMetricAvailability, Vec<u64>) {
    if legacy_only {
        return (legacy_only_availability(), Vec::new());
    }
    let ends = match span_ends(kind, events) {
        Ok(ends) => ends,
        Err(reason) => return (unavailable(reason), Vec::new()),
    };
    let values = ends
        .iter()
        .filter_map(|event| event.duration_ms)
        .collect::<Vec<_>>();
    if values.is_empty() {
        return (unavailable(missing_reason), values);
    }
    (TraceMetricAvailability::Available, values)
}

fn count_values(
    kind: TraceSpanKind,
    events: &[TraceEvent],
    legacy_only: bool,
) -> (TraceMetricAvailability, Vec<u64>) {
    if legacy_only {
        return (legacy_only_availability(), Vec::new());
    }
    let ends = match span_ends(kind, events) {
        Ok(ends) => ends,
        Err(reason) => return (unavailable(reason), Vec::new()),
    };
    (
        TraceMetricAvailability::Available,
        std::iter::repeat_n(1, ends.len()).collect(),
    )
}

fn projection_values<F>(
    kind: TraceSpanKind,
    events: &[TraceEvent],
    legacy_only: bool,
    select: F,
    missing_reason: TraceMetricUnavailableReason,
) -> (TraceMetricAvailability, Vec<u64>)
where
    F: Fn(&TraceSpanProjection) -> Option<u64>,
{
    if legacy_only {
        return (legacy_only_availability(), Vec::new());
    }
    let ends = match span_ends(kind, events) {
        Ok(ends) => ends,
        Err(reason) => return (unavailable(reason), Vec::new()),
    };
    let values = ends
        .iter()
        .filter_map(|event| event.span_projection.as_ref().and_then(&select))
        .collect::<Vec<_>>();
    if values.is_empty() {
        return (unavailable(missing_reason), values);
    }
    (TraceMetricAvailability::Available, values)
}

fn ttft_values(events: &[TraceEvent], legacy_only: bool) -> (TraceMetricAvailability, Vec<u64>) {
    if legacy_only {
        return (legacy_only_availability(), Vec::new());
    }
    let ends = match span_ends(TraceSpanKind::ProviderAttempt, events) {
        Ok(ends) => ends,
        Err(reason) => return (unavailable(reason), Vec::new()),
    };
    let values = ends
        .iter()
        .filter_map(|event| event.time_to_first_token_ms)
        .collect::<Vec<_>>();
    if values.len() == ends.len() {
        return (TraceMetricAvailability::Available, values);
    }
    if !values.is_empty() {
        return (TraceMetricAvailability::Available, values);
    }
    let protocols = ends
        .iter()
        .filter_map(|event| {
            event
                .span_projection
                .as_ref()
                .and_then(|value| value.protocol)
        })
        .collect::<Vec<_>>();
    if protocols.len() == ends.len()
        && protocols
            .iter()
            .all(|protocol| *protocol != TraceProviderProtocol::OpenAiResponses)
    {
        (
            unavailable(TraceMetricUnavailableReason::NonStreamingTtft),
            values,
        )
    } else {
        (
            unavailable(TraceMetricUnavailableReason::MissingTrueTiming),
            values,
        )
    }
}

fn usage_values<F>(
    events: &[TraceEvent],
    legacy_only: bool,
    select: F,
) -> (TraceMetricAvailability, Vec<u64>)
where
    F: Fn(&TraceUsage) -> u64,
{
    if legacy_only {
        return (legacy_only_availability(), Vec::new());
    }
    let ends = match span_ends(TraceSpanKind::ProviderAttempt, events) {
        Ok(ends) => ends,
        Err(reason) => return (unavailable(reason), Vec::new()),
    };
    let values = ends
        .iter()
        .filter_map(|event| {
            event
                .span_projection
                .as_ref()
                .and_then(|projection| projection.usage.as_ref())
                .map(&select)
        })
        .collect::<Vec<_>>();
    if values.is_empty() {
        return (
            unavailable(TraceMetricUnavailableReason::MissingUsage),
            values,
        );
    }
    (TraceMetricAvailability::Available, values)
}

fn provider_error_values(
    events: &[TraceEvent],
    legacy_only: bool,
) -> StoreResult<(TraceMetricAvailability, Vec<u64>)> {
    if legacy_only {
        return Ok((legacy_only_availability(), Vec::new()));
    }
    let ends = match span_ends(TraceSpanKind::ProviderAttempt, events) {
        Ok(ends) => ends,
        Err(reason) => return Ok((unavailable(reason), Vec::new())),
    };
    let error_count = ends
        .iter()
        .filter(|event| {
            event.span_status == Some(TraceSpanStatus::Error)
                || event
                    .span_projection
                    .as_ref()
                    .is_some_and(|projection| projection.error.is_some())
        })
        .count();
    let error_count = u64::try_from(error_count)
        .map_err(|_| StoreError::InvalidState("trace provider error count overflow".to_string()))?;
    Ok((TraceMetricAvailability::Available, vec![error_count]))
}

fn tool_success_values(
    events: &[TraceEvent],
    legacy_only: bool,
) -> StoreResult<(TraceMetricAvailability, Vec<u64>)> {
    let (availability, outcomes) = tool_outcomes(events, legacy_only)?;
    if !matches!(availability, TraceMetricAvailability::Available) {
        return Ok((availability, Vec::new()));
    }
    let successes = outcomes.iter().filter(|success| **success).count();
    let successes = u64::try_from(successes)
        .map_err(|_| StoreError::InvalidState("trace tool success count overflow".to_string()))?;
    Ok((TraceMetricAvailability::Available, vec![successes]))
}

fn tool_outcomes(
    events: &[TraceEvent],
    legacy_only: bool,
) -> StoreResult<(TraceMetricAvailability, Vec<bool>)> {
    if legacy_only {
        return Ok((legacy_only_availability(), Vec::new()));
    }
    let ends = match span_ends(TraceSpanKind::ToolCall, events) {
        Ok(ends) => ends,
        Err(reason) => return Ok((unavailable(reason), Vec::new())),
    };
    let mut outcomes = Vec::with_capacity(ends.len());
    for event in ends {
        let generic_status = event.span_status.ok_or_else(|| {
            StoreError::InvalidState("completed tool span is missing generic status".to_string())
        })?;
        let tool_status = event
            .span_projection
            .as_ref()
            .and_then(|projection| projection.tool.as_ref())
            .and_then(|tool| tool.status)
            .ok_or_else(|| {
                StoreError::InvalidState("completed tool span is missing typed status".to_string())
            })?;
        let expected_generic_status = match tool_status {
            TraceToolStatus::Succeeded => TraceSpanStatus::Ok,
            TraceToolStatus::Cancelled => TraceSpanStatus::Cancelled,
            TraceToolStatus::Failed
            | TraceToolStatus::Rejected
            | TraceToolStatus::PolicyDenied
            | TraceToolStatus::ApprovalRequired
            | TraceToolStatus::BatchRejected => TraceSpanStatus::Error,
        };
        if generic_status != expected_generic_status {
            return Err(StoreError::InvalidState(
                "tool terminal statuses are inconsistent".to_string(),
            ));
        }
        outcomes.push(tool_status == TraceToolStatus::Succeeded);
    }
    Ok((TraceMetricAvailability::Available, outcomes))
}

fn tool_success_rate_values(
    events: &[TraceEvent],
    legacy_only: bool,
) -> StoreResult<(TraceMetricAvailability, Vec<u64>)> {
    let (availability, outcomes) = tool_outcomes(events, legacy_only)?;
    if !matches!(availability, TraceMetricAvailability::Available) {
        return Ok((availability, Vec::new()));
    }
    let successes = outcomes.iter().filter(|success| **success).count();
    let successes = u128::try_from(successes)
        .map_err(|_| StoreError::InvalidState("trace tool success count overflow".to_string()))?;
    let total = u128::try_from(outcomes.len())
        .map_err(|_| StoreError::InvalidState("trace tool span count overflow".to_string()))?;
    let rate = successes
        .checked_mul(10_000)
        .ok_or_else(|| StoreError::InvalidState("trace metric rate overflow".to_string()))?
        / total;
    let rate = u64::try_from(rate)
        .map_err(|_| StoreError::InvalidState("trace metric rate overflow".to_string()))?;
    Ok((TraceMetricAvailability::Available, vec![rate]))
}

fn sample_values(
    kind: TraceMetricSampleKind,
    events: &[TraceEvent],
    legacy_only: bool,
) -> (TraceMetricAvailability, Vec<u64>) {
    if legacy_only {
        return (legacy_only_availability(), Vec::new());
    }
    let values = events
        .iter()
        .flat_map(|event| event.metric_samples.iter())
        .filter(|sample| sample.kind == kind)
        .map(|sample| sample.count)
        .collect::<Vec<_>>();
    if values.is_empty() {
        (
            unavailable(TraceMetricUnavailableReason::NoProducer),
            values,
        )
    } else {
        (TraceMetricAvailability::Available, values)
    }
}

fn capability_cache_values(
    kind: TraceMetricSampleKind,
    counterpart: TraceMetricSampleKind,
    events: &[TraceEvent],
    legacy_only: bool,
) -> (TraceMetricAvailability, Vec<u64>) {
    if legacy_only {
        return (legacy_only_availability(), Vec::new());
    }
    let mut found_any = false;
    let values = events
        .iter()
        .flat_map(|event| event.metric_samples.iter())
        .filter_map(|sample| {
            if sample.kind == kind {
                found_any = true;
                Some(sample.count)
            } else if sample.kind == counterpart {
                found_any = true;
                None
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    if !found_any {
        (
            unavailable(TraceMetricUnavailableReason::NoProducer),
            values,
        )
    } else if values.is_empty() {
        (TraceMetricAvailability::Available, vec![0])
    } else {
        (TraceMetricAvailability::Available, values)
    }
}

fn sample_ratio_values(
    numerator_kind: TraceMetricSampleKind,
    denominator_kind: TraceMetricSampleKind,
    events: &[TraceEvent],
    legacy_only: bool,
) -> StoreResult<(TraceMetricAvailability, Vec<u64>)> {
    if legacy_only {
        return Ok((legacy_only_availability(), Vec::new()));
    }
    let mut numerator = 0_u128;
    let mut denominator = 0_u128;
    let mut found = false;
    for sample in events.iter().flat_map(|event| event.metric_samples.iter()) {
        if sample.kind == numerator_kind || sample.kind == denominator_kind {
            found = true;
            denominator = denominator
                .checked_add(u128::from(sample.count))
                .ok_or_else(|| {
                    StoreError::InvalidState("trace metric sample sum overflow".to_string())
                })?;
            if sample.kind == numerator_kind {
                numerator = numerator
                    .checked_add(u128::from(sample.count))
                    .ok_or_else(|| {
                        StoreError::InvalidState("trace metric sample sum overflow".to_string())
                    })?;
            }
        }
    }
    if !found || denominator == 0 {
        return Ok((
            unavailable(TraceMetricUnavailableReason::NoProducer),
            Vec::new(),
        ));
    }
    let rate = numerator
        .checked_mul(10_000)
        .ok_or_else(|| StoreError::InvalidState("trace metric rate overflow".to_string()))?
        / denominator;
    let rate = u64::try_from(rate)
        .map_err(|_| StoreError::InvalidState("trace metric rate overflow".to_string()))?;
    Ok((TraceMetricAvailability::Available, vec![rate]))
}

fn unavailable(reason: TraceMetricUnavailableReason) -> TraceMetricAvailability {
    TraceMetricAvailability::Unavailable { reason }
}

fn legacy_only_availability() -> TraceMetricAvailability {
    unavailable(TraceMetricUnavailableReason::LegacyOnly)
}

fn distribution(values: Vec<u64>) -> StoreResult<Option<TraceMetricDistribution>> {
    if values.is_empty() {
        return Ok(None);
    }
    let mut sorted = values;
    sorted.sort_unstable();
    let count = u64::try_from(sorted.len())
        .map_err(|_| StoreError::InvalidState("trace metric count overflow".to_string()))?;
    let sum = sorted
        .iter()
        .try_fold(0_u128, |sum, value| sum.checked_add(u128::from(*value)))
        .ok_or_else(|| StoreError::InvalidState("trace metric sum overflow".to_string()))?;
    let sum = u64::try_from(sum)
        .map_err(|_| StoreError::InvalidState("trace metric sum overflow".to_string()))?;
    let nearest_rank = |percentile: u128| {
        let rank = (u128::from(count) * percentile).div_ceil(100);
        sorted[usize::try_from(rank.saturating_sub(1)).unwrap_or(0)]
    };
    Ok(Some(TraceMetricDistribution {
        count,
        sum,
        min: sorted.first().copied(),
        max: sorted.last().copied(),
        mean: Some(sum as f64 / count as f64),
        p50: Some(nearest_rank(50)),
        p95: Some(nearest_rank(95)),
    }))
}

impl SessionStore {
    /// 脱敏并持久化一个 content-addressed artifact ref。
    pub fn register_artifact_ref(
        &self,
        params: RegisterArtifactRefParams<'_>,
    ) -> StoreResult<ArtifactRef> {
        let RegisterArtifactRefParams {
            run_id,
            item_id,
            kind,
            uri,
            content_digest,
            summary,
            metadata,
        } = params;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        validate_artifact_binding(&transaction, run_id, item_id)?;
        let content_digest =
            validate_artifact_fields(kind, uri, content_digest, summary, &metadata)?;
        let duplicate = transaction
            .query_row(
                "select artifact_id from artifact_refs
                 where run_id = ?1 and kind = ?2 and uri = ?3 and content_digest = ?4
                   and ((item_id = ?5) or (item_id is null and ?5 is null))
                 limit 1",
                params![run_id, kind, uri, content_digest, item_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(artifact_id) = duplicate {
            return Err(StoreError::AlreadyExists(format!("artifact {artifact_id}")));
        }
        let artifact = ArtifactRef {
            artifact_id: format!("artifact_{}", short_id()),
            run_id: run_id.to_string(),
            item_id: item_id.map(str::to_string),
            kind: kind.to_string(),
            uri: uri.to_string(),
            content_digest,
            summary: redact_secret_like_text(summary),
            redacted: artifact_needs_redaction(uri, summary, &metadata),
            metadata: redact_secret_like_value(metadata),
        };
        transaction.execute(
            "insert into artifact_refs(artifact_id, run_id, item_id, kind, uri, content_digest, summary, metadata, redacted) values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                artifact.artifact_id,
                artifact.run_id,
                artifact.item_id,
                artifact.kind,
                artifact.uri,
                artifact.content_digest,
                artifact.summary,
                serde_json::to_string(&artifact.metadata)?,
                artifact.redacted,
            ],
        )?;
        transaction.commit()?;
        Ok(artifact)
    }

    /// 读取指定 artifact ref。
    pub fn get_artifact_ref(&self, artifact_id: &str) -> StoreResult<ArtifactRef> {
        let artifact = self
            .connection
            .query_row(
                "select artifact_id, run_id, item_id, kind, uri, content_digest, summary, metadata, redacted from artifact_refs where artifact_id = ?1",
                params![artifact_id],
                artifact_from_row,
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("artifact {artifact_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        validate_stored_artifact(&self.connection, &artifact)?;
        Ok(artifact)
    }

    /// 列出 run 关联的 artifact refs。
    pub fn list_artifact_refs(&self, run_id: &str) -> StoreResult<Vec<ArtifactRef>> {
        validate_artifact_run(&self.connection, run_id)?;
        let mut statement = self.connection.prepare(
            "select artifact_id, run_id, item_id, kind, uri, content_digest, summary, metadata, redacted from artifact_refs where run_id = ?1 order by rowid",
        )?;
        let rows = statement.query_map(params![run_id], artifact_from_row)?;
        let artifacts = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut validated = Vec::with_capacity(artifacts.len());
        for artifact in artifacts {
            validate_stored_artifact(&self.connection, &artifact)?;
            validated.push(artifact);
        }
        Ok(validated)
    }
}

pub(crate) fn validate_turn_trace_binding(
    event: &TraceEvent,
    thread_id: &str,
    turn_id: &str,
) -> StoreResult<()> {
    event.validate_turn_binding(thread_id, turn_id)?;
    Ok(())
}

// Validate all span rows while migrating a legacy database before any source
// table is replaced. This keeps migration rejection atomic and fail closed.
pub(crate) fn validate_trace_span_batch(events: &[TraceEvent]) -> StoreResult<()> {
    let mut spans = BTreeMap::<(String, String), Vec<&TraceEvent>>::new();
    for event in events {
        event
            .validate_span_lifecycle()
            .map_err(|error| StoreError::InvalidState(format!("trace span is invalid: {error}")))?;
        if let Some(span_id) = event.span_id.as_deref() {
            spans
                .entry((event.run_id.clone(), span_id.to_string()))
                .or_default()
                .push(event);
        }
    }
    for ((run_id, span_id), events) in &spans {
        let starts = events
            .iter()
            .filter(|event| event.span_phase == Some(TraceSpanPhase::Start))
            .count();
        let ends = events
            .iter()
            .filter(|event| event.span_phase == Some(TraceSpanPhase::End))
            .count();
        if starts > 1 || ends > 1 {
            return Err(StoreError::InvalidState(format!(
                "trace span {span_id} in run {run_id} has duplicate start or end"
            )));
        }
        if ends == 1 && starts != 1 {
            return Err(StoreError::InvalidState(format!(
                "trace span {span_id} in run {run_id} ends without a start"
            )));
        }
        if let (Some(start), Some(end)) = (
            events
                .iter()
                .find(|event| event.span_phase == Some(TraceSpanPhase::Start)),
            events
                .iter()
                .find(|event| event.span_phase == Some(TraceSpanPhase::End)),
        ) && (start.parent_span_id != end.parent_span_id
            || start.span_kind != end.span_kind
            || match (&start.span_projection, &end.span_projection) {
                (Some(start_projection), Some(end_projection)) => match start.span_kind {
                    Some(kind) => !same_trace_span_identity(kind, start_projection, end_projection),
                    None => true,
                },
                (None, None) => false,
                _ => start.span_kind != Some(TraceSpanKind::PromptAssembly),
            })
        {
            return Err(StoreError::InvalidState(format!(
                "trace span {span_id} in run {run_id} has mismatched start and end identity"
            )));
        }
    }
    for event in events {
        if let Some(parent_span_id) = event.parent_span_id.as_deref()
            && !spans.contains_key(&(event.run_id.clone(), parent_span_id.to_string()))
        {
            return Err(StoreError::InvalidState(
                "trace parent_span_id must identify a span in the same run".to_string(),
            ));
        }
    }
    Ok(())
}

fn same_trace_span_identity(
    kind: TraceSpanKind,
    start: &TraceSpanProjection,
    end: &TraceSpanProjection,
) -> bool {
    if kind != TraceSpanKind::SandboxExecution {
        return start.same_identity_attributes(kind, end);
    }
    let Some(start_sandbox) = start.sandbox.as_ref() else {
        return false;
    };
    let Some(end_sandbox) = end.sandbox.as_ref() else {
        return false;
    };
    start_sandbox.command_id_digest == end_sandbox.command_id_digest
        && start_sandbox
            .command_id_binding_valid
            .is_none_or(|value| Some(value) == end_sandbox.command_id_binding_valid)
}

// Validate every persisted row and its lifecycle before serving a v12 store.
pub(crate) fn validate_trace_span_rows(connection: &Connection) -> StoreResult<()> {
    let mut statement = connection.prepare(
        "select event_id, run_id, session_id, payload, span_id, parent_span_id,
                span_kind, span_phase, span_status, duration_ms, time_to_first_token_ms,
                span_projection, metric_samples
         from trace_events order by rowid",
    )?;
    let rows = statement.query_map([], stored_trace_row)?;
    let raw_rows = rows.collect::<Result<Vec<_>, _>>()?;
    let mut events = Vec::with_capacity(raw_rows.len());
    for row in raw_rows {
        events.push(decode_stored_trace_row(row)?);
    }
    validate_trace_span_batch(&events)
}

// Public generic trace append may store external runs, but it cannot weaken a
// trace that identifies an existing thread or turn.
pub(crate) fn validate_public_trace_binding(
    connection: &Connection,
    event: &TraceEvent,
) -> StoreResult<()> {
    validate_public_trace_bindings(connection, std::slice::from_ref(event))
}

// Batch-prefetch the small set of thread/turn rows needed by a trace page.
// This keeps payload decoding row-local without issuing one binding query per event.
pub(crate) fn validate_public_trace_bindings(
    connection: &Connection,
    events: &[TraceEvent],
) -> StoreResult<()> {
    let thread_ids = events
        .iter()
        .map(|event| event.run_id.clone())
        .collect::<BTreeSet<_>>();
    let turn_ids = events
        .iter()
        .flat_map(|event| {
            event
                .task_id
                .iter()
                .chain(std::iter::once(&event.session_id))
                .cloned()
        })
        .collect::<BTreeSet<_>>();

    let existing_threads = select_trace_thread_ids(connection, &thread_ids)?;
    let turns = select_trace_turn_bindings(connection, &turn_ids)?;
    for event in events {
        let thread_exists = existing_threads.contains(&event.run_id);
        let turn_id = event.task_id.as_deref().unwrap_or(&event.session_id);
        match (thread_exists, turns.get(turn_id)) {
            (false, None) if event.task_id.is_none() => {}
            (false, None) => {
                return Err(StoreError::InvalidState(
                    "trace task_id must identify an existing turn".to_string(),
                ));
            }
            (true, None) if event.task_id.is_none() && event.session_id == event.run_id => {}
            (true, Some(thread_id)) | (false, Some(thread_id)) => {
                validate_turn_trace_binding(event, thread_id, turn_id)?;
            }
            (true, None) => {
                return Err(StoreError::InvalidState(
                    "trace for an existing thread must bind to the thread or an existing turn"
                        .to_string(),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn select_trace_thread_ids(
    connection: &Connection,
    thread_ids: &BTreeSet<String>,
) -> StoreResult<BTreeSet<String>> {
    if thread_ids.is_empty() {
        return Ok(BTreeSet::new());
    }
    let placeholders = std::iter::repeat_n("?", thread_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!("select thread_id from threads where thread_id in ({placeholders})");
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map(params_from_iter(thread_ids.iter()), |row| row.get(0))?;
    rows.collect::<Result<BTreeSet<_>, _>>()
        .map_err(StoreError::Sqlite)
}

pub(crate) fn select_trace_turn_bindings(
    connection: &Connection,
    turn_ids: &BTreeSet<String>,
) -> StoreResult<BTreeMap<String, String>> {
    if turn_ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let placeholders = std::iter::repeat_n("?", turn_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!("select turn_id, thread_id from turns where turn_id in ({placeholders})");
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map(params_from_iter(turn_ids.iter()), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut bindings = BTreeMap::new();
    for row in rows {
        let (turn_id, thread_id) = row?;
        if bindings.insert(turn_id.clone(), thread_id).is_some() {
            return Err(StoreError::InvalidState(format!(
                "duplicate turn binding {turn_id}"
            )));
        }
    }
    Ok(bindings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric_span(
        event_id: &str,
        span_id: &str,
        phase: TraceSpanPhase,
        duration_ms: u64,
    ) -> TraceEvent {
        let mut event = TraceEvent::new(event_id, "metric_run", "metric_session", "trace", "span");
        event.span_id = Some(span_id.to_string());
        event.span_kind = Some(TraceSpanKind::Turn);
        event.span_phase = Some(phase);
        if phase == TraceSpanPhase::End {
            event.span_status = Some(TraceSpanStatus::Ok);
            event.duration_ms = Some(duration_ms);
        }
        event
    }

    fn turn_duration(events: &[TraceEvent]) -> TraceMetric {
        derive_trace_metrics("metric_run", events)
            .expect("derive trace metrics")
            .metric("turn_duration_ms")
            .expect("turn duration metric")
            .clone()
    }

    fn assert_incomplete(events: &[TraceEvent]) {
        let metric = turn_duration(events);
        assert_eq!(
            metric.availability,
            TraceMetricAvailability::Unavailable {
                reason: TraceMetricUnavailableReason::IncompleteStartEnd
            }
        );
        assert!(metric.distribution.is_none());
    }

    #[test]
    fn span_metric_reducer_keeps_complete_pairs_available() {
        let metric = turn_duration(&[
            metric_span("turn_start", "turn", TraceSpanPhase::Start, 0),
            metric_span("turn_end", "turn", TraceSpanPhase::End, 7),
        ]);
        assert_eq!(metric.availability, TraceMetricAvailability::Available);
        assert_eq!(metric.distribution.as_ref().map(|value| value.sum), Some(7));
    }

    #[test]
    fn span_metric_reducer_fails_closed_for_mismatched_span_ids() {
        assert_incomplete(&[
            metric_span("turn_start", "started", TraceSpanPhase::Start, 0),
            metric_span("turn_end", "other", TraceSpanPhase::End, 7),
        ]);
    }

    #[test]
    fn mixed_duration_population_publishes_observed_values() {
        let mut missing_duration = metric_span("b_end", "b", TraceSpanPhase::End, 0);
        missing_duration.duration_ms = None;
        let metric = turn_duration(&[
            metric_span("a_start", "a", TraceSpanPhase::Start, 0),
            metric_span("a_end", "a", TraceSpanPhase::End, 7),
            metric_span("b_start", "b", TraceSpanPhase::Start, 0),
            missing_duration,
        ]);
        assert_eq!(metric.availability, TraceMetricAvailability::Available);
        assert_eq!(metric.distribution.as_ref().map(|value| value.sum), Some(7));
    }
}
