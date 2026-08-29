#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
use super::*;

fn user(text: &str) -> AgentMessage {
    AgentMessage::User {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

fn assistant(text: &str) -> AgentMessage {
    AgentMessage::Assistant {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        stop_reason: None,
        provider_reasoning_replay: None,
    }
}

fn tool_result(call_id: &str, text: &str) -> AgentMessage {
    AgentMessage::ToolResult {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        tool_call_id: Some(call_id.to_string()),
        tool_name: Some("bash".to_string()),
        is_error: None,
        file_operations: None,
    }
}

fn entry_ids(entries: &[SessionEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.id().to_string()).collect()
}

fn session_header(id: &str) -> String {
    format!(
        r#"{{"type":"session","version":2,"id":"{id}","timestamp":"2026-08-20T00:00:00.000Z","cwd":"C:/work"}}"#
    )
}

fn session_message(id: &str, text: &str) -> String {
    format!(
        r#"{{"type":"message","id":"{id}","timestamp":"2026-08-20T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
    )
}

#[test]
fn create_append_reopen_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let cwd = dir.path().join("project");
    let mut manager = SessionManager::create(&cwd, &sessions).unwrap();
    assert!(manager.entries().is_empty());

    let id1 = manager.append_message(user("hello")).unwrap();
    let id2 = manager.append_message(assistant("hi there")).unwrap();
    let id3 = manager
        .append_message(tool_result("call_1", "ls output"))
        .unwrap();
    let file = manager.path().to_path_buf();
    let leaf = manager
        .entries()
        .last()
        .expect("content entry")
        .id()
        .to_string();
    assert_eq!(leaf, id3);

    let content = std::fs::read_to_string(&file).unwrap();
    let first_line: Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert_eq!(first_line["type"], "session");
    assert_eq!(first_line["version"], 2);
    assert_eq!(
        first_line["cwd"],
        normalize_cwd_string(&std::path::absolute(&cwd).unwrap())
    );
    let header_id = first_line["id"].as_str().unwrap();
    let file_name = manager.path().file_name().unwrap().to_str().unwrap();
    assert_eq!(file_name, format!("{header_id}.jsonl"));
    drop(manager);

    let opened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(
        opened.entries().last().expect("content entry").id(),
        leaf.as_str()
    );
    let entries = opened.build_context_entries().unwrap();
    assert_eq!(entry_ids(&entries), vec![id1, id2, id3]);
    for entry in &entries {
        assert_eq!(entry.id().len(), 8);
        assert!(entry.id().chars().all(|c| c.is_ascii_hexdigit()));
    }
    assert!(matches!(&entries[0],
            SessionEntry::Message { message: m, .. } if m.role() == AgentMessageRole::User && m.content_text() == "hello"));
    assert!(matches!(&entries[1],
            SessionEntry::Message { message: m, .. } if m.role() == AgentMessageRole::Assistant && m.content_text() == "hi there"));
    assert!(matches!(&entries[2],
            SessionEntry::Message { message: m, .. } if m.role() == AgentMessageRole::ToolResult
                && m.tool_call_id().is_some_and(|id| id == "call_1")
                && m.tool_name().is_some_and(|name| name == "bash")));
}

#[test]
fn empty_existing_session_file_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("empty.jsonl");
    std::fs::write(&file, b"").unwrap();
    let result = SessionManager::open_existing(&file);
    assert!(
        matches!(result, Err(SessionError::InvalidSession(_))),
        "empty session file must fail closed"
    );
}

