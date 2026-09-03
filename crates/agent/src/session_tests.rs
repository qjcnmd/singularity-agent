#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
use super::*;
use singularity_protocol::{TurnModelUsage, TurnStatus};

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

fn assistant_with_tool_call(call_id: &str, name: &str) -> AgentMessage {
    AgentMessage::Assistant {
        content: vec![ContentBlock::ToolCall {
            id: call_id.to_string(),
            name: name.to_string(),
            args: json!({"command": "cargo test"}),
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
    }
}

fn run_operation(operation_id: &str, turn_id: &str) -> LedgerRecord {
    LedgerRecord::OperationStarted {
        operation_id: operation_id.to_string(),
        kind: OperationKind::Run,
        turn_id: Some(turn_id.to_string()),
    }
}

fn entry_ids(entries: &[SessionEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.id().to_string()).collect()
}

fn session_header(id: &str) -> String {
    format!(
        r#"{{"type":"session","version":{CURRENT_SESSION_VERSION},"id":"{id}","timestamp":"2026-08-20T00:00:00.000Z","cwd":"C:/work"}}"#
    )
}

fn session_message(id: &str, text: &str) -> String {
    format!(
        r#"{{"type":"message","id":"{id}","timestamp":"2026-08-20T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
    )
}

#[test]
fn create_append_reopen_roundtrip() {
    let fixture = test_support::SessionFixture::new();
    let cwd = fixture.home().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let id = "01914f6b-0000-7000-8000-0000000000aa";
    let mut manager = fixture.create_session(&cwd, id).unwrap();
    assert!(manager.entries().is_empty());

    let id1 = manager.append_message(user("hello")).unwrap();
    let id2 = manager.append_message(assistant("hi there")).unwrap();
    let id3 = manager
        .append_message(tool_result("call_1", "ls output"))
        .unwrap();
    let leaf = manager
        .entries()
        .last()
        .expect("content entry")
        .id()
        .to_string();
    assert_eq!(leaf, id3);

    let content = std::fs::read_to_string(fixture.session_path(id)).unwrap();
    let first_line: Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert_eq!(first_line["type"], "session");
    assert_eq!(first_line["version"], CURRENT_SESSION_VERSION);
    assert_eq!(
        first_line["cwd"],
        normalize_cwd_string(&std::path::absolute(&cwd).unwrap())
    );
    assert_eq!(first_line["id"].as_str().unwrap(), id);
    drop(manager);

    let opened = fixture.open_read_only(id).unwrap();
    assert_eq!(
        opened.entries().last().expect("content entry").id(),
        leaf.as_str()
    );
    let view = context::ContextView::derive(&opened).unwrap();
    assert_eq!(entry_ids(view.entries()), vec![id1, id2, id3]);
    for entry in view.entries() {
        assert_eq!(entry.id().len(), 8);
        assert!(entry.id().chars().all(|c| c.is_ascii_hexdigit()));
    }
    assert!(matches!(&view.entries()[0],
            SessionEntry::Message { message: m, .. } if m.role() == AgentMessageRole::User && m.content_text() == "hello"));
    assert!(matches!(&view.entries()[1],
            SessionEntry::Message { message: m, .. } if m.role() == AgentMessageRole::Assistant && m.content_text() == "hi there"));
    assert!(matches!(&view.entries()[2],
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
    let view = context::ContextView::derive(&reopened).unwrap();
    assert_eq!(entry_ids(view.entries()), vec![m1, m2, s1, m3]);
    assert_eq!(
        view.entries().len(),
        reopened.entries().len(),
        "context entries match the full linear file order"
    );
    let ids = view
        .entries()
        .iter()
        .map(super::format::SessionEntry::id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(ids.len(), view.entries().len(), "entry ids must be unique");
}

/// T016：同一会话同一时刻至多一个存活写者；第二个写者被显式拒绝。
#[test]
fn one_writer_excludes_a_second_concurrent_writer() {
    let fixture = test_support::SessionFixture::new();
    let id = "01914f6b-0000-7000-8000-0000000000aa";
    let first = fixture.create_session(fixture.home(), id).unwrap();
    let error = fixture
        .open_for_repair(id)
        .expect_err("a second writer must be rejected while the first is alive");
    assert!(
        matches!(error, SessionError::WriterConflict { .. }),
        "expected WriterConflict, got {error:?}"
    );
    drop(first);
    // 写者释放后重开成功。
    let reopened = fixture.open_for_repair(id).unwrap();
    assert!(reopened.entries().is_empty());
}

/// T016：operation 的终态事实先于任何终态投影 durable——`operation_finished`
/// 落盘成功后条目才可见；未落盘时重开看不到终态。
#[test]
fn terminal_record_is_durable_before_visibility() {
    let fixture = test_support::SessionFixture::new();
    let id = "01914f6b-0000-7000-8000-0000000000ab";
    let mut manager = fixture.create_session(fixture.home(), id).unwrap();
    manager
        .append_record(run_operation("op-1", "turn-1"))
        .unwrap();
    // 未终结：重开（只读）看到 open run，投影为 running。
    drop(manager);
    let reopened = fixture.open_read_only(id).unwrap();
    let operations = reduce_operations(reopened.entries());
    assert_eq!(
        open_operations(&operations).len(),
        1,
        "run is open before finish"
    );
    drop(reopened);

    let mut manager = SessionManager::open_existing(&fixture.session_path(id)).unwrap();
    manager
        .append_record(LedgerRecord::OperationFinished {
            operation_id: "op-1".to_string(),
            turn_id: Some("turn-1".to_string()),
            outcome: TurnStatus::Completed,
            usage: Some(TurnModelUsage {
                total_tokens: 42,
                usage_present: true,
                usage_complete: true,
                ..TurnModelUsage::default()
            }),
            truncated: false,
        })
        .unwrap();
    drop(manager);

    let reopened = fixture.open_read_only(id).unwrap();
    let operations = reduce_operations(reopened.entries());
    assert!(
        open_operations(&operations).is_empty(),
        "run finished durably"
    );
    assert_eq!(operations[0].finished, Some(TurnStatus::Completed));
    let terminal_usage = reopened
        .ledger_records()
        .into_iter()
        .find_map(|record| match record {
            LedgerRecord::OperationFinished {
                outcome: TurnStatus::Completed,
                usage: Some(usage),
                ..
            } => Some(usage),
            _ => None,
        })
        .expect("durable completed terminal carries usage");
    assert_eq!(terminal_usage.total_tokens, 42);
}

/// T017：崩溃遗留的未终结 run 在重开时被收敛为 interrupted，未解决工具补
/// synthetic failed 结果，且修复幂等（第二次打开不再改动）。
#[test]
fn reopen_interrupted_operation_repair_is_idempotent_and_synthetic() {
    let fixture = test_support::SessionFixture::new();
    let id = "01914f6b-0000-7000-8000-0000000000ac";
    let mut manager = fixture.create_session(fixture.home(), id).unwrap();
    manager
        .append_record(run_operation("op-1", "turn-1"))
        .unwrap();
    manager
        .append_record(LedgerRecord::OperationFinished {
            operation_id: "op-1".to_string(),
            turn_id: Some("turn-1".to_string()),
            outcome: TurnStatus::Completed,
            usage: Some(TurnModelUsage::default()),
            truncated: false,
        })
        .unwrap();
    manager
        .append_record(run_operation("op-2", "turn-2"))
        .unwrap();
    drop(manager);

    let mut reopened = SessionManager::open_existing(&fixture.session_path(id)).unwrap();
    assert_eq!(reopened.repair_interrupted_operations().unwrap(), 1);
    drop(reopened);

    let reopened = fixture.open_read_only(id).unwrap();
    let operations = reduce_operations(reopened.entries());
    assert!(
        open_operations(&operations).is_empty(),
        "all runs converged"
    );
    let open_turn2 = operations
        .iter()
        .find(|operation| operation.operation_id == "op-2")
        .expect("op-2 present");
    assert_eq!(open_turn2.finished, Some(TurnStatus::Interrupted));
    drop(reopened);

    let mut reopened = SessionManager::open_existing(&fixture.session_path(id)).unwrap();
    assert_eq!(reopened.repair_interrupted_operations().unwrap(), 0);
}

/// 恢复未完成工具调用：崩溃恢复只补模型可见失败并终结 operation，不产生任何新的执行事实。
#[test]
fn recovery_resolves_uncompleted_tool_calls_with_synthetic_error() {
    let fixture = test_support::SessionFixture::new();
    let id = "01914f6b-0000-7000-8000-0000000000ad";
    let mut manager = fixture.create_session(fixture.home(), id).unwrap();
    manager
        .append_record(run_operation("op-1", "turn-1"))
        .unwrap();
    manager
        .append_message(assistant_with_tool_call("call-1", "write"))
        .unwrap();
    let entries_before = manager.entries().len();
    drop(manager);

    let mut reopened = SessionManager::open_existing(&fixture.session_path(id)).unwrap();
    assert_eq!(reopened.repair_interrupted_operations().unwrap(), 1);
    let appended = &reopened.entries()[entries_before..];
    let synthetic_results = appended
        .iter()
        .filter(|entry| matches!(entry, SessionEntry::Message { message, .. } if message.role() == AgentMessageRole::ToolResult))
        .count();
    assert_eq!(synthetic_results, 1, "exactly one synthetic tool result");
    assert!(
        appended.iter().any(|entry| matches!(
            entry,
            SessionEntry::Record {
                record: LedgerRecord::OperationFinished {
                    outcome: TurnStatus::Interrupted,
                    ..
                },
                ..
            }
        )),
        "open run converges to interrupted"
    );
    let result_text = appended
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Message { message, .. }
                if message.role() == AgentMessageRole::ToolResult =>
            {
                Some(message.content_text())
            }
            _ => None,
        })
        .unwrap();
    assert!(result_text.contains("do not retry"), "{result_text}");
}

/// 归约把已配对的 tool_call 与 tool_result 视为解决，不产生未解决工具。
#[test]
fn reduction_pairs_tool_calls_with_persisted_results() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let mut manager = SessionManager::create(dir.path(), &sessions).unwrap();
    manager
        .append_record(run_operation("op-1", "turn-1"))
        .unwrap();
    manager
        .append_message(assistant_with_tool_call("call-1", "bash"))
        .unwrap();
    manager
        .append_message_with_id("res-1", tool_result("call-1", "ok"))
        .unwrap();
    let operations = reduce_operations(manager.entries());
    assert_eq!(operations[0].open_tools.len(), 0, "tool call is paired");
}

/// 归约只折叠事实：每个未终结 operation 各自被修复收敛。
#[test]
fn repair_converges_every_open_operation() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let mut manager = SessionManager::create(dir.path(), &sessions).unwrap();
    manager
        .append_record(run_operation("op-1", "turn-1"))
        .unwrap();
    manager
        .append_record(run_operation("op-2", "turn-2"))
        .unwrap();
    assert_eq!(
        manager.repair_interrupted_operations().unwrap(),
        2,
        "every open operation converges"
    );
    let operations = reduce_operations(manager.entries());
    assert!(
        open_operations(&operations).is_empty(),
        "both runs carry terminal records"
    );
}

/// usage 的形状是封闭的：七个键全部必填、只认 camelCase。
#[test]
fn terminal_usage_shape_is_closed() {
    let complete_usage = json!({
        "inputTokens": 0,
        "outputTokens": 0,
        "totalTokens": 42,
        "cachedInputTokens": 0,
        "reasoningTokens": 0,
        "usagePresent": true,
        "usageComplete": true
    });
    let mut missing_key = complete_usage.clone();
    missing_key
        .as_object_mut()
        .expect("usage object")
        .remove("usagePresent");
    let mut other_casing = complete_usage;
    {
        let object = other_casing.as_object_mut().expect("usage object");
        object.remove("inputTokens");
        object.insert("input_tokens".to_string(), json!(0));
    }
    for usage in [missing_key, other_casing] {
        let record = json!({
            "recordType": "operation_finished",
            "operationId": "op-1",
            "turnId": "turn-1",
            "outcome": "completed",
            "usage": usage
        });
        assert!(
            serde_json::from_value::<LedgerRecord>(record.clone()).is_err(),
            "{record} must not read as a terminal record"
        );
    }
}

/// v4 格式契约：条目与记录载荷的未知字段一律拒绝。
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
            "unknownField": 1,
            "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}
        }),
        json!({
            "type": "compaction",
            "id": "c-1",
            "compaction": {"summary": "s", "unknown": 1}
        }),
        json!({
            "type": "metadata",
            "id": "md-1",
            "metadata": {"metadataType": "thread_name", "name": "n", "unknown": 1}
        }),
        json!({
            "type": "record",
            "id": "r-1",
            "record": {"recordType": "operation_finished", "operationId": "op", "outcome": "completed", "usage": null, "extra": 1}
        }),
        json!({
            "type": "record",
            "id": "r-1",
            "record": {"recordType": "operation_started", "operationId": "op", "kind": "run", "unknown": true}
        }),
    ];
    for value in cases {
        assert!(
            serde_json::from_value::<SessionEntry>(value.clone()).is_err(),
            "unknown fields must be rejected: {value}"
        );
    }
}

