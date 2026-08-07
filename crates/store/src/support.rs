//! Shared validation, sequencing, redaction, and row-decoding helpers.

use super::*;

pub(crate) fn next_sequence(current: Option<i64>, label: &str) -> StoreResult<u64> {
    let current = current
        .map(|sequence| sequence_from_sql(sequence, label))
        .transpose()?
        .unwrap_or(0);
    current
        .checked_add(1)
        .ok_or_else(|| StoreError::InvalidState(format!("{label} overflow")))
}

// 将 u64 sequence 转换为 SQLite 可存储的 i64。
pub(crate) fn sequence_to_sql(sequence: u64, label: &str) -> StoreResult<i64> {
    i64::try_from(sequence)
        .map_err(|_| StoreError::InvalidState(format!("{label} exceeds sqlite integer range")))
}

// 将 SQLite sequence 解码为非负 u64。
pub(crate) fn sequence_from_sql(sequence: i64, label: &str) -> StoreResult<u64> {
    u64::try_from(sequence)
        .map_err(|_| StoreError::InvalidState(format!("{label} must be non-negative")))
}

// 按 item kind 清理敏感内容并返回是否发生脱敏。
pub(crate) fn sanitize_item_payload(
    kind: &ItemKind,
    mut payload: Value,
) -> StoreResult<(Value, bool)> {
    match kind {
        ItemKind::UserMessage => {
            let items = payload.as_array_mut().ok_or_else(|| {
                StoreError::InvalidState(
                    "user message payload must be an InputItem array".to_string(),
                )
            })?;
            let mut redacted = false;
            for item in items {
                let object = item.as_object_mut().ok_or_else(|| {
                    StoreError::InvalidState(
                        "user message contains malformed InputItem".to_string(),
                    )
                })?;
                let valid = object.len() == 2
                    && object.get("type").and_then(Value::as_str) == Some("text")
                    && object.get("text").is_some_and(Value::is_string);
                if !valid {
                    return Err(StoreError::InvalidState(
                        "user message contains malformed InputItem".to_string(),
                    ));
                }
                let text = object
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        StoreError::InvalidState(
                            "user message contains malformed InputItem".to_string(),
                        )
                    })?
                    .to_string();
                if contains_sensitive_text(&text) {
                    object.insert(
                        "text".to_string(),
                        Value::String(REDACTED_USER_INPUT.to_string()),
                    );
                    redacted = true;
                }
            }
            Ok((payload, redacted))
        }
        ItemKind::AgentMessage => {
            let object = payload.as_object_mut().ok_or_else(|| {
                StoreError::InvalidState("agent message payload must contain delta".to_string())
            })?;
            let valid = object.len() == 1 && object.get("delta").is_some_and(Value::is_string);
            if !valid {
                return Err(StoreError::InvalidState(
                    "agent message payload must contain only a string delta".to_string(),
                ));
            }
            let delta = object
                .get("delta")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    StoreError::InvalidState(
                        "agent message payload must contain only a string delta".to_string(),
                    )
                })?
                .to_string();
            let redacted = contains_sensitive_text(&delta);
            if redacted {
                object.insert(
                    "delta".to_string(),
                    Value::String(REDACTED_ASSISTANT_OUTPUT.to_string()),
                );
            }
            Ok((payload, redacted))
        }
        _ => {
            let serialized = serde_json::to_string(&payload)?;
            if contains_sensitive_text(&serialized) {
                Ok((serde_json::json!({"redacted": true}), true))
            } else {
                Ok((payload, false))
            }
        }
    }
}

// 将持久化 item 投影为模型可消费的 conversation message。
pub(crate) fn conversation_projection(
    kind: &ItemKind,
    payload: &Value,
) -> StoreResult<(ConversationRole, String)> {
    match kind {
        ItemKind::UserMessage => {
            let items = payload.as_array().ok_or_else(|| {
                StoreError::InvalidState("malformed user message payload".to_string())
            })?;
            let content = items
                .iter()
                .map(|item| {
                    item.get("text").and_then(Value::as_str).ok_or_else(|| {
                        StoreError::InvalidState("malformed user message InputItem".to_string())
                    })
                })
                .collect::<StoreResult<Vec<_>>>()?
                .join("\n");
            Ok((ConversationRole::User, content))
        }
        ItemKind::AgentMessage => {
            let content = payload
                .get("delta")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    StoreError::InvalidState("malformed agent message payload".to_string())
                })?
                .to_string();
            Ok((ConversationRole::Assistant, content))
        }
        _ => Err(StoreError::InvalidState(
            "non-conversation item selected for history".to_string(),
        )),
    }
}

