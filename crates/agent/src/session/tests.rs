#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
use super::test_support::SessionFixture;
use super::*;
use crate::message::{AgentMessage, ContentBlock};
use serde_json::{Value, json};
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

/// 请求身份只由外层观测承载：新写入的 context 不再输出重复 id，旧日志里的
/// 该键仍在反序列化边界被接收并丢弃，结构体的严格校验不被放宽。
#[test]
fn request_context_never_stores_a_duplicate_request_identity() {
    let legacy: super::request::RequestContext = serde_json::from_value(json!({
        "request_id": "req-1",
        "definitions": "def-1",
        "model_preferences": {"maxOutputTokens": 1024}
    }))
    .expect("a legacy context with a duplicate id still reads");
    assert_eq!(legacy.definitions, "def-1");
    assert_eq!(legacy.model_preferences.max_output_tokens, Some(1024));
    let encoded = serde_json::to_value(&legacy).unwrap();
    assert_eq!(
        encoded.get("request_id"),
        None,
        "new writes do not repeat the request identity"
    );
    assert_eq!(
        serde_json::to_value(super::request::RequestContext::new(
            "def-1".to_string(),
            legacy.model_preferences,
        ))
        .unwrap()
        .get("request_id"),
        None
    );
    assert!(
        serde_json::from_value::<super::request::RequestContext>(json!({
            "definitions": "def-1",
            "model_preferences": {"maxOutputTokens": null},
            "unexpected": true
        }))
        .is_err(),
        "only the known legacy key is tolerated; other unknown keys stay rejected"
    );
}

/// 公开思考只进入历史展示，不进入模型请求投影，因而不占用同一请求的输入预算。
#[test]
fn public_thinking_does_not_change_the_request_pressure() {
    let dir = tempfile::tempdir().unwrap();
    let measured = |thinking: String| {
        let mut manager = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        manager
            .append_message(AgentMessage::Assistant {
                content: vec![
                    ContentBlock::Thinking { thinking },
                    ContentBlock::Text {
                        text: "answer".to_string(),
                    },
                ],
                stop_reason: None,
                provider_reasoning_replay: None,
            })
            .unwrap();
        let view = context::ContextView::derive(&manager).unwrap();
        (view.messages(&manager), view.request_tokens(0))
    };
    let (short_messages, short_tokens) = measured("brief".to_string());
    let (long_messages, long_tokens) = measured("reasoning ".repeat(500));
    assert_eq!(short_messages, long_messages);
    assert_eq!(short_tokens, long_tokens);
}

fn assistant_with_tool_call(call_id: &str, name: &str) -> AgentMessage {
    AgentMessage::Assistant {
        content: vec![ContentBlock::ToolCall(singularity_model::ModelToolCall {
            tool_call_id: call_id.to_string(),
            tool_name: name.to_string(),
            arguments: json!({"command": "cargo test"}),
        })],
        stop_reason: None,
        provider_reasoning_replay: None,
    }
}

fn tool_result(call_id: &str, text: &str) -> AgentMessage {
    AgentMessage::ToolResult {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        tool_call_id: call_id.to_string(),
        is_error: false,
        duration_ms: None,
        diff: None,
        read_source: None,
    }
}

fn run_operation(operation_id: &str, turn_id: &str) -> LedgerRecord {
    LedgerRecord::OperationStarted {
        operation_id: operation_id.to_string(),
        kind: OperationKind::Run,
        turn_id: Some(turn_id.to_string()),
    }
}

fn entry_ids<T: std::ops::Deref<Target = SessionEntry>>(
    entries: impl IntoIterator<Item = T>,
) -> Vec<String> {
    entries
        .into_iter()
        .map(|entry| entry.id().to_string())
        .collect()
}

/// 按磁盘字面 cwd 构造会话头；已存字面值可能与运行期归一化形状不同。
fn session_header_with_cwd(id: &str, cwd: &str) -> String {
    serde_json::json!({
        "type": "session",
        "version": CURRENT_SESSION_VERSION,
        "id": id,
        "timestamp": "2026-08-20T00:00:00.000Z",
        "cwd": cwd,
    })
    .to_string()
}

fn session_header(id: &str) -> String {
    let cwd = singularity_core::canonicalize_workspace(std::env::current_dir().unwrap())
        .unwrap()
        .display()
        .to_string();
    session_header_with_cwd(id, &cwd)
}

fn session_message(id: &str, text: &str) -> String {
    format!(
        r#"{{"type":"message","id":"{id}","timestamp":"2026-08-20T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
    )
}

