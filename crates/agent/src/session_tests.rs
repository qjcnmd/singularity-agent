use super::*;
use singularity_model::{ModelRole, ProviderReasoningReplay};

fn user(text: &str) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::User,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        provider_reasoning_replay: None,
        tool_call_id: None,
        tool_name: None,
        is_error: None,
        timestamp: Some(1_700_000_000_000),
    }
}

fn assistant(text: &str) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::Assistant,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        provider_reasoning_replay: None,
        tool_call_id: None,
        tool_name: None,
        is_error: None,
        timestamp: Some(1_700_000_000_001),
    }
}

fn tool_result(call_id: &str, text: &str) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::ToolResult,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        provider_reasoning_replay: None,
        tool_call_id: Some(call_id.to_string()),
        tool_name: Some("bash".to_string()),
        is_error: None,
        timestamp: Some(1_700_000_000_002),
    }
}

fn compaction(summary: &str, first_kept_entry_id: Option<String>) -> CompactionEntry {
    CompactionEntry {
        summary: summary.to_string(),
        first_kept_entry_id,
        tokens_before: Some(100),
        previous_summary: None,
        details: None,
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

fn session_message(id: &str, parent: Option<&str>, text: &str) -> String {
    let parent = parent.map_or_else(|| "null".to_string(), |value| format!("\"{value}\""));
    format!(
        r#"{{"type":"message","id":"{id}","parentId":{parent},"timestamp":"2026-08-20T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
    )
}

#[test]
fn create_append_reopen_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let cwd = dir.path().join("project");
    let mut manager = SessionManager::create(&cwd, &sessions).unwrap();
    assert!(manager.leaf_id().is_empty());

    let id1 = manager.append_message(user("hello")).unwrap();
    let id2 = manager.append_message(assistant("hi there")).unwrap();
    let id3 = manager
        .append_message(tool_result("call_1", "ls output"))
        .unwrap();
    let file = manager.path().to_path_buf();
    let leaf = manager.leaf_id().to_string();
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

    let opened = SessionManager::open(&file).unwrap();
    assert_eq!(opened.leaf_id(), leaf);
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
    let result = SessionManager::open(&file);
    assert!(
        matches!(result, Err(SessionError::InvalidSession(_))),
        "empty session file must fail closed"
    );
}

#[test]
fn build_context_entries_compaction_slicing() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let _m1 = manager.append_message(user("1")).unwrap();
    let _m2 = manager.append_message(assistant("2")).unwrap();
    let m3 = manager.append_message(user("3")).unwrap();
    let m4 = manager.append_message(assistant("4")).unwrap();

    let c1 = manager
        .append_compaction(compaction("summary of 1,2", Some(m3.clone())))
        .unwrap();
    let m5 = manager.append_message(user("5")).unwrap();

    let context = manager.build_context_entries().unwrap();
    assert_eq!(entry_ids(&context), vec![c1.clone(), m3, m4, m5.clone()]);

    let c2 = manager
        .append_compaction(compaction("summary of 3,4,5", Some(m5.clone())))
        .unwrap();
    let m6 = manager.append_message(assistant("6")).unwrap();
    let context = manager.build_context_entries().unwrap();
    assert_eq!(entry_ids(&context), vec![c2, m5, m6]);
}

#[test]
fn separate_session_managers_follow_the_latest_durable_leaf() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let cwd = dir.path().join("project");
    let mut turn_worker = SessionManager::create(&cwd, &sessions).unwrap();
    let m1 = turn_worker.append_message(user("first")).unwrap();
    let m2 = turn_worker.append_message(assistant("second")).unwrap();

    let mut settings_writer = SessionManager::open(turn_worker.path()).unwrap();
    let s1 = settings_writer
        .append_metadata(
            SessionMetadata::thread_settings("openai", "gpt-4o", Some("high".to_string())).unwrap(),
        )
        .unwrap();
    let m3 = turn_worker.append_message(user("third")).unwrap();

    let reopened = SessionManager::open(turn_worker.path()).unwrap();
    let entries = reopened.build_context_entries().unwrap();
    assert_eq!(
        entry_ids(&entries),
        vec![m1.clone(), m2.clone(), s1.clone(), m3.clone()]
    );
    assert_eq!(reopened.entries[reopened.by_id[&s1]].parent_id, m2);
    assert_eq!(reopened.entries[reopened.by_id[&m3]].parent_id, s1);
}