#[test]
fn reopen_reads_full_durable_linear_chain_after_owner_transitions() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let cwd = dir.path().join("project");
    let mut turn_worker = SessionManager::create(&cwd, &sessions).unwrap();
    let m1 = turn_worker.append_message(user("first")).unwrap();
    let m2 = turn_worker.append_message(assistant("second")).unwrap();
    let file = turn_worker.path().to_path_buf();
    drop(turn_worker);

    // 单写者语义下，写者交接必须经重开（drop 后再 open），同一时刻至多一个
    // 存活的写者。后续 owner 追加 metadata，再后续 owner 继续追加消息。
    let mut settings_writer = SessionManager::open_existing(&file).unwrap();
    let s1 = settings_writer
        .append_metadata(SessionMetadata::thread_settings(
            "openai",
            "test-model",
            Some("high".to_string()),
        ))
        .unwrap();
    drop(settings_writer);
    let mut turn_worker = SessionManager::open_existing(&file).unwrap();
    let m3 = turn_worker.append_message(user("third")).unwrap();
    drop(turn_worker);

    // 重开从 JSONL 重建完整线性链。
    let reopened = SessionManager::open_existing(&file).unwrap();
    let entries = reopened.build_context_entries().unwrap();
    assert_eq!(entry_ids(&entries), vec![m1, m2, s1, m3]);
    // 线性事实源：entries 依次落盘，id 无重复。
    assert_eq!(
        entries.len(),
        reopened.entries.len(),
        "context entries match the full linear file order"
    );
    let ids = entries
        .iter()
        .map(super::format::SessionEntry::id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(ids.len(), entries.len(), "entry ids must be unique");
}

#[test]
fn reopen_interrupted_repair_is_idempotent_and_synthetic() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
    manager
        .append_metadata(SessionMetadata::turn_started("turn_1"))
        .unwrap();
    manager
        .append_metadata(SessionMetadata::turn_started("turn_2"))
        .unwrap();
    manager
        .append_metadata(SessionMetadata::turn_terminal(
            "turn_1",
            TurnTerminalStatus::Completed,
            TurnModelUsage {
                total_tokens: 42,
                usage_present: true,
                usage_complete: true,
                ..TurnModelUsage::default()
            },
            true,
        ))
        .unwrap();
    let file = manager.path().to_path_buf();
    drop(manager);

    let mut reopened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(reopened.repair_interrupted_turns().unwrap(), 1);
    drop(reopened);

    let reopened = SessionManager::open_existing(&file).unwrap();
    let interrupted = reopened
        .metadata_entries()
        .into_iter()
        .find(|entry| {
            entry.kind() == SessionMetadataKind::TurnTerminal
                && entry.terminal_status() == Some(TurnTerminalStatus::Interrupted)
        })
        .expect("interrupted terminal entry exists");
    assert_eq!(interrupted.turn_id(), Some("turn_2"));
    drop(reopened);

    let mut reopened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(reopened.repair_interrupted_turns().unwrap(), 0);
}

#[test]
fn thread_settings_reject_sensitive_fields() {
    for key in ["apiKey", "authorization", "auth_token", "password"] {
        let mut value = json!({
            "metadataType": "thread_settings",
            "provider": "openai",
            "model": "test-model"
        });
        value
            .as_object_mut()
            .expect("settings object")
            .insert(key.to_string(), json!("secret"));
        assert!(
            serde_json::from_value::<SessionMetadata>(value).is_err(),
            "{key} must be rejected"
        );
    }
}