// 执行 SQLite foreign_key_check，并拒绝已有违反项。
pub(crate) fn fail_closed_on_foreign_key_violations(
    connection: &Connection,
    phase: &str,
) -> StoreResult<()> {
    let violation = connection
        .query_row("pragma foreign_key_check", [], |row| {
            let table: String = row.get(0)?;
            let row_id: i64 = row.get(1)?;
            Ok(format!("{table}:{row_id}"))
        })
        .optional()?;
    if let Some(violation) = violation {
        return Err(StoreError::InvalidState(format!(
            "store foreign key violation during {phase}: {violation}"
        )));
    }
    Ok(())
}

// 校验 approval request 的显式 thread/turn 绑定。
pub(crate) fn ensure_approval_request_binding(
    connection: &Connection,
    request: &ApprovalRequest,
) -> StoreResult<()> {
    if request.thread_id.trim().is_empty() || request.turn_id.trim().is_empty() {
        return Err(StoreError::InvalidState(
            APPROVAL_BINDING_REQUIRED.to_string(),
        ));
    }
    ensure_request_turn_binding(connection, request)
}

// 校验 pending checkpoint 与 request 的 turn 绑定。
pub(crate) fn ensure_request_turn_binding(
    connection: &Connection,
    request: &ApprovalRequest,
) -> StoreResult<()> {
    let thread_id = connection
        .query_row(
            "select thread_id from turns where turn_id = ?1",
            params![request.turn_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => {
                StoreError::NotFound(format!("turn {}", request.turn_id))
            }
            other => StoreError::Sqlite(other),
        })?;
    if thread_id != request.thread_id {
        return Err(StoreError::InvalidState(
            APPROVAL_TURN_THREAD_MISMATCH.to_string(),
        ));
    }
    Ok(())
}

// 判断 turn status 是否已经不可再推进。
pub(crate) fn is_terminal_turn_status(status: &TurnStatus) -> bool {
    matches!(
        status,
        TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Interrupted
    )
}

// 校验状态更新没有覆盖终态或制造非法迁移。
pub(crate) fn validate_turn_status_update(
    current: &Turn,
    next_status: &TurnStatus,
    next_agent_loop_status: Option<&str>,
    authority: TurnOutcomeAuthority,
) -> StoreResult<()> {
    if authority == TurnOutcomeAuthority::InfrastructureFailure
        && (*next_status != TurnStatus::Failed || next_agent_loop_status != Some("failed"))
    {
        return Err(StoreError::InvalidState(
            "infrastructure failure can only finalize as failed".to_string(),
        ));
    }
    if current.agent_loop_status == "cancel_requested"
        && *next_status != TurnStatus::Interrupted
        && authority != TurnOutcomeAuthority::InfrastructureFailure
    {
        return Err(StoreError::InvalidState(
            "cancel-requested turn can only finalize as interrupted".to_string(),
        ));
    }
    if is_terminal_turn_status(&current.status) {
        if current.status != *next_status {
            return Err(StoreError::InvalidState(
                "terminal turn status cannot be overwritten".to_string(),
            ));
        }
        if next_agent_loop_status.is_some_and(|status| status != current.agent_loop_status) {
            return Err(StoreError::InvalidState(
                "terminal turn agent_loop_status cannot be overwritten".to_string(),
            ));
        }
    }
    Ok(())
}

// 将 approval request 编码并写入 approvals 表。
pub(crate) fn insert_approval(
    connection: &Connection,
    request: &ApprovalRequest,
) -> StoreResult<()> {
    ensure_approval_request_binding(connection, request)?;
    connection
        .execute(
            "insert into approvals(
                 request_id, thread_id, turn_id, payload, decision_outcome, decision_reason
             ) values(?1, ?2, ?3, ?4, null, null)",
            params![
                request.request_id,
                request.thread_id,
                request.turn_id,
                serde_json::to_string(request)?
            ],
        )
        .map_err(|error| match error {
            rusqlite::Error::SqliteFailure(ref sqlite_error, _)
                if sqlite_error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                StoreError::AlreadyExists(format!("approval {}", request.request_id))
            }
            other => StoreError::Sqlite(other),
        })?;
    Ok(())
}