#[test]
fn append_does_not_recreate_a_missing_session() {
    let fixture = test_support::SessionFixture::new();
    let mut manager = fixture
        .create_session(fixture.home(), "01914f6b-0000-7000-8000-0000000000bb")
        .unwrap();
    manager.append_message(user("saved")).unwrap();
    std::fs::remove_file(manager.path()).unwrap();

    let error = manager
        .append_message(user("must not appear saved"))
        .unwrap_err();
    assert!(
        matches!(error, SessionError::Io(ref cause) if cause.kind() == std::io::ErrorKind::NotFound)
    );
    assert!(!manager.path().exists());
    assert_eq!(manager.entries().len(), 1);
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
        singularity_core::canonicalize_workspace(&cwd)
            .unwrap()
            .display()
    );
    assert_eq!(first_line["id"].as_str().unwrap(), id);
    drop(manager);

    let opened = fixture.open_read_only(id).unwrap();
    assert_eq!(
        opened.entries().last().expect("content entry").id(),
        leaf.as_str()
    );
    let view = context::ContextView::derive(&opened).unwrap();
    let visible: Vec<_> = view.original_entries(&opened).cloned().collect();
    assert_eq!(entry_ids(visible.as_slice()), vec![id1, id2, id3]);
    assert!(matches!(&visible.as_slice()[0],
            SessionEntry::Message { message: m, .. } if matches!(m, AgentMessage::User { .. }) && m.content_text() == "hello"));
    assert!(matches!(&visible.as_slice()[1],
            SessionEntry::Message { message: m, .. } if matches!(m, AgentMessage::Assistant { .. }) && m.content_text() == "hi there"));
    assert!(matches!(&visible.as_slice()[2],
            SessionEntry::Message {
                message:
                    m @ AgentMessage::ToolResult {
                        tool_call_id,
                        ..
                    },
                ..
            } if m.content_text() == "ls output" && tool_call_id == "call_1"));
}

#[test]
fn tool_results_link_by_call_id_and_reject_the_retired_name_field() {
    let execution = crate::tools::ToolExecution {
        content: "ok".to_string(),
        diff: None,
        is_error: false,
        duration_ms: Some(5),
        read_source: None,
    };
    let message = crate::message::tool_result_message("call-1", &execution);
    let wire = serde_json::to_string(&message).unwrap();
    assert!(
        !wire.contains("toolName"),
        "the original ToolCall record owns the name: {wire}"
    );
    assert_eq!(message.tool_call_id(), Some("call-1"));
    assert_eq!(message.content_text(), "ok");
    // v8 起结果不再携带名称；带该字段的旧记录按未知字段拒绝，不静默忽略。
    let retired = r#"{"role":"toolResult","content":[{"type":"text","text":"ok"}],"toolCallId":"call-1","toolName":"bash","isError":false}"#;
    assert!(serde_json::from_str::<AgentMessage>(retired).is_err());
}

/// 工具结果的调用身份与错误标记是必需语义：缺字段或显式 null 都不再被
/// 读成「正常成功结果」，而是在解析时拒绝。
#[test]
fn tool_result_requires_call_id_and_error_flag() {
    for line in [
        r#"{"role":"toolResult","content":[{"type":"text","text":"ok"}],"isError":false}"#,
        r#"{"role":"toolResult","content":[{"type":"text","text":"ok"}],"toolCallId":"call-1"}"#,
        r#"{"role":"toolResult","content":[{"type":"text","text":"ok"}],"toolCallId":null,"isError":false}"#,
        r#"{"role":"toolResult","content":[{"type":"text","text":"ok"}],"toolCallId":"call-1","isError":null}"#,
    ] {
        assert!(
            serde_json::from_str::<AgentMessage>(line).is_err(),
            "incomplete tool result must be rejected: {line}"
        );
    }
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
    std::fs::create_dir(&cwd).unwrap();
    let mut turn_worker = SessionManager::create(&cwd, &sessions).unwrap();
    let m1 = turn_worker.append_message(user("first")).unwrap();
    let m2 = turn_worker.append_message(assistant("second")).unwrap();
    let file = turn_worker.path().to_path_buf();
    drop(turn_worker);

    // 单写者语义下，写者交接必须经重开（drop 后再 open），同一时刻至多一个
    // 存活的写者。后续 owner 追加 metadata，再后续 owner 继续追加消息。
    let mut settings_writer = SessionManager::open_existing(&file).unwrap();
    let s1 = settings_writer
        .append_metadata(SessionMetadata::ThreadSettings {
            provider: "openai".to_string(),
            model: "test-model".to_string(),
            reasoning: Some("high".to_string()),
        })
        .unwrap();
    drop(settings_writer);
    let mut turn_worker = SessionManager::open_existing(&file).unwrap();
    let m3 = turn_worker.append_message(user("third")).unwrap();
    drop(turn_worker);

    // 重开从 JSONL 重建完整线性链。
    let reopened = SessionManager::open_existing(&file).unwrap();
    let view = context::ContextView::derive(&reopened).unwrap();
    let visible: Vec<_> = view.original_entries(&reopened).cloned().collect();
    assert_eq!(
        entry_ids(reopened.entries()),
        vec![m1.clone(), m2.clone(), s1, m3.clone()]
    );
    assert_eq!(entry_ids(visible.as_slice()), vec![m1, m2, m3]);
    assert_eq!(
        visible.as_slice().len(),
        3,
        "context contains only model-visible entries in file order"
    );
    let ids = visible
        .iter()
        .map(super::format::SessionEntry::id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        ids.len(),
        visible.as_slice().len(),
        "entry ids must be unique"
    );
}

