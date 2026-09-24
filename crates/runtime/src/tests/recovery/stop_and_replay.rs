use super::*;

/// 接受停止与终态落盘故障同时发生：终态无法提交时未送达输入同样不归还，
/// 处置事件与持久终态路径一致。
#[test]
fn an_accepted_stop_survives_a_terminal_write_failure() {
    use crate::conversation::ConversationError;
    use crate::error::TurnRunError;
    use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};
    use singularity_protocol::{ControlDisposition, ProviderAttemptStatus, TurnEvent};

    let fixture = SessionsFixture::new();
    let provider = Arc::new(ScriptedProvider::new([ScriptedAttempt::failure_kind(
        singularity_model::ModelErrorKind::AuthError,
        "invalid api key",
    )]));
    let (conversation, path) = crate::test_support::conversation_with(
        &fixture,
        Arc::clone(&provider) as Arc<dyn Provider + Send + Sync>,
        None,
    );
    let permissions = std::fs::metadata(&path).unwrap().permissions();
    let mut events = Vec::new();
    let mut blocked = false;
    let steered = std::sync::Mutex::new(None);
    let queued = std::sync::Mutex::new(None);
    let result = {
        let conversation = Arc::clone(&conversation);
        crate::test_support::run_async(conversation.run_turn("go", &mut |event| {
            if let TurnEvent::ProviderAttempt { observation, .. } = &event
                && observation.status == ProviderAttemptStatus::Error
                && !blocked
            {
                // 真实失败已经确定：接受停止并让终态记录写不进去。
                *steered.lock().unwrap() = Some(
                    conversation
                        .steer("cancelled steer")
                        .expect("steer is accepted"),
                );
                *queued.lock().unwrap() = Some(
                    conversation
                        .submit_follow_up("must stay queued")
                        .expect("a queued follow-up is accepted"),
                );
                conversation.abort().expect("the stop is accepted");
                let mut readonly = permissions.clone();
                readonly.set_readonly(true);
                std::fs::set_permissions(&path, readonly).unwrap();
                blocked = true;
            }
            events.push(event);
        }))
    };
    std::fs::set_permissions(&path, permissions).unwrap();

    assert!(blocked);
    assert!(
        matches!(
            result,
            Err(ConversationError::Turn(
                TurnRunError::Terminalization { .. }
            ))
        ),
        "a terminal write failure keeps its own error shape: {result:?}"
    );
    let cancelled: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            TurnEvent::ControlChanged { control }
                if control.disposition == ControlDisposition::Cancelled =>
            {
                Some(control.control_id.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        cancelled,
        vec![steered.lock().unwrap().as_ref().unwrap().control_id.clone()],
        "the undelivered steer is dispositioned as cancelled"
    );
    let pending = conversation.snapshot().pending_controls;
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].control_id,
        queued.lock().unwrap().as_ref().unwrap().control_id
    );
}