/// v4 格式契约：完整 round-trip（含 operation 记录与嵌套载荷）。
#[test]
fn v4_format_round_trips_nested_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let message_id = manager.append_message(assistant("hello")).unwrap();
    let compaction_id = manager
        .append_compaction_with_id(
            "c-1",
            CompactionEntry {
                summary: "compacted".to_string(),
                first_kept_entry_id: "m-1".to_string(),
                usage: None,
                details: None,
            },
        )
        .unwrap();
    let record_id = manager
        .append_record(LedgerRecord::OperationFinished {
            operation_id: "op-1".to_string(),
            turn_id: Some("turn-1".to_string()),
            outcome: TurnStatus::Completed,
            usage: Some(TurnModelUsage {
                total_tokens: 7,
                usage_present: true,
                usage_complete: true,
                ..TurnModelUsage::default()
            }),
            truncated: true,
        })
        .unwrap();
    let file = manager.path().to_path_buf();
    drop(manager);

    let reopened = SessionManager::open_existing(&file).unwrap();
    let entries = reopened.entries();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].id(), message_id);
    assert_eq!(entries[1].id(), compaction_id);
    assert_eq!(entries[2].id(), record_id);
    assert!(matches!(
        &entries[2],
        SessionEntry::Record {
            record:
                LedgerRecord::OperationFinished {
                    turn_id,
                    outcome: TurnStatus::Completed,
                    truncated: true,
                    ..
                },
            ..
        } if turn_id.as_deref() == Some("turn-1")
    ));
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
    assert_eq!(
        context::ContextView::derive(&opened)
            .unwrap()
            .entries()
            .len(),
        1
    );
    assert!(std::fs::read(&file).unwrap().ends_with(b"\n"));
    drop(opened);
    let mut reopened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(
        context::ContextView::derive(&reopened)
            .unwrap()
            .entries()
            .len(),
        1
    );
    reopened.append_message(user("after repair")).unwrap();
    drop(reopened);
    let reopened_again = SessionManager::open_existing(&file).unwrap();
    assert_eq!(
        context::ContextView::derive(&reopened_again)
            .unwrap()
            .entries()
            .len(),
        2
    );

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
    assert_eq!(
        context::ContextView::derive(&opened)
            .unwrap()
            .entries()
            .len(),
        1
    );
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

    // 2. header version 必须等于当前版本（v5），v4 及更早一律拒绝，无迁移桥。
    for old_v in [1, 2, 3, 4, 6] {
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
        r#"{"type":"session","version":5,"id":"01914f6b-0000-7000-8000-000000000001","timestamp":"2026-08-20T00:00:00.000Z","cwd":"C:/work","extra":"field"}"#,
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
        r#"{"type":"session","version":5,"id":"not-a-uuid","timestamp":"2026-08-20T00:00:00.000Z","cwd":"C:/work"}"#,
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
    let before = context::ContextView::derive(&manager).unwrap();
    manager.file = dir.path().to_path_buf();
    assert!(manager.append_message(user("must fail")).is_err());
    assert_eq!(
        entry_ids(before.entries()),
        entry_ids(context::ContextView::derive(&manager).unwrap().entries())
    );
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
                timestamp: "2026-08-20T00:00:00.000Z".to_string(),
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
fn access_open_repair_write_repairs_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
    let session_id = manager.session_id().to_string();
    manager
        .append_record(run_operation("op-1", "turn_1"))
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
    let operations = reduce_operations(reopened.entries());
    assert!(open_operations(&operations).is_empty());
    assert_eq!(operations[0].finished, Some(TurnStatus::Interrupted));
    assert_eq!(operations[0].turn_id.as_deref(), Some("turn_1"));
}