/// 崩溃遗留恢复测试：异常退出的未终结 run 在重新打开时收敛为 interrupted，
/// 未解决的工具调用补齐 synthetic failed 结果，且修复操作保持幂等（二次打开不再改动）。
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
            error: None,
            truncated: false,
            user_stopped: false,
        })
        .unwrap();
    manager
        .append_record(run_operation("op-2", "turn-2"))
        .unwrap();
    drop(manager);

    let reopened = fixture.open_for_repair(id).unwrap();
    drop(reopened);

    let reopened = fixture.open_read_only(id).unwrap();
    assert!(
        reduce_operations(reopened.entries()).unwrap().is_none(),
        "all runs converged"
    );
    assert!(
        reopened.ledger_records().iter().any(|record| matches!(
            record,
            LedgerRecord::OperationFinished {
                operation_id,
                turn_id: Some(turn_id),
                outcome: TurnStatus::Interrupted,
                ..
            } if operation_id == "op-2" && turn_id == "turn-2"
        )),
        "op-2 converged to interrupted while keeping its turn binding"
    );
    drop(reopened);

    let before = std::fs::read(fixture.session_path(id)).unwrap();
    let _reopened = fixture.open_for_repair(id).unwrap();
    assert_eq!(std::fs::read(fixture.session_path(id)).unwrap(), before);
}

/// 多个未解决调用按原始调用顺序补齐结果，且修复结果不复制工具名称。
#[test]
fn recovery_keeps_unresolved_tool_order_without_copying_names() {
    let fixture = test_support::SessionFixture::new();
    let id = "01914f6b-0000-7000-8000-0000000000ae";
    let mut manager = fixture.create_session(fixture.home(), id).unwrap();
    manager
        .append_record(run_operation("op-1", "turn-1"))
        .unwrap();
    let mut message = assistant_with_tool_call("first", "read");
    if let AgentMessage::Assistant { content, .. } = &mut message {
        content.push(ContentBlock::ToolCall(singularity_model::ModelToolCall {
            tool_call_id: "second".into(),
            tool_name: "write".into(),
            arguments: json!({"path": "b"}),
        }));
    }
    manager.append_message(message).unwrap();
    manager
        .append_message(tool_result("first", "done"))
        .unwrap();
    drop(manager);

    let repaired = fixture.open_for_repair(id).unwrap();
    let results: Vec<_> = repaired
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message {
                message:
                    AgentMessage::ToolResult {
                        tool_call_id,
                        is_error,
                        ..
                    },
                ..
            } => Some((tool_call_id.clone(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2, "the recorded result and the repaired one");
    assert_eq!(results[0].0, "first");
    assert_eq!(
        results[1].0, "second",
        "unresolved calls keep their original order"
    );
    assert!(results[1].1, "repair visible as a failure");
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
    manager
        .append_message(tool_result("call-1", "previous completed call"))
        .unwrap();
    manager
        .append_message(assistant_with_tool_call("call-1", "write"))
        .unwrap();
    let entries_before = manager.entries().len();
    drop(manager);

    let reopened = fixture.open_for_repair(id).unwrap();
    let appended = &reopened.entries()[entries_before..];
    let synthetic_results = appended
        .iter()
        .filter(|entry| matches!(entry, SessionEntry::Message { message, .. } if matches!(message, AgentMessage::ToolResult { .. })))
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
                if matches!(message, AgentMessage::ToolResult { .. }) =>
            {
                Some(message.content_text())
            }
            _ => None,
        })
        .unwrap();
    assert!(result_text.contains("outcome unknown"), "{result_text}");
    assert!(
        result_text.contains("Inspect the current state"),
        "{result_text}"
    );
}