#[test]
fn typed_metadata_round_trips_existing_flat_wire_shape() {
    let cases = [
        json!({"metadataType": "turn_started", "turnId": "turn-1"}),
        json!({
            "metadataType": "turn_terminal",
            "turnId": "turn-1",
            "status": "completed",
            "usage": {
                "inputTokens": 0,
                "outputTokens": 0,
                "totalTokens": 42,
                "cachedInputTokens": 0,
                "reasoningTokens": 0,
                "usagePresent": true,
                "usageComplete": true
            },
            "usageComplete": true
        }),
        json!({
            "metadataType": "turn_terminal",
            "turnId": "turn-1",
            "status": "failed",
            "usage": {
                "inputTokens": 0,
                "outputTokens": 0,
                "totalTokens": 0,
                "cachedInputTokens": 0,
                "reasoningTokens": 0,
                "usagePresent": false,
                "usageComplete": false
            },
            "usageComplete": false
        }),
        json!({
            "metadataType": "thread_settings",
            "provider": "openai",
            "model": "test-model",
            "reasoning": "high"
        }),
        json!({"metadataType": "thread_name", "name": "Typed metadata"}),
        json!({"metadataType": "thread_settings", "model": "legacy-model"}),
    ];
    for value in cases {
        let metadata: SessionMetadata =
            serde_json::from_value(value.clone()).expect("read metadata");
        assert_eq!(
            serde_json::to_value(metadata).expect("write metadata"),
            value
        );
    }

    // 兼容读取：历史文件的部分/旧命名 usage 形状仍可反序列化。
    let legacy: SessionMetadata = serde_json::from_value(json!({
        "metadataType": "turn_terminal",
        "turnId": "turn-1",
        "status": "completed",
        "usage": {"totalTokens": 42},
        "usageComplete": true
    }))
    .expect("legacy camelCase usage reads");
    assert!(matches!(
        &legacy,
        SessionMetadata::TurnTerminal { usage, .. }
            if usage.total_tokens == 42 && usage.usage_present
    ));
    let snake: TurnModelUsage =
        serde_json::from_value(json!({"input_tokens": 3})).expect("snake usage reads");
    assert_eq!(snake.input_tokens, 3);

    let metadata: SessionMetadata = serde_json::from_value(json!({
        "metadataType": "thread_settings",
        "provider": "openai",
        "model": "test-model",
        "reasoning": "high"
    }))
    .expect("read typed settings");
    assert!(matches!(
        metadata,
        SessionMetadata::ThreadSettings {
            ref provider,
            ref model,
            reasoning: Some(ref reasoning),
        } if provider.as_deref() == Some("openai") && model == "test-model" && reasoning == "high"
    ));
}

/// v2 格式契约：三类条目的未知字段一律拒绝（含载荷内部与消息/内容块层级）。
#[test]
fn unknown_fields_are_rejected_across_all_entry_kinds() {
    let cases = [
        json!({
            "type": "message",
            "id": "m-1",
            "message": {"role": "user", "content": [{"type": "text", "text": "hi"}], "unknown": 1}
        }),
        json!({
            "type": "message",
            "id": "m-1",
            "message": {"role": "user", "content": [{"type": "text", "text": "hi", "extra": true}]}
        }),
        json!({
            "type": "message",
            "id": "m-1",
            "unknownField": 1,
            "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}
        }),
        json!({
            "type": "compaction",
            "id": "c-1",
            "compaction": {"summary": "s", "unknown": 1}
        }),
        json!({
            "type": "compaction",
            "id": "c-1",
            "extra": true,
            "compaction": {"summary": "s"}
        }),
        json!({
            "type": "metadata",
            "id": "md-1",
            "metadata": {"metadataType": "turn_started", "turnId": "t-1", "unknown": 1}
        }),
        json!({
            "type": "metadata",
            "id": "md-1",
            "metadata": {"metadataType": "turn_terminal", "turnId": "t-1", "status": "completed", "usage": {}, "usageComplete": true, "extra": false}
        }),
        json!({
            "type": "metadata",
            "id": "md-1",
            "stray": 1,
            "metadata": {"metadataType": "thread_name", "name": "n"}
        }),
    ];
    for value in cases {
        assert!(
            serde_json::from_value::<SessionEntry>(value.clone()).is_err(),
            "unknown fields must be rejected: {value}"
        );
    }
}

/// v2 格式契约：v1 文件在 header 校验处按版本号拒绝。
#[test]
fn v1_session_files_are_rejected_by_version() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("v1.jsonl");
    std::fs::write(
        &file,
        r#"{"type":"session","version":1,"id":"01914f6b-0000-7000-8000-000000000001","timestamp":"2026-08-20T00:00:00.000Z","cwd":"C:/work"}
{"type":"message","id":"m-1","timestamp":"2026-08-20T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}
"#,
    )
    .unwrap();
    assert!(matches!(
        SessionManager::open_existing(&file).unwrap_err(),
        SessionError::InvalidHeader(_)
    ));
}