// 生成用于持久化记录的短随机 id。
pub(crate) fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_string()
}

// 验证 artifact registration 的 thread/turn/item 绑定。
pub(crate) fn validate_artifact_binding(
    connection: &Connection,
    run_id: &str,
    item_id: Option<&str>,
) -> StoreResult<()> {
    validate_artifact_run(connection, run_id)?;
    if let Some(item_id) = item_id {
        validate_artifact_item(connection, run_id, item_id)?;
    }
    Ok(())
}

// 读取时重新验证持久化 artifact，避免数据库中被篡改的 ref 进入公共 fetch。
pub(crate) fn validate_stored_artifact(
    connection: &Connection,
    artifact: &ArtifactRef,
) -> StoreResult<()> {
    validate_artifact_id(&artifact.artifact_id)?;
    validate_artifact_binding(connection, &artifact.run_id, artifact.item_id.as_deref())?;
    let normalized_digest = validate_artifact_fields(
        &artifact.kind,
        &artifact.uri,
        &artifact.content_digest,
        &artifact.summary,
        &artifact.metadata,
    )?;
    if normalized_digest != artifact.content_digest {
        return Err(StoreError::InvalidState(format!(
            "artifact {} content digest is not canonical",
            artifact.artifact_id
        )));
    }
    if redact_secret_like_text(&artifact.uri) != artifact.uri
        || redact_secret_like_text(&artifact.summary) != artifact.summary
        || redact_secret_like_value(artifact.metadata.clone()) != artifact.metadata
    {
        return Err(StoreError::InvalidState(format!(
            "artifact {} contains unredacted sensitive data",
            artifact.artifact_id
        )));
    }
    Ok(())
}

pub(crate) fn validate_artifact_run(connection: &Connection, run_id: &str) -> StoreResult<()> {
    if run_id.trim().is_empty() {
        return Err(StoreError::InvalidState(
            "artifact run_id must not be empty".to_string(),
        ));
    }
    let thread_exists = connection
        .query_row(
            "select 1 from threads where thread_id = ?1",
            params![run_id],
            |_| Ok(()),
        )
        .optional()?;
    if thread_exists.is_some() {
        return Ok(());
    }
    let turn_exists = connection
        .query_row(
            "select 1 from turns where turn_id = ?1",
            params![run_id],
            |_| Ok(()),
        )
        .optional()?;
    if turn_exists.is_some() {
        return Err(StoreError::InvalidState(
            "artifact run_id must identify a thread, not a turn".to_string(),
        ));
    }
    Err(StoreError::NotFound(format!("artifact run {run_id}")))
}