#[test]
fn build_session_context_replays_assistant_tool_calls() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
    manager
        .append_message(AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: "call_1".to_string(),
                name: "write".to_string(),
                args: serde_json::json!({
                    "path": "hello.txt",
                    "content": "hello",
                }),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        })
        .unwrap();
    manager
        .append_message(tool_result(
            "call_1",
            "Successfully wrote 5 bytes to hello.txt",
        ))
        .unwrap();

    let file = manager.path().to_path_buf();
    drop(manager);
    let manager = SessionManager::open(&file).unwrap();
    let ctx = manager.build_session_context().unwrap();
    assert_eq!(ctx.messages.len(), 2);
    assert_eq!(ctx.messages[0].role, ModelRole::Assistant);
    assert_eq!(ctx.messages[0].content, "");
    assert_eq!(ctx.messages[0].tool_calls.len(), 1);
    let call = &ctx.messages[0].tool_calls[0];
    assert_eq!(call.tool_call_id, "call_1");
    assert_eq!(call.tool_name, "write");
    assert_eq!(call.parse_status, ModelToolParseStatus::Valid);
    assert!(call.validation_errors.is_empty());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&call.raw_arguments).unwrap(),
        serde_json::json!({ "path": "hello.txt", "content": "hello" })
    );
    assert_eq!(ctx.messages[1].role, ModelRole::Tool);
    assert_eq!(ctx.messages[1].tool_call_id.as_deref(), Some("call_1"));
}

#[test]
fn repository_read_is_bounded_and_filtered() {
    let dir = tempfile::tempdir().unwrap();
    let repo = SessionRepository::new(dir.path());
    let mut manager = SessionManager::create_with_id(
        dir.path(),
        dir.path(),
        "a1b2c3d4-e5f6-7a8b-9c0d-1e2f3a4b5c6d",
    )
    .unwrap();
    let m1 = manager.append_message(user("one")).unwrap();
    let _c1 = manager.append_compaction(compaction("sum1", None)).unwrap();
    let m2 = manager.append_message(assistant("two")).unwrap();
    let _s1 = manager
        .append_metadata(SessionMetadata::turn_completed("turn-1"))
        .unwrap();
    let m3 = manager.append_message(user("three")).unwrap();
    drop(manager);

    let read_all = repo
        .read(
            "a1b2c3d4-e5f6-7a8b-9c0d-1e2f3a4b5c6d",
            &SessionReadOptions {
                recent_limit: 2,
                filter: SessionEntryFilter::All,
                range: None,
            },
        )
        .unwrap();
    assert_eq!(read_all.total_entries, 5);
    assert_eq!(read_all.summary.as_deref(), Some("sum1"));
    assert_eq!(read_all.entries.len(), 2);

    let read_messages = repo
        .read(
            "a1b2c3d4-e5f6-7a8b-9c0d-1e2f3a4b5c6d",
            &SessionReadOptions {
                recent_limit: 10,
                filter: SessionEntryFilter::Messages,
                range: Some((1, 3)),
            },
        )
        .unwrap();
    assert_eq!(entry_ids(&read_messages.entries), vec![m2, m3]);

    let read_first = repo
        .read(
            "a1b2c3d4-e5f6-7a8b-9c0d-1e2f3a4b5c6d",
            &SessionReadOptions {
                recent_limit: 1,
                filter: SessionEntryFilter::Messages,
                range: Some((0, 1)),
            },
        )
        .unwrap();
    assert_eq!(entry_ids(&read_first.entries), vec![m1]);
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
    let bad_fields = [
        ("apiKey", json!("secret")),
        ("authorization", json!("Bearer xyz")),
        ("auth_token", json!("token")),
        ("password", json!("pw")),
    ];
    for (key, val) in bad_fields {
        let mut map = Map::new();
        map.insert(key.to_string(), val);
        assert!(
            SessionMetadata::new(SessionMetadataKind::ThreadSettings, map).is_err(),
            "{key} must be rejected"
        );
    }
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
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
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
fn build_session_context_model_from_thread_settings() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir
        .path()
        .join("01914f6b-0000-7000-8000-000000000001.jsonl");
    let lines = [
        r#"{"type":"session","version":1,"id":"01914f6b-0000-7000-8000-000000000001","timestamp":"2026-08-20T00:00:00.000Z","cwd":"C:/work"}"#,
        r#"{"type":"message","id":"aaaa1111","parentId":null,"timestamp":"2026-08-20T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#,
        r#"{"type":"metadata","id":"bbbb2222","parentId":"aaaa1111","timestamp":"2026-08-20T00:00:02.000Z","metadataType":"thread_settings","provider":"openai","model":"gpt-4o"}"#,
        r#"{"type":"message","id":"dddd4444","parentId":"bbbb2222","timestamp":"2026-08-20T00:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"reply"}]}}"#,
    ];
    std::fs::write(&file, lines.join("\n")).unwrap();

    let manager = SessionManager::open(&file).unwrap();
    let ctx = manager.build_session_context().unwrap();
    assert_eq!(ctx.model.as_deref(), Some("openai/gpt-4o"));
    let roles: Vec<ModelRole> = ctx.messages.iter().map(|m| m.role.clone()).collect();
    assert_eq!(roles, vec![ModelRole::User, ModelRole::Assistant]);
    assert_eq!(ctx.messages[0].content, "hello");
    assert_eq!(ctx.messages[1].content, "reply");
}