/// v2 格式契约：新格式完整 round-trip（含嵌套载荷与终态单条）。
#[test]
fn v2_format_round_trips_nested_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let message_id = manager
        .append_message(AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
            stop_reason: None,
            provider_reasoning_replay: None,
        })
        .unwrap();
    let compaction_id = manager
        .append_compaction(CompactionEntry {
            summary: "compacted".to_string(),
            first_kept_entry_id: Some("m-1".to_string()),
            tokens_before: Some(123),
            usage: None,
            details: None,
        })
        .unwrap();
    let metadata_id = manager
        .append_metadata(SessionMetadata::turn_terminal(
            "turn-1",
            TurnTerminalStatus::Completed,
            TurnModelUsage {
                total_tokens: 7,
                usage_present: true,
                usage_complete: true,
                ..TurnModelUsage::default()
            },
            true,
        ))
        .unwrap();
    let file = manager.path().to_path_buf();
    drop(manager);

    let reopened = SessionManager::open_existing(&file).unwrap();
    let entries = reopened.entries();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].id(), message_id);
    assert_eq!(entries[1].id(), compaction_id);
    assert_eq!(entries[2].id(), metadata_id);
    assert!(matches!(
        &entries[2],
        SessionEntry::Metadata {
            metadata:
                SessionMetadata::TurnTerminal {
                    turn_id,
                    status: TurnTerminalStatus::Completed,
                    usage,
                    usage_complete: true,
                },
            ..
        } if turn_id == "turn-1"
            && usage
                == &TurnModelUsage {
                    total_tokens: 7,
                    usage_present: true,
                    usage_complete: true,
                    ..TurnModelUsage::default()
                }
    ));
}