/// 进程在终态提交前异常退出测试：验证持久化前缀（已记录的 operation 起始、
/// 工具调用等）在 resume_thread 时从 ledger 事实收敛：为未完成工具补齐
/// 失败结果闭合配对，记录唯一的 interrupted 终态，未完成副作用绝不自动重放，收敛后会话可直接接受新轮次。
#[test]
fn crash_before_terminal_commit_converges_from_ledger_on_resume() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let catalog = fixture.catalog();
    let cwd = std::env::current_dir()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let thread = catalog.create_thread(&cwd, None).expect("create thread");
    let thread_id = thread.thread_id;
    let path = sessions.join(format!("{thread_id}.jsonl"));

    // 进程死亡时刻的 durable 前缀（写者 drop = 锁释放，终态未落盘）。
    let mut writer = SessionManager::open_existing_with_access(
        &path,
        &fixture.coordinator,
        ExpectedSession {
            id: &thread_id,
            cwd: None,
        },
        singularity_agent::session::SessionAccess::Append,
    )
    .expect("writer open");
    writer
        .append_record(LedgerRecord::OperationStarted {
            operation_id: "op-crash".to_string(),
            kind: singularity_agent::session::OperationKind::Run,
            turn_id: Some("turn-crash".to_string()),
        })
        .expect("operation started");
    writer
        .append_message(singularity_agent::message::AgentMessage::Assistant {
            content: vec![singularity_agent::message::ContentBlock::ToolCall(
                singularity_model::ModelToolCall {
                    tool_call_id: "call-1".to_string(),
                    tool_name: "edit".to_string(),
                    arguments: serde_json::json!({
                        "path": "x.txt",
                        "oldString": "a",
                        "newString": "b"
                    }),
                },
            )],
            stop_reason: None,
            provider_reasoning_replay: None,
        })
        .expect("assistant with tool call");
    drop(writer);

    let resumed = catalog
        .resume_thread(&thread_id, &cwd)
        .expect("resume converges the open operation");
    assert_eq!(
        catalog
            .read_thread_summary(&thread_id)
            .expect("summary projection")
            .status,
        Some(singularity_protocol::TurnStatus::Interrupted),
        "the crashed turn projects as interrupted from ledger facts"
    );

    let session = SessionData::open(&path).expect("reopen");
    let repair_at = session
        .entries()
        .iter()
        .position(|entry| {
            matches!(entry, singularity_agent::session::SessionEntry::Message { message, .. }
                if message.content_text() == singularity_agent::session::REPAIR_UNKNOWN_OUTCOME)
        })
        .expect("the unresolved tool call closes with a model-visible failure");
    let finished_at = session
        .entries()
        .iter()
        .position(|entry| {
            matches!(entry,
                singularity_agent::session::SessionEntry::Record {
                    record: LedgerRecord::OperationFinished {
                        operation_id,
                        outcome: singularity_protocol::TurnStatus::Interrupted,
                        ..
                    },
                    ..
                } if operation_id == "op-crash")
        })
        .expect("exactly one interrupted terminal record converges the operation");
    assert!(
        repair_at < finished_at,
        "the repair record is durable before the recovered terminal outcome"
    );
    let terminals = session
        .ledger_records()
        .iter()
        .filter(|record| matches!(record, LedgerRecord::OperationFinished { .. }))
        .count();
    assert_eq!(
        terminals, 1,
        "exactly one terminal outcome for the crashed turn"
    );
    drop(session);

    // 收敛后的 Thread 在同一条执行链上继续新 turn。
    let provider = Arc::new(singularity_model::test_support::ScriptedProvider::ok(
        "recovered continuation",
    ));
    let runner = fixture.runner(Some(
        provider as Arc<dyn singularity_model::Provider + Send + Sync>,
    ));
    let conversation = Conversation::new(runner, resumed);
    let mut sink = |_event| {};
    let outcome =
        crate::test_support::run_async(conversation.run_turn("continue after crash", &mut sink))
            .expect("the next turn runs on the converged ledger");
    assert_eq!(
        outcome.turn_status,
        singularity_protocol::TurnStatus::Completed
    );
}

/// 撕裂尾部与未终结 operation 同时存在时的恢复测试：打开写路径时优先丢弃不完整的
/// 尾部半写行，确保持久化前缀完整，再按 ledger 事实幂等收敛未终结 operation；
/// 上下文视图仅由完整条目派生。
#[test]
fn torn_tail_is_repaired_before_recovery_decisions() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let catalog = fixture.catalog();
    let cwd = std::env::current_dir()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let thread = catalog.create_thread(&cwd, None).expect("create thread");
    let thread_id = thread.thread_id;
    let path = sessions.join(format!("{thread_id}.jsonl"));

    let mut writer = SessionManager::open_existing_with_access(
        &path,
        &fixture.coordinator,
        ExpectedSession {
            id: &thread_id,
            cwd: None,
        },
        singularity_agent::session::SessionAccess::Append,
    )
    .expect("writer open");
    writer
        .append_record(LedgerRecord::OperationStarted {
            operation_id: "op-torn".to_string(),
            kind: singularity_agent::session::OperationKind::Run,
            turn_id: Some("turn-torn".to_string()),
        })
        .expect("operation started");
    writer
        .append_message(singularity_agent::message::AgentMessage::User {
            content: vec![singularity_agent::message::ContentBlock::Text {
                text: "question before the crash".to_string(),
            }],
        })
        .expect("user message");
    drop(writer);

    // 进程在写入中途死亡：最后一行没有换行符也不是合法 JSON。
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open for torn tail")
        .write_all(b"{\"type\":\"message\",\"id\":\"__incomplete_tail__")
        .expect("write torn tail");

    catalog
        .resume_thread(&thread_id, &cwd)
        .expect("resume repairs the tail and converges the operation");
    assert_eq!(
        catalog
            .read_thread_summary(&thread_id)
            .expect("summary projection")
            .status,
        Some(singularity_protocol::TurnStatus::Interrupted)
    );

    let content = std::fs::read_to_string(&path).expect("read file");
    assert!(content.ends_with('\n'), "the file ends with a full line");
    assert!(
        !content.contains("__incomplete_tail__"),
        "the incomplete tail is dropped, never parsed as a fact"
    );

    let session = SessionData::open(&path).expect("reopen");
    assert!(
        session.entries().iter().any(|entry| {
            matches!(entry, singularity_agent::session::SessionEntry::Message { message, .. }
                if message.content_text() == "question before the crash")
        }),
        "the durable prefix survives the tail repair"
    );
    singularity_agent::session::ContextView::derive(&session).expect("valid repaired context");
}
