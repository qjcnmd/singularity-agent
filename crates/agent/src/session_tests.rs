use super::*;

fn user(text: &str) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::User,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        stop_reason: None,
        provider_reasoning_replay: None,
        tool_call_id: None,
        tool_name: None,
        is_error: None,
    }
}

fn assistant(text: &str) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::Assistant,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        stop_reason: None,
        provider_reasoning_replay: None,
        tool_call_id: None,
        tool_name: None,
        is_error: None,
    }
}

fn tool_result(call_id: &str, text: &str) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::ToolResult,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        stop_reason: None,
        provider_reasoning_replay: None,
        tool_call_id: Some(call_id.to_string()),
        tool_name: Some("bash".to_string()),
        is_error: None,
    }
}

fn entry_ids(entries: &[SessionEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.id.clone()).collect()
}

fn session_header(id: &str) -> String {
    format!(
        r#"{{"type":"session","version":1,"id":"{id}","timestamp":"2026-08-20T00:00:00.000Z","cwd":"C:/work"}}"#
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
    let leaf = manager.entries().last().expect("content entry").id.clone();
    assert_eq!(leaf, id3);

    let content = std::fs::read_to_string(&file).unwrap();
    let first_line: Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert_eq!(first_line["type"], "session");
    assert_eq!(first_line["version"], 1);
    assert_eq!(
        first_line["cwd"],
        normalize_cwd_string(&std::path::absolute(&cwd).unwrap())
    );
    let header_id = first_line["id"].as_str().unwrap();
    let file_name = manager.path().file_name().unwrap().to_str().unwrap();
    assert_eq!(file_name, format!("{header_id}.jsonl"));
    drop(manager);

    let opened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(opened.entries().last().expect("content entry").id, leaf);
    let entries = opened.build_context_entries().unwrap();
    assert_eq!(entry_ids(&entries), vec![id1, id2, id3]);
    for entry in &entries {
        assert_eq!(entry.id.len(), 8);
        assert!(entry.id.chars().all(|c| c.is_ascii_hexdigit()));
    }
    assert!(matches!(&entries[0].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::User && m.content_text() == "hello"));
    assert!(matches!(&entries[1].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::Assistant && m.content_text() == "hi there"));
    assert!(matches!(&entries[2].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::ToolResult
                && m.tool_call_id.as_deref() == Some("call_1")
                && m.tool_name.as_deref() == Some("bash")));
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
        .append_metadata(
            SessionMetadata::thread_settings("openai", "test-model", Some("high".to_string()))
                .unwrap(),
        )
        .unwrap();
    drop(settings_writer);
    let mut turn_worker = SessionManager::open_existing(&file).unwrap();
    let m3 = turn_worker.append_message(user("third")).unwrap();

    // 重开从 JSONL 重建完整线性链。
    let reopened = SessionManager::open_existing(&file).unwrap();
    let entries = reopened.build_context_entries().unwrap();
    assert_eq!(
        entry_ids(&entries),
        vec![m1.clone(), m2.clone(), s1.clone(), m3.clone()]
    );
    // 线性事实源：entries 依次落盘，id 无重复。
    assert_eq!(
        entries.len(),
        reopened.entries.len(),
        "context entries match the full linear file order"
    );
    let ids = entries
        .iter()
        .map(|entry| entry.id.as_str())
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
        .append_metadata(SessionMetadata::turn_completed("turn_1"))
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
        .find(|entry| entry.kind() == SessionMetadataKind::TurnInterrupted)
        .expect("turn_interrupted entry exists");
    assert_eq!(interrupted.turn_id(), Some("turn_2"));
    assert!(interrupted.synthetic());

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
        json!({"metadataType": "turn_completed", "turnId": "turn-1"}),
        json!({"metadataType": "turn_failed", "turnId": "turn-1", "error": "failed"}),
        json!({
            "metadataType": "turn_interrupted",
            "turnId": "turn-1",
            "reason": "cancelled",
            "synthetic": false
        }),
        json!({
            "metadataType": "thread_settings",
            "provider": "openai",
            "model": "test-model",
            "reasoning": "high"
        }),
        json!({"metadataType": "thread_name", "name": "Typed metadata"}),
        json!({
            "metadataType": "usage",
            "turnId": "turn-1",
            "usage": {"totalTokens": 42}
        }),
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

#[test]
fn repair_orphaned_tool_calls_appends_synthetic_failed_result_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
    manager
        .append_message(AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: "orphan_call_1".to_string(),
                name: "bash".to_string(),
                args: json!({"command": "cargo test"}),
            }],
            stop_reason: None,
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
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
        .filter_map(|entry| match &entry.entry_type {
            SessionEntryType::Message(message) if message.role == AgentMessageRole::ToolResult => {
                Some(message)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(
        tool_results[0].tool_call_id.as_deref(),
        Some("orphan_call_1")
    );
    assert!(
        tool_results[0]
            .content_text()
            .contains("previous execution outcome unknown")
    );
    assert!(tool_results[0].content_text().contains("do not retry"));

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

    // 2. 旧版本 v2, v3, v4
    for old_v in [2, 3, 4] {
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
        r#"{"type":"session","version":1,"id":"01914f6b-0000-7000-8000-000000000001","timestamp":"2026-08-20T00:00:00.000Z","cwd":"C:/work","extra":"field"}"#,
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
        r#"{"type":"session","version":1,"id":"not-a-uuid","timestamp":"2026-08-20T00:00:00.000Z","cwd":"C:/work"}"#,
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
            SessionEntryType::Message(user("oversized")),
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