pub(crate) fn validate_artifact_item(
    connection: &Connection,
    run_id: &str,
    item_id: &str,
) -> StoreResult<()> {
    if item_id.trim().is_empty() {
        return Err(StoreError::InvalidState(
            "artifact item_id must not be empty".to_string(),
        ));
    }
    let turn_id = connection
        .query_row(
            "select turn_id from items where item_id = ?1",
            params![item_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("artifact item {item_id}")))?;
    let item_thread_id = connection
        .query_row(
            "select thread_id from turns where turn_id = ?1",
            params![turn_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => StoreError::InvalidState(format!(
                "artifact item {item_id} references a missing turn"
            )),
            other => StoreError::Sqlite(other),
        })?;
    if item_thread_id != run_id {
        return Err(StoreError::InvalidState(
            "artifact item does not belong to run".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_artifact_fields(
    kind: &str,
    uri: &str,
    content_digest: &str,
    summary: &str,
    metadata: &Value,
) -> StoreResult<String> {
    if kind.trim().is_empty()
        || kind.len() > ARTIFACT_KIND_MAX_BYTES
        || !kind.chars().enumerate().all(|(index, character)| {
            (index == 0 && character.is_ascii_alphanumeric())
                || (index > 0
                    && (character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')))
        })
        || contains_artifact_reference(kind)
    {
        return Err(StoreError::InvalidState(
            "artifact kind is invalid".to_string(),
        ));
    }
    validate_artifact_text("uri", uri)?;
    let Some(uri_path) = uri.strip_prefix(ARTIFACT_URI_PREFIX) else {
        return Err(StoreError::InvalidState(
            "artifact uri must use artifact://".to_string(),
        ));
    };
    if uri_path.is_empty()
        || contains_sensitive_text(uri)
        || is_protected_path(uri_path)
        || contains_artifact_reference(uri_path)
    {
        return Err(StoreError::InvalidState(
            "artifact uri is not safe".to_string(),
        ));
    }
    validate_artifact_text("summary", summary)?;
    if contains_artifact_reference(summary) {
        return Err(StoreError::InvalidState(
            "artifact summary contains an unregistered reference".to_string(),
        ));
    }
    let normalized_digest = validate_artifact_digest(content_digest)?;
    validate_artifact_metadata(metadata)?;
    Ok(normalized_digest)
}

pub(crate) fn validate_artifact_id(artifact_id: &str) -> StoreResult<()> {
    if artifact_id.len() <= "artifact_".len()
        || artifact_id.len() > ARTIFACT_TEXT_MAX_BYTES
        || !artifact_id.starts_with("artifact_")
        || !artifact_id["artifact_".len()..]
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return Err(StoreError::InvalidState(
            "artifact id is invalid".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_artifact_text(field: &str, value: &str) -> StoreResult<()> {
    if value.trim().is_empty()
        || value.len() > ARTIFACT_TEXT_MAX_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(StoreError::InvalidState(format!(
            "artifact {field} is invalid"
        )));
    }
    Ok(())
}

pub(crate) fn validate_artifact_digest(value: &str) -> StoreResult<String> {
    let Some(hex) = value.strip_prefix(TRACE_HASH_PREFIX) else {
        return Err(StoreError::InvalidState(
            "artifact content digest must use sha256".to_string(),
        ));
    };
    if hex.len() != SHA256_HEX_LENGTH || !hex.chars().all(|character| character.is_ascii_hexdigit())
    {
        return Err(StoreError::InvalidState(
            "artifact content digest is invalid".to_string(),
        ));
    }
    Ok(format!("{TRACE_HASH_PREFIX}{}", hex.to_ascii_lowercase()))
}

pub(crate) fn validate_artifact_metadata(value: &Value) -> StoreResult<()> {
    let size = serde_json::to_vec(value)?.len();
    if size > ARTIFACT_METADATA_MAX_BYTES {
        return Err(StoreError::InvalidState(
            "artifact metadata is too large".to_string(),
        ));
    }
    validate_artifact_metadata_value(value, 0)
}

pub(crate) fn validate_artifact_metadata_value(value: &Value, depth: usize) -> StoreResult<()> {
    if depth > ARTIFACT_METADATA_MAX_DEPTH {
        return Err(StoreError::InvalidState(
            "artifact metadata is too deeply nested".to_string(),
        ));
    }
    match value {
        Value::Object(entries) => {
            for (key, value) in entries {
                if key.trim().is_empty()
                    || key.len() > ARTIFACT_TEXT_MAX_BYTES
                    || key.chars().any(char::is_control)
                    || is_artifact_reference_key(key)
                {
                    return Err(StoreError::InvalidState(
                        "artifact metadata contains an invalid reference field".to_string(),
                    ));
                }
                validate_artifact_metadata_value(value, depth + 1)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_artifact_metadata_value(value, depth + 1)?;
            }
        }
        Value::String(text) => {
            if text.chars().any(char::is_control)
                || contains_artifact_reference(text)
                || is_protected_path(text)
            {
                return Err(StoreError::InvalidState(
                    "artifact metadata contains unsafe text".to_string(),
                ));
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

pub(crate) fn is_artifact_reference_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "artifact_ref"
            | "artifact_refs"
            | "artifactref"
            | "artifactrefs"
            | "diff_ref"
            | "diff_refs"
            | "diffref"
            | "diffrefs"
    )
}

pub(crate) fn contains_artifact_reference(value: &str) -> bool {
    value.to_ascii_lowercase().contains(ARTIFACT_URI_PREFIX)
}

// 将 artifact_refs 行解码为 ArtifactRef。
pub(crate) fn artifact_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRef> {
    let metadata: String = row.get(7)?;
    Ok(ArtifactRef {
        artifact_id: row.get(0)?,
        run_id: row.get(1)?,
        item_id: row.get(2)?,
        kind: row.get(3)?,
        uri: row.get(4)?,
        content_digest: row.get(5)?,
        summary: row.get(6)?,
        metadata: serde_json::from_str(&metadata).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        redacted: row.get(8)?,
    })
}

/// SQLite trace row, including every read-only projection column.
pub(crate) struct StoredTraceRow {
    pub(crate) event_id: String,
    pub(crate) run_id: String,
    pub(crate) session_id: String,
    pub(crate) payload: String,
    pub(crate) span_id: Option<String>,
    pub(crate) parent_span_id: Option<String>,
    pub(crate) span_kind: Option<String>,
    pub(crate) span_phase: Option<String>,
    pub(crate) span_status: Option<String>,
    pub(crate) duration_ms: Option<i64>,
    pub(crate) time_to_first_token_ms: Option<i64>,
    pub(crate) span_projection: Option<String>,
    pub(crate) metric_samples: String,
}

pub(crate) fn stored_trace_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredTraceRow> {
    Ok(StoredTraceRow {
        event_id: row.get(0)?,
        run_id: row.get(1)?,
        session_id: row.get(2)?,
        payload: row.get(3)?,
        span_id: row.get(4)?,
        parent_span_id: row.get(5)?,
        span_kind: row.get(6)?,
        span_phase: row.get(7)?,
        span_status: row.get(8)?,
        duration_ms: row.get(9)?,
        time_to_first_token_ms: row.get(10)?,
        span_projection: row.get(11)?,
        metric_samples: row.get(12)?,
    })
}

// 解码现行 trace 行，同时验证列与 payload 的完整投影一致。
pub(crate) fn decode_stored_trace_row(row: StoredTraceRow) -> StoreResult<TraceEvent> {
    let StoredTraceRow {
        event_id,
        run_id,
        session_id,
        payload,
        span_id,
        parent_span_id,
        span_kind,
        span_phase,
        span_status,
        duration_ms,
        time_to_first_token_ms,
        span_projection,
        metric_samples,
    } = row;
    let event = decode_trace_payload(&payload)?;
    let projected_span = span_projection
        .as_deref()
        .map(serde_json::from_str::<TraceSpanProjection>)
        .transpose()
        .map_err(|error| {
            StoreError::InvalidState(format!("trace span projection is invalid: {error}"))
        })?;
    let projected_samples = serde_json::from_str::<Vec<TraceMetricSample>>(&metric_samples)
        .map_err(|error| {
            StoreError::InvalidState(format!("trace metric samples are invalid: {error}"))
        })?;
    if event.event_id != event_id
        || event.run_id != run_id
        || event.session_id != session_id
        || event.span_id != span_id
        || event.parent_span_id != parent_span_id
        || event.span_kind.map(|kind| kind.as_storage_text()) != span_kind.as_deref()
        || event.span_phase.map(|phase| phase.as_storage_text()) != span_phase.as_deref()
        || event.span_status.map(|status| status.as_storage_text()) != span_status.as_deref()
        || optional_i64_to_u64(duration_ms, "trace duration")? != event.duration_ms
        || optional_i64_to_u64(time_to_first_token_ms, "trace time to first token")?
            != event.time_to_first_token_ms
        || event.span_projection != projected_span
        || event.metric_samples != projected_samples
    {
        return Err(StoreError::InvalidState(format!(
            "trace {event_id} columns do not match payload"
        )));
    }
    Ok(event)
}

// 解码 trace payload 并恢复完整性校验所需对象。
pub(crate) fn decode_trace_payload(payload: &str) -> StoreResult<TraceEvent> {
    let event: TraceEvent = serde_json::from_str(payload)?;
    if !event.redaction_applied {
        return Err(StoreError::TraceIntegrity(
            "stored trace was not sanitized".to_string(),
        ));
    }
    event
        .validate_span_lifecycle()
        .map_err(|error| StoreError::InvalidState(format!("trace span is invalid: {error}")))?;
    let expected_hash = trace_envelope_hash(&event);
    if event.payload_hash != expected_hash {
        return Err(StoreError::TraceIntegrity(format!(
            "event envelope hash mismatch for {}",
            event.event_id
        )));
    }
    Ok(event)
}

// 对 trace 的 payload 与可见文本执行脱敏投影。
pub(crate) fn sanitize_trace_event(event: &TraceEvent) -> TraceEvent {
    let mut sanitized = event.clone();
    sanitized.summary = redact_secret_like_text(&sanitized.summary);
    sanitized.payload = redact_secret_like_value(sanitized.payload);
    sanitized.span_projection = sanitized.span_projection.map(sanitize_trace_projection);
    sanitized.artifact_refs = sanitized
        .artifact_refs
        .into_iter()
        .map(|artifact_ref| redact_secret_like_text(&artifact_ref))
        .collect();
    sanitized.redaction_applied = true;
    sanitized.payload_hash = trace_envelope_hash(&sanitized);
    sanitized
}

fn sanitize_trace_projection(mut projection: TraceSpanProjection) -> TraceSpanProjection {
    projection.provider_name = projection
        .provider_name
        .map(|value| redact_secret_like_text(&value));
    projection.model_name = projection
        .model_name
        .map(|value| redact_secret_like_text(&value));
    projection.error = projection.error.map(|mut error| {
        error.code = error.code.map(|value| {
            bounded_stable_code(&value).unwrap_or_else(|| REDACTED_ARTIFACT_VALUE.to_string())
        });
        error
    });
    projection.tool = projection.tool.map(|mut tool| {
        tool.tool_name = tool.tool_name.map(|value| redact_secret_like_text(&value));
        tool.tool_call_id_digest = tool
            .tool_call_id_digest
            .map(|value| redact_secret_like_text(&value));
        tool
    });
    projection.sandbox = projection.sandbox.map(|mut sandbox| {
        sandbox.command_id_digest = sandbox
            .command_id_digest
            .map(|value| redact_secret_like_text(&value));
        sandbox
    });
    projection
}

// 对 canonical JSON 计算带前缀的 SHA-256 摘要。
pub(crate) fn trace_payload_hash(payload: &Value) -> String {
    trace_value_hash(payload)
}

// 对脱敏后的完整 event envelope 计算摘要，payload_hash 本身不参与输入。
pub(crate) fn trace_envelope_hash(event: &TraceEvent) -> String {
    let mut envelope = serde_json::to_value(event).expect("trace event serialization cannot fail");
    if let Value::Object(fields) = &mut envelope {
        fields.remove("payload_hash");
    }
    trace_value_hash(&envelope)
}

fn trace_value_hash(value: &Value) -> String {
    let canonical = canonical_json(value);
    let digest = Sha256::digest(canonical.as_bytes());
    format!("{TRACE_HASH_PREFIX}{digest:x}")
}

pub(crate) fn optional_i64_to_u64(value: Option<i64>, label: &str) -> StoreResult<Option<u64>> {
    value
        .map(|value| {
            u64::try_from(value)
                .map_err(|_| StoreError::InvalidState(format!("{label} must be non-negative")))
        })
        .transpose()
}

// 以稳定 key 顺序序列化 JSON，作为哈希输入。
pub(crate) fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            serde_json::to_string(value).expect("JSON scalar serialization cannot fail")
        }
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(fields) => {
            let ordered = fields.iter().collect::<BTreeMap<_, _>>();
            let entries = ordered
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("JSON key serialization cannot fail"),
                        canonical_json(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{entries}}}")
        }
    }
}

// 递归识别并替换 secret-like JSON 值。
pub(crate) fn redact_secret_like_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact_secret_like_text(&text)),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(redact_secret_like_value).collect())
        }
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| {
                    if contains_secret_like(&key) {
                        (key, Value::String(REDACTED_ARTIFACT_VALUE.to_string()))
                    } else {
                        (key, redact_secret_like_value(value))
                    }
                })
                .collect(),
        ),
        other => other,
    }
}

// 判断 artifact URI、摘要或 metadata 是否触发脱敏。
pub(crate) fn artifact_needs_redaction(uri: &str, summary: &str, metadata: &Value) -> bool {
    contains_secret_like(uri)
        || contains_secret_like(summary)
        || value_contains_secret_like(metadata)
}

// 递归判断 JSON 值是否包含 secret-like 内容。
pub(crate) fn value_contains_secret_like(value: &Value) -> bool {
    match value {
        Value::String(text) => contains_secret_like(text),
        Value::Array(items) => items.iter().any(value_contains_secret_like),
        Value::Object(entries) => entries
            .iter()
            .any(|(key, value)| contains_secret_like(key) || value_contains_secret_like(value)),
        _ => false,
    }
}

// 将文本中的 secret-like 片段替换为统一占位符。
pub(crate) fn redact_secret_like_text(text: &str) -> String {
    if contains_secret_like(text) {
        REDACTED_ARTIFACT_VALUE.to_string()
    } else {
        text.to_string()
    }
}

// 判断文本是否命中敏感 marker 或 core 敏感文本规则。
pub(crate) fn contains_secret_like(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    contains_sensitive_text(text)
        || SENSITIVE_ARTIFACT_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker))
}