/// 定义索引保存全部旧定义：A→B→A 时第三次复用首份记录，不重复落盘。
/// 断言走公开的追加入口与落盘记录：请求上下文引用的定义 id 与持久化的
/// RequestDefinitions 条数即可证明复用，不查询内部索引。
#[test]
fn definitions_are_reused_across_an_intervening_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let request = |tool: &str| {
        let mut request = singularity_model::ModelTurnRequest::new("request", Vec::new());
        request.tools.push(singularity_model::ModelToolSchema {
            name: tool.to_string(),
            description: format!("{tool} tool"),
            parameters_schema: serde_json::json!({"type": "object"}),
        });
        request
    };
    let observation = |request_id: &str| singularity_protocol::RequestObservation {
        request_id: request_id.to_string(),
        request_head: None,
        purpose: singularity_protocol::RequestPurpose::Generation,
        ordinal: 0,
        attempt: 0,
        provider: "scripted".to_string(),
        model: "scripted-model".to_string(),
        status: singularity_protocol::ProviderAttemptStatus::Started,
        duration_ms: 0,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        error: None,
        diagnostic_code: None,
        request_error: None,
    };
    let mut referenced = Vec::new();
    for (index, tool) in ["read", "bash", "read"].into_iter().enumerate() {
        manager
            .append_model_request(
                observation(&format!("request-{index}")),
                Some(&request(tool)),
            )
            .unwrap();
        referenced.push(
            manager
                .ledger_records()
                .iter()
                .rev()
                .find_map(|record| match record {
                    LedgerRecord::ModelRequest {
                        context: Some(context),
                        ..
                    } => Some(context.definitions.clone()),
                    _ => None,
                })
                .expect("every request records the definitions it references"),
        );
    }

    assert_ne!(referenced[0], referenced[1]);
    assert_eq!(
        referenced[0], referenced[2],
        "the repeated definition references the first record again"
    );
    assert_eq!(
        manager
            .ledger_records()
            .iter()
            .filter(|record| matches!(record, LedgerRecord::RequestDefinitions { .. }))
            .count(),
        2,
        "the third identical definition is not persisted a second time"
    );
}

#[test]
fn out_of_order_tool_commits_replay_in_call_order_live_and_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    manager
        .append_record(run_operation("op-1", "turn-1"))
        .unwrap();
    let mut message = assistant_with_tool_call("first", "read");
    if let AgentMessage::Assistant { content, .. } = &mut message {
        content.push(ContentBlock::ToolCall(singularity_model::ModelToolCall {
            tool_call_id: "second".into(),
            tool_name: "read".into(),
            arguments: json!({"path":"b"}),
        }));
    }
    let mut live = context::ContextView::derive(&manager).unwrap();
    // provider call ID 可能在后续批次中被复用；每次部分提交
    // 都必须已与同一 ledger 的全新投影一致。
    for _ in 0..2 {
        manager.append_message(message.clone()).unwrap();
        live.append_entry(&manager, manager.entries().len() - 1)
            .unwrap();
        for id in ["second", "first"] {
            manager.append_message(tool_result(id, id)).unwrap();
            live.append_entry(&manager, manager.entries().len() - 1)
                .unwrap();
            let fresh = context::ContextView::derive(&manager).unwrap();
            assert_eq!(live.messages(&manager), fresh.messages(&manager));
            assert_eq!(live.request_tokens(123), fresh.request_tokens(123));
        }
    }
    assert!(
        reduce_operations(manager.entries())
            .unwrap()
            .expect("active operation")
            .open_tools
            .is_empty()
    );
    let ordered: Vec<_> = live
        .original_entries(&manager)
        .filter_map(|entry| match entry {
            SessionEntry::Message { message, .. } => message.tool_call_id().map(str::to_string),
            _ => None,
        })
        .collect();
    assert_eq!(ordered, ["first", "second", "first", "second"]);
    assert!(
        live.original_entries(&manager)
            .eq(context::ContextView::derive(&manager)
                .unwrap()
                .original_entries(&manager))
    );
    let path = manager.path().to_path_buf();
    let live_entries: Vec<_> = live.original_entries(&manager).cloned().collect();
    drop(manager);
    let restored = SessionData::open(&path).unwrap();
    assert_eq!(
        live_entries,
        context::ContextView::derive(&restored)
            .unwrap()
            .original_entries(&restored)
            .cloned()
            .collect::<Vec<_>>()
    );
}

