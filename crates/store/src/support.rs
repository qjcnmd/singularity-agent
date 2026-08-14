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

// 生成用于持久化记录的短随机 id。
pub(crate) fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_string()
}