#[test]
fn access_open_append_keeps_interrupted_operation_and_appends_under_lock() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
    let session_id = manager.session_id().to_string();
    manager
        .append_record(run_operation("op-1", "turn_1"))
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
    let operations = reduce_operations(opened.entries());
    assert_eq!(
        open_operations(&operations).len(),
        1,
        "Append intent must not repair interrupted operations"
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
        assert!(error.to_string().contains("other-id"));
    }
}

// --- JSONL 字节级 round-trip 夹具 -------------------------------------------
//
// 这些夹具固定会话线的 wire 形状（键名、camelCase、skip-if-none 行为、枚举
// 词形）。任何对 `AgentMessage`/`SessionEntry`/`LedgerRecord` 的序列化改动
// 都必须先跑本测试：一个键的形状改变即意味着格式破坏。

/// 逐行断言：给定完整会话文件字节，逐条 entry 反向 round-trip 后与原始行
/// 按键集合一致（不含尾随换行）。
fn assert_lines_round_trip(file_bytes: &[u8]) {
    let text = String::from_utf8(file_bytes.to_vec()).expect("fixture is UTF-8");
    let lines = text.lines().collect::<Vec<_>>();
    assert!(!lines.is_empty(), "fixture must have a header");
    let first: serde_json::Value = serde_json::from_str(lines[0]).expect("header parses");
    assert_eq!(first["type"], "session");
    assert_eq!(first["version"], CURRENT_SESSION_VERSION);
    for line in lines.iter().skip(1) {
        let entry: SessionEntry = serde_json::from_str(line).expect("entry parses");
        let rewritten = serde_json::to_string(&entry).expect("entry serializes");
        let original: serde_json::Value = serde_json::from_str(line).expect("fixture parses");
        let round_tripped: serde_json::Value =
            serde_json::from_str(&rewritten).expect("round-trip parses");
        assert_eq!(
            round_tripped, original,
            "JSONL entry must round-trip without key-set drift"
        );
    }
}