/// 归约验证完整 ledger，并只返回仍未结束的那个 operation。
#[test]
fn overlapping_operations_are_rejected() {
    let fixture = SessionFixture::new();
    let mut manager = fixture
        .create_session(fixture.home(), &uuid::Uuid::now_v7().to_string())
        .unwrap();
    manager
        .append_record(run_operation("op-1", "turn-1"))
        .unwrap();
    manager
        .append_record(run_operation("op-2", "turn-2"))
        .unwrap();
    assert!(reduce_operations(manager.entries()).is_err());
    let path = manager.path().to_path_buf();
    drop(manager);
    assert!(SessionManager::open_existing(&path).is_err());
}

/// 已完成的操作 ID 不能被后续 operation 复用；完整 ledger 归约仍检测该重复。
#[test]
fn completed_operation_ids_cannot_be_reused() {
    let fixture = SessionFixture::new();
    let mut manager = fixture
        .create_session(fixture.home(), &uuid::Uuid::now_v7().to_string())
        .unwrap();
    manager
        .append_record(run_operation("op-1", "turn-1"))
        .unwrap();
    manager
        .append_record(LedgerRecord::OperationFinished {
            operation_id: "op-1".to_string(),
            turn_id: Some("turn-1".to_string()),
            outcome: TurnStatus::Completed,
            usage: Some(TurnModelUsage::default()),
            error: None,
            truncated: false,
            user_stopped: false,
        })
        .unwrap();
    manager
        .append_record(run_operation("op-1", "turn-2"))
        .unwrap();
    assert!(reduce_operations(manager.entries()).is_err());
    let path = manager.path().to_path_buf();
    drop(manager);
    assert!(SessionManager::open_existing(&path).is_err());
}