#[test]
fn responses_private_replay_round_trips_exactly_through_jsonl() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let cwd = dir.path().join("project");
    let replay = ProviderReasoningReplay::Responses {
        provider_name: "provider".to_string(),
        model_name: "model".to_string(),
        reasoning_effort: "high".to_string(),
        tool_call_ids: vec!["call_1".to_string()],
        items: vec![
            json!({
                "type": "reasoning",
                "id": "rs_1",
                "encrypted_content": "opaque-secret"
            }),
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "write",
                "arguments": "{\"path\":\"out.txt\"}"
            }),
        ],
    };
    let mut manager =
        SessionManager::create_with_id(&cwd, &sessions, "f7be8e2a-6f7f-4b55-b1cf-ef6d00c4d8f8")
            .unwrap();
    manager
        .append_message(AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "summary projection".to_string(),
                    signature: None,
                },
                ContentBlock::ToolCall {
                    id: "call_1".to_string(),
                    name: "write".to_string(),
                    args: json!({"path": "out.txt"}),
                },
            ],
            provider_reasoning_replay: Some(replay.clone()),
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        })
        .unwrap();
    let path = manager.path().to_path_buf();
    drop(manager);

    let reopened = SessionManager::open_existing(&path).unwrap();
    let entries = reopened.build_context_entries().unwrap();
    let message = match &entries[0].entry_type {
        SessionEntryType::Message(message) => message,
        other => panic!("expected assistant message, got {other:?}"),
    };
    assert_eq!(message.provider_reasoning_replay.as_ref(), Some(&replay));
    assert_eq!(message.thinking_blocks().len(), 1);
    let debug = format!("{message:?}");
    assert!(!debug.contains("opaque-secret"));
    assert!(debug.contains("output_item_count"));
    let wire = serde_json::to_value(message).unwrap();
    assert_eq!(
        wire["providerReasoningReplay"]["items"][0]["encrypted_content"],
        "opaque-secret"
    );
    assert_eq!(
        reopened.build_session_context().unwrap().messages[0].content,
        ""
    );
}