/// 完整会话夹具：header + operation 记录（started/control/finished）+
/// user/assistant/toolResult + compaction + thread settings/name。
const COMPLETE_SESSION: &str = r###"{"cwd":"C:/work","id":"01914f6b-0000-7000-8000-0000000000e1","timestamp":"2026-08-20T00:00:00.000Z","type":"session","version":5}
{"type":"record","id":"r-op-start","timestamp":"2026-08-20T00:00:00.500Z","record":{"recordType":"operation_started","operationId":"op-1","kind":"run","turnId":"turn-1"}}
{"type":"message","id":"m-user-1","timestamp":"2026-08-20T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}
{"type":"message","id":"m-assistant-1","timestamp":"2026-08-20T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"reasoning trace"},{"type":"text","text":"analysis"},{"type":"tool_call","id":"call-1","name":"bash","args":{"command":"cargo test"}}],"stopReason":"stop"}}
{"type":"message","id":"m-tr-1","timestamp":"2026-08-20T00:00:03.000Z","message":{"role":"toolResult","content":[{"type":"text","text":"ok"}],"toolCallId":"call-1","toolName":"bash","isError":false}}
{"type":"record","id":"r-ctl-1","timestamp":"2026-08-20T00:00:03.200Z","record":{"recordType":"control_accepted","controlId":"ctl-1","turnId":"turn-1","channel":"steer","sequence":1,"disposition":"injected","text":"go left"}}
{"type":"compaction","id":"c-1","timestamp":"2026-08-20T00:00:05.000Z","compaction":{"summary":"## Goal\ncompacted history","firstKeptEntryId":"m-user-1","usage":{"inputTokens":100,"outputTokens":50,"totalTokens":150,"cachedInputTokens":10,"reasoningTokens":0,"usagePresent":true,"usageComplete":true},"details":{"cut":"from_entry"}}}
{"type":"record","id":"r-op-finish","timestamp":"2026-08-20T00:00:06.000Z","record":{"recordType":"operation_finished","operationId":"op-1","turnId":"turn-1","outcome":"completed","usage":{"inputTokens":0,"outputTokens":0,"totalTokens":0,"cachedInputTokens":0,"reasoningTokens":0,"usagePresent":false,"usageComplete":false},"truncated":true}}
{"type":"metadata","id":"md-2","timestamp":"2026-08-20T00:00:07.000Z","metadata":{"metadataType":"thread_settings","provider":"openai_compatible","model":"test-model-a","reasoning":"high"}}
{"type":"metadata","id":"md-3","timestamp":"2026-08-20T00:00:08.000Z","metadata":{"metadataType":"thread_name","name":"typed metadata"}}"###;

#[test]
fn jsonl_wire_round_trip_fixtures_cover_all_entry_shapes() {
    assert_lines_round_trip(COMPLETE_SESSION.as_bytes());
}

/// 投影：operation 记录驱动 turn 计数、终态与 usage 累计。
#[test]
fn project_session_derives_thread_facts_from_operation_records() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let mut manager = SessionManager::create(dir.path(), &sessions).unwrap();
    manager
        .append_record(run_operation("op-1", "turn-1"))
        .unwrap();
    manager.append_message(user("hello")).unwrap();
    manager
        .append_record(LedgerRecord::OperationFinished {
            operation_id: "op-1".to_string(),
            turn_id: Some("turn-1".to_string()),
            outcome: TurnStatus::Completed,
            usage: Some(TurnModelUsage {
                total_tokens: 42,
                usage_present: true,
                usage_complete: true,
                ..TurnModelUsage::default()
            }),
            truncated: false,
        })
        .unwrap();
    let summary = project_session(&manager, false);
    assert_eq!(summary.turn_count, 1);
    assert_eq!(summary.status, Some(TurnStatus::Completed));
    assert_eq!(summary.total_tokens, 42);
}