/// 正常终结必须已经闭合全部工具调用：仍有未配对调用的终结记录是无效序列，
/// 未闭合的 operation 才由既有修复补未知结果。
#[test]
fn terminal_with_unresolved_tool_calls_is_rejected() {
    let fixture = SessionFixture::new();
    let mut manager = fixture
        .create_session(fixture.home(), &uuid::Uuid::now_v7().to_string())
        .unwrap();
    manager
        .append_record(run_operation("op-1", "turn-1"))
        .unwrap();
    manager
        .append_message(AgentMessage::Assistant {
            content: vec![ContentBlock::ToolCall(singularity_model::ModelToolCall {
                tool_call_id: "call-1".to_string(),
                tool_name: "read".to_string(),
                arguments: json!({"path": "x.txt"}),
            })],
            stop_reason: None,
            provider_reasoning_replay: None,
        })
        .unwrap();
    manager
        .append_record(LedgerRecord::OperationFinished {
            operation_id: "op-1".to_string(),
            turn_id: Some("turn-1".to_string()),
            outcome: TurnStatus::Completed,
            usage: Some(TurnModelUsage::default()),
            error: None,
            truncated: false,
            user_stopped: false,
        })
        .unwrap();
    assert!(reduce_operations(manager.entries()).is_err());
    let path = manager.path().to_path_buf();
    drop(manager);
    assert!(SessionManager::open_existing(&path).is_err());
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

/// 持久格式拒绝未知字段，避免静默丢失无法识别的数据。
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
            "type": "message",
            "id": "m-1",
            "message": {"role": "assistant", "content": [{"type": "tool_call", "id": "c-1", "name": "read", "args": {}, "unknown": 1}]}
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

#[test]
fn strict_open_repairs_torn_tail_and_missing_final_newline() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir
        .path()
        .join("01914f6b-0000-7000-8000-000000000001.jsonl");
    // 已存 cwd 使用原生分隔符并带结尾分隔符：运行期路径另行归一化，尾部修复
    // 只能收敛撕裂的尾部，不得把归一化形状写回磁盘。
    let saved_cwd = format!("{}{}", dir.path().display(), std::path::MAIN_SEPARATOR);
    let prefix = format!(
        "{}\n{}\n",
        session_header_with_cwd("01914f6b-0000-7000-8000-000000000001", &saved_cwd),
        session_message("entry-1", "one")
    );
    std::fs::write(&file, format!("{prefix}{{\"type\":\"message\",\"id\":\"")).unwrap();
    let opened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(
        context::ContextView::derive(&opened)
            .unwrap()
            .original_entries(&opened)
            .len(),
        1
    );
    assert!(std::fs::read(&file).unwrap().ends_with(b"\n"));
    let repaired = std::fs::read_to_string(&file).unwrap();
    let repaired_header: serde_json::Value =
        serde_json::from_str(repaired.lines().next().unwrap()).unwrap();
    assert_eq!(repaired_header["cwd"], serde_json::json!(saved_cwd));
    drop(opened);
    let mut reopened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(
        context::ContextView::derive(&reopened)
            .unwrap()
            .original_entries(&reopened)
            .len(),
        1
    );
    reopened.append_message(user("after repair")).unwrap();
    drop(reopened);
    let reopened_again = SessionManager::open_existing(&file).unwrap();
    assert_eq!(
        context::ContextView::derive(&reopened_again)
            .unwrap()
            .original_entries(&reopened_again)
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
            .original_entries(&opened)
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

    let error = SessionData::open(&file)
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

    let opened = SessionData::open(&file).unwrap();
    assert_eq!(opened.created_at(), "2026-08-20T00:00:00.000Z");
}

#[test]
fn strict_open_rejects_invalid_headers_and_old_versions() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = serde_json::to_string(dir.path()).unwrap();

    // 1. 缺失 version
    let missing_version = dir.path().join("missing-version.jsonl");
    std::fs::write(
        &missing_version,
        format!(r#"{{"type":"session","id":"01914f6b-0000-7000-8000-000000000001","timestamp":"2026-08-20T00:00:00.000Z","cwd":{cwd}}}"#),
    )
    .unwrap();
    assert!(matches!(
        SessionManager::open_existing(&missing_version).unwrap_err(),
        SessionError::InvalidHeader(_)
    ));

    // 2. header 只接受当前版本。
    for version in [1, 2, 3, 4, 5, 6, 7] {
        let old_file = dir.path().join(format!("unsupported-v{version}.jsonl"));
        std::fs::write(
            &old_file,
            format!(
                r#"{{"type":"session","version":{version},"id":"01914f6b-0000-7000-8000-000000000001","timestamp":"2026-08-20T00:00:00.000Z","cwd":{cwd}}}"#
            ),
        )
        .unwrap();
        assert!(matches!(
            SessionManager::open_existing(&old_file).unwrap_err(),
            SessionError::InvalidHeader(_)
        ));
    }

    // 3. header 含有未知字段；其余字段合法，使样例只违反这一条规则。
    let unknown_field = dir.path().join("unknown-field.jsonl");
    std::fs::write(
        &unknown_field,
        format!(r#"{{"type":"session","version":{CURRENT_SESSION_VERSION},"id":"01914f6b-0000-7000-8000-000000000001","timestamp":"2026-08-20T00:00:00.000Z","cwd":{cwd},"extra":"field"}}"#),
    )
    .unwrap();
    assert!(matches!(
        SessionManager::open_existing(&unknown_field).unwrap_err(),
        SessionError::InvalidHeader(_)
    ));

    // 4. header id 不是合法 UUID；其余字段合法，使样例只违反这一条规则。
    let non_uuid = dir.path().join("non-uuid.jsonl");
    std::fs::write(
        &non_uuid,
        format!(r#"{{"type":"session","version":{CURRENT_SESSION_VERSION},"id":"not-a-uuid","timestamp":"2026-08-20T00:00:00.000Z","cwd":{cwd}}}"#),
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
    manager.data.file = dir.path().to_path_buf();
    assert!(manager.append_message(user("must fail")).is_err());
    assert_eq!(
        entry_ids(before.original_entries(&manager)),
        entry_ids(
            context::ContextView::derive(&manager)
                .unwrap()
                .original_entries(&manager)
        )
    );
    assert!(manager.entries().is_empty());
}

#[test]
fn access_open_append_keeps_interrupted_operation_and_appends_under_lock() {
    let fixture = SessionFixture::new();
    let mut manager = fixture
        .create_session(fixture.home(), &uuid::Uuid::now_v7().to_string())
        .unwrap();
    let session_id = manager.session_id().to_string();
    manager
        .append_record(run_operation("op-1", "turn_1"))
        .unwrap();
    let file = manager.path().to_path_buf();
    drop(manager);

    let coordinator = std::sync::Arc::new(WriterLockCoordinator::default());
    let mut opened = SessionManager::open_existing_with_access(
        &file,
        &coordinator,
        ExpectedSession {
            id: &session_id,
            cwd: None,
        },
        SessionAccess::Append,
    )
    .unwrap();
    let operation = reduce_operations(opened.entries())
        .unwrap()
        .expect("Append intent must not repair interrupted operations");
    assert_eq!(operation.turn_id.as_deref(), Some("turn_1"));
    opened
        .append_metadata(SessionMetadata::ThreadName {
            name: "renamed".to_string(),
        })
        .unwrap();
}

/// 期望身份不符时本次打开零文件变更：校验发生在任何尾部重写之前。撕裂的尾部
/// 本来会被 RepairAndRewrite 截掉并补写换行，身份拒绝必须先于这一步，且两种
/// 意图都以同一份可定位的失败关闭。
#[test]
fn access_open_rejects_a_wrong_id_before_repairing_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir
        .path()
        .join("01914f6b-0000-7000-8000-000000000001.jsonl");
    let prefix = format!(
        "{}\n{}\n",
        session_header("01914f6b-0000-7000-8000-000000000001"),
        session_message("entry-1", "one")
    );
    // 半条 JSON 结尾：正常打开会重写该文件。
    std::fs::write(&file, format!("{prefix}{{\"type\":\"message\",\"id\":\"")).unwrap();
    let before = std::fs::read(&file).unwrap();

    let coordinator = std::sync::Arc::new(WriterLockCoordinator::default());
    for access in [SessionAccess::RepairWrite, SessionAccess::Append] {
        let error = SessionManager::open_existing_with_access(
            &file,
            &coordinator,
            ExpectedSession {
                id: "other-id",
                cwd: None,
            },
            access,
        )
        .expect_err("header id mismatch must fail closed for both intents");
        assert!(matches!(error, SessionError::InvalidHeader(_)));
        assert!(
            error.to_string().contains("other-id"),
            "the failure names the expected identity: {error}"
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            before,
            "a rejected open must not repair or rewrite the file"
        );
    }

    // 合法身份仍完成尾部修复：本轮唯一的提前校验没有拿走既有修复语义。
    let repaired = SessionManager::open_existing_with_access(
        &file,
        &coordinator,
        ExpectedSession {
            id: "01914f6b-0000-7000-8000-000000000001",
            cwd: None,
        },
        SessionAccess::RepairWrite,
    )
    .unwrap();
    assert_eq!(
        context::ContextView::derive(&repaired)
            .unwrap()
            .original_entries(&repaired)
            .len(),
        1
    );
    assert!(std::fs::read(&file).unwrap().ends_with(b"\n"));
}

/// 期望目录不符时本次打开零文件变更：工作区归属校验与 id 校验一样，发生在
/// 尾部重写与未完成 operation 修复之前。
#[test]
fn access_open_rejects_a_foreign_directory_before_repairing() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir
        .path()
        .join("01914f6b-0000-7000-8000-000000000002.jsonl");
    let session_id = "01914f6b-0000-7000-8000-000000000002";
    let owner = dir.path().join("owner");
    std::fs::create_dir_all(&owner).unwrap();
    let header = format!(
        "{}\n",
        session_header_with_cwd(session_id, &owner.to_string_lossy())
    );
    let prefix = format!("{header}{}\n", session_message("entry-1", "one"));
    // 半条 JSON 结尾：正常打开会重写该文件。
    std::fs::write(&file, format!("{prefix}{{\"type\":\"message\",\"id\":\"")).unwrap();
    let before = std::fs::read(&file).unwrap();

    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let coordinator = std::sync::Arc::new(WriterLockCoordinator::default());

    let error = SessionManager::open_existing_with_access(
        &file,
        &coordinator,
        ExpectedSession {
            id: session_id,
            cwd: Some(&other.to_string_lossy()),
        },
        SessionAccess::RepairWrite,
    )
    .expect_err("a foreign directory must fail closed before any repair");
    assert!(matches!(error, SessionError::ScopeMismatch { .. }));
    assert_eq!(
        std::fs::read(&file).unwrap(),
        before,
        "a rejected open must not rewrite the tail"
    );

    // 同一文件在正确目录下仍完成尾部修复。
    let repaired = SessionManager::open_existing_with_access(
        &file,
        &coordinator,
        ExpectedSession {
            id: session_id,
            cwd: Some(&owner.to_string_lossy()),
        },
        SessionAccess::RepairWrite,
    )
    .unwrap();
    assert_eq!(
        repaired.cwd_string(),
        singularity_core::display_path(&owner)
    );
    assert!(std::fs::read(&file).unwrap().ends_with(b"\n"));
}

// --- JSONL 字节级 round-trip 夹具 -------------------------------------------
//
// 这些夹具固定会话线的 wire 形状（键名、camelCase、skip-if-none 行为、枚举
// 词形）。任何对 AgentMessage/SessionEntry/LedgerRecord 的序列化改动
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
const COMPLETE_SESSION: &str = r###"{"cwd":"C:/work","id":"01914f6b-0000-7000-8000-0000000000e1","timestamp":"2026-08-20T00:00:00.000Z","type":"session","version":8}
{"type":"record","id":"r-op-start","timestamp":"2026-08-20T00:00:00.500Z","record":{"recordType":"operation_started","operationId":"op-1","kind":"run","turnId":"turn-1"}}
{"type":"message","id":"m-user-1","timestamp":"2026-08-20T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}
{"type":"message","id":"m-assistant-1","timestamp":"2026-08-20T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"reasoning trace"},{"type":"text","text":"analysis"},{"type":"tool_call","id":"call-1","name":"bash","args":{"command":"cargo test"}}],"stopReason":"stop"}}
{"type":"message","id":"m-tr-1","timestamp":"2026-08-20T00:00:03.000Z","message":{"role":"toolResult","content":[{"type":"text","text":"ok"}],"toolCallId":"call-1","isError":false}}
{"type":"compaction","id":"c-1","timestamp":"2026-08-20T00:00:05.000Z","compaction":{"summary":"## Goal\ncompacted history","firstKeptEntryId":"m-user-1"}}
{"type":"record","id":"r-op-finish","timestamp":"2026-08-20T00:00:06.000Z","record":{"recordType":"operation_finished","operationId":"op-1","turnId":"turn-1","outcome":"completed","usage":{"inputTokens":0,"outputTokens":0,"totalTokens":0,"cachedInputTokens":0,"reasoningTokens":0,"usagePresent":false,"usageComplete":false},"truncated":true}}
{"type":"metadata","id":"md-2","timestamp":"2026-08-20T00:00:07.000Z","metadata":{"metadataType":"thread_settings","provider":"openai_compatible","model":"test-model-a","reasoning":"high"}}
{"type":"metadata","id":"md-3","timestamp":"2026-08-20T00:00:08.000Z","metadata":{"metadataType":"thread_name","name":"typed metadata"}}"###;

#[test]
fn jsonl_wire_round_trip_fixtures_cover_all_entry_shapes() {
    assert_lines_round_trip(COMPLETE_SESSION.as_bytes());
    let definitions: super::request::RequestDefinitions = serde_json::from_value(serde_json::json!({
        "messages": [{"role": "system", "content": "saved instructions", "tool_call_id": null, "tool_calls": null}],
        "tools": []
    })).unwrap();
    assert_eq!(definitions.messages[0].content, "saved instructions");
    let preferences: singularity_protocol::RequestPreferences =
        serde_json::from_value(serde_json::json!({
            "model_name": "previous-model", "maxOutputTokens": 1024
        }))
        .unwrap();
    assert_eq!(preferences.max_output_tokens, Some(1024));
}

/// 实测校正属于产生它的那份内容：追加不改变形状，结构替换使校正失效。
///
/// 校正描述的是「最近一次同形状请求」的实测差量；一旦剪枝或摘要换掉了正文，
/// 同一个差量继续参与估价就不再对应任何真实请求，因此重建必须重新估价。
#[test]
fn a_measured_correction_survives_appends_and_expires_with_its_content() {
    use singularity_model::ModelUsage;

    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    manager
        .append_message(assistant_with_tool_call("one", "read"))
        .unwrap();
    manager
        .append_message(tool_result("one", "tool output"))
        .unwrap();
    let overhead = 100;
    let mut view = context::ContextView::derive(&manager).unwrap();
    let estimated = view.request_tokens(overhead);

    // 一次实测远高于估价的请求：校正生效。输入的 usage 形状与协议解析结果一致：
    // 供应商只上报输入与输出，总数已由两者补出，校正直接消费这个总数而不再相加。
    let usage = ModelUsage {
        input_tokens: estimated + 3_998,
        output_tokens: 2,
        total_tokens: estimated + 4_000,
        usage_present: true,
        ..ModelUsage::default()
    };
    view.record_usage(&usage, 0, overhead);
    let corrected = view.request_tokens(overhead);
    assert_eq!(corrected, estimated + 4_000);

    // 正常追加不重建视图：校正保留，同形状的下一轮请求仍有依据。
    manager.append_message(user("more")).unwrap();
    let appended = manager.entries().len() - 1;
    view.append_entry(&manager, appended).unwrap();
    assert_eq!(
        view.request_tokens(overhead),
        corrected + context::entry_token_estimate(&manager.entries()[appended])
    );

    // 结构替换（工具结果剪枝记录）会重建视图：校正随内容一起失效。
    let pruned_entry = manager
        .entries()
        .iter()
        .find(|entry| {
            matches!(
                entry,
                SessionEntry::Message {
                    message: AgentMessage::ToolResult { .. },
                    ..
                }
            )
        })
        .unwrap()
        .id()
        .to_string();
    manager
        .append_record(LedgerRecord::ToolResultPruned {
            entry_id: pruned_entry,
            content: vec![ContentBlock::Text {
                text: "[pruned]".to_string(),
            }],
        })
        .unwrap();
    view.append_entry(&manager, manager.entries().len() - 1)
        .unwrap();
    assert_eq!(
        view.request_tokens(overhead),
        ContextView::derive(&manager)
            .unwrap()
            .request_tokens(overhead),
        "a structural replacement drops the correction instead of carrying it over"
    );
}