#[test]
fn repair_orphaned_tool_calls_appends_synthetic_failed_result_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
    manager
        .append_message(AgentMessage::Assistant {
            content: vec![ContentBlock::ToolCall {
                id: "orphan_call_1".to_string(),
                name: "bash".to_string(),
                args: json!({"command": "cargo test"}),
            }],
            stop_reason: None,
            provider_reasoning_replay: None,
        })
        .unwrap();
    let file = manager.path().to_path_buf();
    drop(manager);

    let mut reopened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(reopened.repair_orphaned_tool_calls().unwrap(), 1);
    drop(reopened);

    let reopened = SessionManager::open_existing(&file).unwrap();
    let entries = reopened.build_context_entries().unwrap();
    let tool_results = entries
        .iter()
        .filter_map(|entry| match &entry {
            SessionEntry::Message { message, .. }
                if message.role() == AgentMessageRole::ToolResult =>
            {
                Some(message)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(
        tool_results[0].tool_call_id(),
        Some(&"orphan_call_1".to_string())
    );
    assert!(
        tool_results[0]
            .content_text()
            .contains("previous execution outcome unknown")
    );
    assert!(tool_results[0].content_text().contains("do not retry"));
    drop(reopened);

    let mut reopened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(reopened.repair_orphaned_tool_calls().unwrap(), 0);
}

#[test]
fn strict_open_repairs_torn_tail_and_missing_final_newline() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir
        .path()
        .join("01914f6b-0000-7000-8000-000000000001.jsonl");
    let prefix = format!(
        "{}\n{}\n",
        session_header("01914f6b-0000-7000-8000-000000000001"),
        session_message("entry-1", "one")
    );
    std::fs::write(&file, format!("{prefix}{{\"type\":\"message\",\"id\":\"")).unwrap();
    let opened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(opened.build_context_entries().unwrap().len(), 1);
    assert!(std::fs::read(&file).unwrap().ends_with(b"\n"));
    drop(opened);
    let mut reopened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(reopened.build_context_entries().unwrap().len(), 1);
    reopened.append_message(user("after repair")).unwrap();
    drop(reopened);
    let reopened_again = SessionManager::open_existing(&file).unwrap();
    assert_eq!(reopened_again.build_context_entries().unwrap().len(), 2);

    let missing_newline = dir
        .path()
        .join("01914f6b-0000-7000-8000-000000000002.jsonl");
    std::fs::write(
        &missing_newline,
        format!(
            "{}\n{}",
            session_header("01914f6b-0000-7000-8000-000000000002"),
            session_message("entry-1", "one")
        ),
    )
    .unwrap();
    let opened = SessionManager::open_existing(&missing_newline).unwrap();
    assert_eq!(opened.build_context_entries().unwrap().len(), 1);
    assert!(std::fs::read(&missing_newline).unwrap().ends_with(b"\n"));
}

#[test]
fn read_only_open_rejects_repairable_tail_without_mutating_file() {
    let dir = tempfile::tempdir().unwrap();
    let id = "01914f6b-0000-7000-8000-000000000004";
    let file = dir.path().join(format!("{id}.jsonl"));
    let original = format!(
        "{}\n{}\n{{\"type\":\"message\",\"id\":\"",
        session_header(id),
        session_message("entry-1", "one")
    );
    std::fs::write(&file, &original).unwrap();

    let error = SessionManager::open_existing_read_only(&file)
        .expect_err("discovery must reject a rollout requiring tail repair");
    assert!(error.to_string().contains("read-only"), "{error}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
}

#[test]
fn read_only_open_preserves_header_creation_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let id = "01914f6b-0000-7000-8000-000000000005";
    let file = dir.path().join(format!("{id}.jsonl"));
    std::fs::write(
        &file,
        format!(
            "{}\n{}\n",
            session_header(id),
            session_message("entry-1", "one")
        ),
    )
    .unwrap();

    let opened = SessionManager::open_existing_read_only(&file).unwrap();
    assert_eq!(opened.created_at(), "2026-08-20T00:00:00.000Z");
}

#[test]
fn strict_open_rejects_invalid_headers_and_old_versions() {
    let dir = tempfile::tempdir().unwrap();

    // 1. 缺失 version
    let missing_version = dir.path().join("missing-version.jsonl");
    std::fs::write(
        &missing_version,
        r#"{"type":"session","id":"01914f6b-0000-7000-8000-000000000001","timestamp":"2026-08-20T00:00:00.000Z","cwd":"C:/work"}"#,
    )
    .unwrap();
    assert!(matches!(
        SessionManager::open_existing(&missing_version).unwrap_err(),
        SessionError::InvalidHeader(_)
    ));

    // 2. 非当前版本 v1, v3, v4
    for old_v in [1, 3, 4] {
        let old_file = dir.path().join(format!("old-v{old_v}.jsonl"));
        std::fs::write(
            &old_file,
            format!(
                r#"{{"type":"session","version":{old_v},"id":"01914f6b-0000-7000-8000-000000000001","timestamp":"2026-08-20T00:00:00.000Z","cwd":"C:/work"}}"#
            ),
        )
        .unwrap();
        assert!(matches!(
            SessionManager::open_existing(&old_file).unwrap_err(),
            SessionError::InvalidHeader(_)
        ));
    }

    // 3. header 含有未知字段
    let unknown_field = dir.path().join("unknown-field.jsonl");
    std::fs::write(
        &unknown_field,
        r#"{"type":"session","version":2,"id":"01914f6b-0000-7000-8000-000000000001","timestamp":"2026-08-20T00:00:00.000Z","cwd":"C:/work","extra":"field"}"#,
    )
    .unwrap();
    assert!(matches!(
        SessionManager::open_existing(&unknown_field).unwrap_err(),
        SessionError::InvalidHeader(_)
    ));

    // 4. header id 不是合法 UUID
    let non_uuid = dir.path().join("non-uuid.jsonl");
    std::fs::write(
        &non_uuid,
        r#"{"type":"session","version":2,"id":"not-a-uuid","timestamp":"2026-08-20T00:00:00.000Z","cwd":"C:/work"}"#,
    )
    .unwrap();
    assert!(matches!(
        SessionManager::open_existing(&non_uuid).unwrap_err(),
        SessionError::InvalidHeader(_)
    ));
}

#[test]
fn append_io_failure_does_not_advance_memory() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let before = manager.build_context_entries().unwrap();
    manager.file = dir.path().to_path_buf();
    assert!(manager.append_message(user("must fail")).is_err());
    assert_eq!(manager.build_context_entries().unwrap(), before);
    assert!(manager.entries().is_empty());
}