#[test]
fn strict_open_rejects_intermediate_malformed_json() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir
        .path()
        .join("01914f6b-0000-7000-8000-000000000001.jsonl");
    let content = format!(
        "{}\n{}\nnot-json\n{}\n",
        session_header("01914f6b-0000-7000-8000-000000000001"),
        session_message("entry-1", None, "one"),
        session_message("entry-2", Some("entry-1"), "two"),
    );
    std::fs::write(&file, content).unwrap();
    let error =
        SessionManager::open_existing(&file).expect_err("malformed middle line must be rejected");
    assert!(matches!(error, SessionError::MalformedLine { line: 3, .. }));
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
        session_message("entry-1", None, "one")
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
            session_message("entry-1", None, "one")
        ),
    )
    .unwrap();
    let opened = SessionManager::open_existing(&missing_newline).unwrap();
    assert_eq!(opened.build_context_entries().unwrap().len(), 1);
    assert!(std::fs::read(&missing_newline).unwrap().ends_with(b"\n"));
}

#[test]
fn strict_open_rejects_duplicate_missing_parent_and_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (
            "01914f6b-0000-7000-8000-000000000001",
            format!(
                "{}\n{}\n{}\n",
                session_header("01914f6b-0000-7000-8000-000000000001"),
                session_message("same", None, "one"),
                session_message("same", Some("same"), "two")
            ),
            "duplicate",
        ),
        (
            "01914f6b-0000-7000-8000-000000000002",
            format!(
                "{}\n{}\n",
                session_header("01914f6b-0000-7000-8000-000000000002"),
                session_message("entry-1", Some("no-parent"), "one")
            ),
            "missing parent",
        ),
        (
            "01914f6b-0000-7000-8000-000000000003",
            format!(
                "{}\n{}\n{}\n",
                session_header("01914f6b-0000-7000-8000-000000000003"),
                session_message("a", Some("b"), "one"),
                session_message("b", Some("a"), "two")
            ),
            "cycle",
        ),
    ];
    for (id, content, expected) in cases {
        let file = dir.path().join(format!("{id}.jsonl"));
        std::fs::write(&file, content).unwrap();
        let error = SessionManager::open_existing(&file)
            .expect_err("invalid session structure must be rejected");
        assert!(error.to_string().contains(expected), "{id}: {error}");
    }
}

#[test]
fn strict_open_rejects_complete_invalid_final_json() {
    let dir = tempfile::tempdir().unwrap();
    let id = "01914f6b-0000-7000-8000-000000000001";
    let file = dir.path().join(format!("{id}.jsonl"));
    std::fs::write(&file, format!("{}\n[]", session_header(id))).unwrap();
    let error = SessionManager::open_existing(&file)
        .expect_err("complete invalid final entry must be rejected");
    assert!(matches!(error, SessionError::InvalidEntry { line: 2, .. }));
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
fn strict_open_rejects_unknown_entry_types_and_roles() {
    let dir = tempfile::tempdir().unwrap();
    let header = session_header("01914f6b-0000-7000-8000-000000000001");

    // 1. 未知 entry 类型 (custom, label 等)
    for unknown_type in [
        "custom",
        "label",
        "model_change",
        "thinking_level_change",
        "hookMessage",
    ] {
        let f = dir.path().join(format!("{unknown_type}.jsonl"));
        let line = format!(
            "{header}\n{{\"type\":\"{unknown_type}\",\"id\":\"e1\",\"parentId\":null,\"timestamp\":\"2026-08-20T00:00:01.000Z\"}}\n"
        );
        std::fs::write(&f, line).unwrap();
        assert!(matches!(
            SessionManager::open_existing(&f).unwrap_err(),
            SessionError::InvalidEntry { line: 2, .. }
        ));
    }

    // 2. 未知 message role (bashExecution, custom, hookMessage 等)
    for unknown_role in [
        "bashExecution",
        "custom",
        "hookMessage",
        "compactionSummary",
        "system",
    ] {
        let f = dir.path().join(format!("role-{unknown_role}.jsonl"));
        let line = format!(
            "{header}\n{{\"type\":\"message\",\"id\":\"e1\",\"parentId\":null,\"timestamp\":\"2026-08-20T00:00:01.000Z\",\"message\":{{\"role\":\"{unknown_role}\",\"content\":[{{\"type\":\"text\",\"text\":\"hi\"}}]}}}}\n"
        );
        std::fs::write(&f, line).unwrap();
        assert!(matches!(
            SessionManager::open_existing(&f).unwrap_err(),
            SessionError::InvalidEntry { line: 2, .. }
        ));
    }

    // 3. 缺失 timestamp 的 entry
    let no_ts = dir.path().join("no-ts.jsonl");
    let line = format!(
        "{header}\n{{\"type\":\"message\",\"id\":\"e1\",\"parentId\":null,\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"hi\"}}]}}}}\n"
    );
    std::fs::write(&no_ts, line).unwrap();
    assert!(matches!(
        SessionManager::open_existing(&no_ts).unwrap_err(),
        SessionError::InvalidEntry { line: 2, .. }
    ));

    // 4. entry 中包含未知 envelope 字段
    let extra_env = dir.path().join("extra-env.jsonl");
    let line = format!(
        "{header}\n{{\"type\":\"message\",\"id\":\"e1\",\"parentId\":null,\"timestamp\":\"2026-08-20T00:00:01.000Z\",\"unknownEnvelopeField\":true,\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"hi\"}}]}}}}\n"
    );
    std::fs::write(&extra_env, line).unwrap();
    assert!(matches!(
        SessionManager::open_existing(&extra_env).unwrap_err(),
        SessionError::InvalidEntry { line: 2, .. }
    ));
}

#[test]
fn bounded_session_read_checks_file_metadata_before_parsing_body() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("oversized.jsonl");
    std::fs::write(&file, "not-json").unwrap();
    let error = match parse_session_lines_with_limits(&file, 1, 1024, 10) {
        Ok(_) => panic!("metadata limit must reject before body parsing"),
        Err(error) => error,
    };
    assert!(matches!(error, SessionError::InvalidSession(message) if message.contains("1 bytes")));
}

#[test]
fn bounded_session_read_rejects_an_oversized_line_without_unbounded_growth() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("oversized-line.jsonl");
    let header = session_header("01914f6b-0000-7000-8000-000000000001");
    let line_limit = header.len();
    std::fs::write(&file, format!("{header}\n{}\n", "x".repeat(line_limit + 1))).unwrap();
    let error = match parse_session_lines_with_limits(&file, 1024, line_limit, 10) {
        Ok(_) => panic!("line limit must fail closed"),
        Err(error) => error,
    };
    assert!(matches!(error, SessionError::InvalidSession(message) if message.contains("line 2")));
}

#[test]
fn bounded_session_read_counts_content_entries_without_counting_header() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("entry-boundary.jsonl");
    let content = format!(
        "{}\n{}\n{}\n",
        session_header("01914f6b-0000-7000-8000-000000000001"),
        session_message("entry-1", None, "one"),
        session_message("entry-2", Some("entry-1"), "two")
    );
    std::fs::write(&file, content).unwrap();
    assert!(parse_session_lines_with_limits(&file, 1024, 1024, 2).is_ok());
    let error = match parse_session_lines_with_limits(&file, 1024, 1024, 1) {
        Ok(_) => panic!("second content entry must exceed a one-entry content limit"),
        Err(error) => error,
    };
    assert!(
        matches!(error, SessionError::InvalidSession(message) if message.contains("1 entries"))
    );
}

#[test]
fn append_io_failure_does_not_advance_memory() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let before = manager.build_context_entries().unwrap();
    manager.file = dir.path().to_path_buf();
    assert!(manager.append_message(user("must fail")).is_err());
    assert_eq!(manager.build_context_entries().unwrap(), before);
    assert!(manager.leaf_id().is_empty());
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
    assert!(manager.leaf_id().is_empty());
}