#[test]
fn append_limits_reject_without_writing_or_advancing_memory() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let mut manager = SessionManager::create(dir.path(), &sessions).unwrap();
    let before_bytes = std::fs::read(manager.path()).unwrap();
    let error = manager
        .append_entry_with_limits(
            SessionEntry::Message {
                id: "oversized".to_string(),
                timestamp: None,
                message: user("oversized"),
            },
            AppendLimits {
                line_bytes: 1,
                file_bytes: 1024 * 1024,
                entries: 10,
            },
        )
        .expect_err("line limit should reject");
    assert!(matches!(
        error,
        SessionError::AppendLimitExceeded {
            kind: "line bytes",
            ..
        }
    ));
    assert_eq!(std::fs::read(manager.path()).unwrap(), before_bytes);
    assert!(manager.entries().is_empty());
}
#[test]
fn verify_session_id_matches_or_rejects_header() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let cwd = dir.path().join("project");
    let manager = SessionManager::create(&cwd, &sessions).unwrap();
    let id = manager.session_id().to_string();

    assert!(manager.verify_session_id(&id).is_ok());
    let error = manager
        .verify_session_id("other-id")
        .expect_err("must reject mismatch");
    assert!(matches!(error, SessionError::InvalidHeader(_)));
    assert!(error.to_string().contains("other-id"));
}
#[test]
fn access_open_repair_write_repairs_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
    let session_id = manager.session_id().to_string();
    manager
        .append_metadata(SessionMetadata::turn_started("turn_1"))
        .unwrap();
    let file = manager.path().to_path_buf();
    drop(manager);

    let coordinator = std::sync::Arc::new(WriterLockCoordinator::new(dir.path()));
    let opened = SessionManager::open_existing_with_access(
        &file,
        &coordinator,
        &session_id,
        SessionAccess::RepairWrite,
    )
    .unwrap();
    drop(opened);

    let reopened = SessionManager::open_existing_read_only(&file).unwrap();
    let interrupted = reopened
        .metadata_entries()
        .into_iter()
        .find(|entry| {
            entry.kind() == SessionMetadataKind::TurnTerminal
                && entry.terminal_status() == Some(TurnTerminalStatus::Interrupted)
        })
        .expect("RepairWrite open appended synthetic interrupted terminal");
    assert_eq!(interrupted.turn_id(), Some("turn_1"));
}

#[test]
fn access_open_append_keeps_interrupted_turn_and_appends_under_lock() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
    let session_id = manager.session_id().to_string();
    manager
        .append_metadata(SessionMetadata::turn_started("turn_1"))
        .unwrap();
    let file = manager.path().to_path_buf();
    drop(manager);

    let coordinator = std::sync::Arc::new(WriterLockCoordinator::new(dir.path()));
    let mut opened = SessionManager::open_existing_with_access(
        &file,
        &coordinator,
        &session_id,
        SessionAccess::Append,
    )
    .unwrap();
    assert!(
        opened
            .metadata_entries()
            .into_iter()
            .all(|entry| entry.terminal_status() != Some(TurnTerminalStatus::Interrupted)),
        "Append intent must not repair interrupted turns"
    );
    opened
        .append_metadata(SessionMetadata::thread_name("renamed"))
        .unwrap();
}

#[test]
fn access_open_verifies_header_id_for_both_intents() {
    let dir = tempfile::tempdir().unwrap();
    let manager = SessionManager::create(dir.path(), dir.path()).unwrap();
    let file = manager.path().to_path_buf();
    drop(manager);

    let coordinator = std::sync::Arc::new(WriterLockCoordinator::new(dir.path()));
    for access in [SessionAccess::RepairWrite, SessionAccess::Append] {
        let error =
            SessionManager::open_existing_with_access(&file, &coordinator, "other-id", access)
                .expect_err("header id mismatch must fail closed for both intents");
        assert!(matches!(error, SessionError::InvalidHeader(_)));
    }
}
