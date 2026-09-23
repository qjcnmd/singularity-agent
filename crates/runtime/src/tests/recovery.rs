#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! Runner 的持久化先于发布（durable-before-publish）与崩溃恢复端到端测试。
//!
//! 通过受控网关将执行精确挂起在「首个 provider 请求已发出」处：验证
//! operation_started 与 model_request 已先行落盘，而终态记录尚未产生；
//! 放行后轮次收敛，终态记录才持久化。文件行序反映真实的持久化时序。

use std::sync::Arc;

use crate::Conversation;
use crate::test_support::{GatedProvider, SessionsFixture};
use singularity_agent::session::{
    ExpectedSession, LedgerRecord, SessionData, SessionManager, reduce_operations,
};
use singularity_model::Provider;

#[test]
fn terminal_write_failure_after_assistant_completion_publishes_no_turn_terminal() {
    use crate::conversation::ConversationError;
    use crate::error::TurnRunError;
    use singularity_protocol::{DiagnosticSeverity, TurnEvent, diagnostic_code};

    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let runner = fixture.runner(Some(Arc::new(
        singularity_model::test_support::ScriptedProvider::ok("finished work"),
    )));
    let thread = fixture
        .catalog()
        .create_thread(fixture.home().to_str().unwrap(), None)
        .unwrap();
    let path = sessions.join(format!("{}.jsonl", thread.thread_id));
    let permissions = std::fs::metadata(&path).unwrap().permissions();
    let conversation = Conversation::new(Arc::clone(&runner), thread.clone());
    let mut events = Vec::new();
    let mut blocked_terminal = false;
    let result = conversation.run_turn("go", &mut |event| {
        // assistant 结果已提交；只剩 turn 终态未落盘。
        if matches!(event, TurnEvent::ItemCompleted { .. }) && !blocked_terminal {
            let mut readonly = permissions.clone();
            readonly.set_readonly(true);
            std::fs::set_permissions(&path, readonly).unwrap();
            blocked_terminal = true;
        }
        events.push(event);
    });
    std::fs::set_permissions(&path, permissions).unwrap();

    assert!(blocked_terminal);
    assert!(matches!(
        result,
        Err(ConversationError::Turn(TurnRunError::Terminalization {
            storage: Some(_),
            ..
        }))
    ));
    assert!(!events.iter().any(|event| matches!(
        event,
        TurnEvent::TurnCompleted { .. } | TurnEvent::TurnFailed { .. }
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event,
                TurnEvent::Diagnostic { code, severity: DiagnosticSeverity::Error, .. }
                    if code == diagnostic_code::STORAGE_FATAL
            ))
            .count(),
        1
    );
    let saved = SessionData::open(&path).unwrap();
    assert!(saved.entries().iter().any(|entry| matches!(entry,
        singularity_agent::session::SessionEntry::Message { message, .. }
            if message.content_text() == "finished work"
    )));
    assert!(reduce_operations(saved.entries()).unwrap().is_some());
    drop(saved);
    // 失败的运行已释放其写者，使常规修复得以关闭该 operation。
    let repaired = SessionManager::open_existing_with_access(
        &path,
        &fixture.coordinator,
        ExpectedSession {
            id: &thread.thread_id,
            cwd: None,
        },
        singularity_agent::session::SessionAccess::RepairWrite,
    )
    .unwrap();
    assert!(reduce_operations(repaired.entries()).unwrap().is_none());
}

#[test]
fn operation_start_is_durable_before_the_provider_call_and_terminal_after() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();

    let (gate, started_rx) = GatedProvider::stop_gate();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    gate.with_release(release_rx);

    let runner = fixture.runner(Some(gate as Arc<dyn Provider + Send + Sync>));
    let thread = fixture
        .catalog()
        .create_thread(std::env::current_dir().unwrap().to_str().unwrap(), None)
        .expect("create thread");
    let thread_id = thread.thread_id.clone();
    let conversation = Conversation::new(runner, thread);

    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            let mut sink = |_event| {};
            conversation.run_turn("go", &mut sink)
        })
    };

    // turn 停在 provider 边界：起始记录已 durable，终态尚未产生。
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("turn reaches the provider");
    let path = sessions.join(format!("{thread_id}.jsonl"));
    let mid = SessionData::open(&path).expect("read-only open mid-turn");
    let operation = reduce_operations(mid.entries())
        .unwrap()
        .expect("exactly one open run while the turn is executing");
    let started_turn_id = operation
        .turn_id
        .expect("a run operation carries its turn id");
    assert!(
        mid.ledger_records()
            .iter()
            .any(|record| matches!(record, LedgerRecord::OperationStarted { .. })),
        "the operation started is durable before the provider call"
    );
    assert!(
        !mid.ledger_records()
            .iter()
            .any(|record| matches!(record, LedgerRecord::OperationFinished { .. })),
        "no terminal record is published before the turn converges"
    );
    drop(mid);

    // 放行：provider 返回，turn 收敛，终态记录落盘。
    release_tx.send(()).expect("release the gate");
    let outcome = worker.join().expect("worker").expect("turn ok");
    assert_eq!(
        outcome.turn_status,
        singularity_protocol::TurnStatus::Completed
    );

    let after = SessionData::open(&path).expect("reopen");
    assert!(
        reduce_operations(after.entries()).unwrap().is_none(),
        "run converged"
    );
    let finished_turn_id = after
        .ledger_records()
        .iter()
        .find_map(|record| match record {
            LedgerRecord::OperationFinished {
                turn_id,
                outcome: singularity_protocol::TurnStatus::Completed,
                ..
            } => turn_id.clone(),
            _ => None,
        })
        .expect("a completed terminal record is durable");
    assert_eq!(
        finished_turn_id, started_turn_id,
        "the durable terminal record closes the started turn"
    );
}

/// 执行期工具结果提交失败（存储故障）测试：副作用已经发生，但结果无法落盘。
/// 断言不产生普通可信终态、链条停止、未执行输入留队，且重开时既有修复恰好
/// 补一次未知结果，绝不重放工具。
#[test]
fn session_commit_failure_during_tool_results_stops_the_chain_without_a_trusted_terminal() {
    use crate::conversation::ConversationError;
    use crate::error::TurnRunError;
    use singularity_agent::session::{REPAIR_UNKNOWN_OUTCOME, SessionAccess, SessionEntry};
    use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};
    use singularity_protocol::{DiagnosticSeverity, TurnEvent, TurnFailureCause, diagnostic_code};

    let fixture = SessionsFixture::new();
    let provider = Arc::new(ScriptedProvider::new([ScriptedAttempt::tool_call(
        "call-1",
        "read",
        serde_json::json!({"path": "Cargo.toml"}),
    )]));
    let (conversation, path) = crate::test_support::conversation_with(
        &fixture,
        Arc::clone(&provider) as Arc<dyn Provider + Send + Sync>,
        None,
    );
    let permissions = std::fs::metadata(&path).unwrap().permissions();
    let mut events = Vec::new();
    let mut blocked = false;
    let queued = std::sync::Mutex::new(None);
    let result = {
        let conversation = Arc::clone(&conversation);
        conversation.run_turn("go", &mut |event| {
            // 工具已经执行、结果提交之前把会话文件置为只读：提交本身失败。
            if matches!(event, TurnEvent::ToolExecutionStart { .. }) && !blocked {
                let mut readonly = permissions.clone();
                readonly.set_readonly(true);
                std::fs::set_permissions(&path, readonly).unwrap();
                blocked = true;
                *queued.lock().unwrap() =
                    Some(conversation.submit_follow_up("must stay queued").unwrap());
            }
            events.push(event);
        })
    };
    std::fs::set_permissions(&path, permissions).unwrap();

    assert!(blocked);
    assert!(
        matches!(
            result,
            Err(ConversationError::Turn(TurnRunError::Terminalization {
                execution: Some(ref error),
                storage: None,
            })) if error.cause == TurnFailureCause::Store
        ),
        "an execution-time session failure must not be reported as a trusted terminal: {result:?}"
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        TurnEvent::TurnCompleted { .. } | TurnEvent::TurnFailed { .. }
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event,
                TurnEvent::Diagnostic { code, severity: DiagnosticSeverity::Error, .. }
                    if code == diagnostic_code::STORAGE_FATAL
            ))
            .count(),
        1
    );
    // 链条停止：没有第二次模型请求，后续输入原样留队。
    assert_eq!(provider.requests().len(), 1);
    let pending = conversation.snapshot().pending_controls;
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].control_id,
        queued.lock().unwrap().as_ref().unwrap().control_id
    );

    // operation 仍未闭合，未配对的工具调用保留给既有修复路径。
    let saved = SessionData::open(&path).unwrap();
    let operation = reduce_operations(saved.entries())
        .unwrap()
        .expect("the failed operation stays open for repair");
    assert_eq!(operation.open_tools, vec!["call-1".to_string()]);
    assert!(
        !saved
            .ledger_records()
            .iter()
            .any(|record| matches!(record, LedgerRecord::OperationFinished { .. })),
        "no trusted terminal record is written for an execution-time storage failure"
    );
    drop(saved);

    let repaired = SessionManager::open_existing_with_access(
        &path,
        &fixture.coordinator,
        ExpectedSession {
            id: path.file_stem().unwrap().to_str().unwrap(),
            cwd: None,
        },
        SessionAccess::RepairWrite,
    )
    .unwrap();
    assert_eq!(
        repaired
            .entries()
            .iter()
            .filter(
                |entry| matches!(entry, SessionEntry::Message { message, .. }
                if message.content_text() == REPAIR_UNKNOWN_OUTCOME)
            )
            .count(),
        1,
        "exactly one unknown-outcome result closes the unresolved tool call"
    );
    drop(repaired);
    assert_eq!(
        provider.requests().len(),
        1,
        "repair never replays a tool with side effects"
    );
}

/// 收尾故障不覆盖原始执行失败：provider 鉴权错误之后终态记录写入失败，
/// 宿主报告必须同时保留两个原因，且不发布任何终态事件。
#[test]
fn terminal_write_failure_keeps_the_execution_failure_and_the_storage_failure() {
    use crate::conversation::ConversationError;
    use crate::error::TurnRunError;
    use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};
    use singularity_protocol::{
        DiagnosticSeverity, ProviderAttemptStatus, TurnEvent, TurnFailureCause, diagnostic_code,
    };

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
    let result = conversation.run_turn("go", &mut |event| {
        // 失败的 attempt 观测已经落盘；此后只剩 turn 终态未写。
        if let TurnEvent::ProviderAttempt { observation, .. } = &event
            && observation.status == ProviderAttemptStatus::Error
            && !blocked
        {
            let mut readonly = permissions.clone();
            readonly.set_readonly(true);
            std::fs::set_permissions(&path, readonly).unwrap();
            blocked = true;
        }
        events.push(event);
    });
    std::fs::set_permissions(&path, permissions).unwrap();

    assert!(blocked);
    let Err(ConversationError::Turn(error)) = &result else {
        panic!("terminalization must fail without a trusted terminal: {result:?}");
    };
    let TurnRunError::Terminalization {
        execution: Some(execution),
        storage: Some(storage),
    } = error
    else {
        panic!("both the execution failure and the storage failure are preserved: {error:?}");
    };
    assert_eq!(execution.cause, TurnFailureCause::ProviderAuth);
    assert!(execution.message.contains("invalid api key"));
    assert!(!storage.is_empty());
    let reported = error.to_string();
    assert!(
        reported.contains("invalid api key") && reported.contains("terminal record"),
        "the host report keeps both causes: {reported}"
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        TurnEvent::TurnCompleted { .. } | TurnEvent::TurnFailed { .. }
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event,
                TurnEvent::Diagnostic { code, severity: DiagnosticSeverity::Error, .. }
                    if code == diagnostic_code::STORAGE_FATAL
            ))
            .count(),
        1
    );
}

/// 接受停止与执行期存储故障同时发生：致命失败不改变已接受停止的处置——本轮
/// 停止窗口内未送达的 steer 既不归还也不再交付，处置事件带同一控制身份；
/// 用户先前明确排队的输入原样留队。
#[test]
fn an_accepted_stop_survives_a_fatal_session_failure() {
    use crate::conversation::ConversationError;
    use crate::error::TurnRunError;
    use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};
    use singularity_protocol::{ControlDisposition, TurnEvent, TurnFailureCause};

    let fixture = SessionsFixture::new();
    let provider = Arc::new(ScriptedProvider::new([ScriptedAttempt::tool_call(
        "call-1",
        "read",
        serde_json::json!({"path": "Cargo.toml"}),
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
        conversation.run_turn("go", &mut |event| {
            if matches!(event, TurnEvent::ToolExecutionStart { .. }) && !blocked {
                // 停止窗口内先接受一条 steer，再接受停止；随后把会话文件置为
                // 只读，使工具结果的提交本身失败（副作用已发生、结果无法落盘）。
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
        })
    };
    std::fs::set_permissions(&path, permissions).unwrap();

    assert!(blocked);
    assert!(
        matches!(
            result,
            Err(ConversationError::Turn(TurnRunError::Terminalization {
                execution: Some(ref error),
                storage: None,
            })) if error.cause == TurnFailureCause::Store
        ),
        "the storage failure still stops the chain without a trusted terminal: {result:?}"
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
        "the undelivered steer is dispositioned as cancelled, not handed back as pending"
    );
    let pending = conversation.snapshot().pending_controls;
    assert_eq!(
        pending.len(),
        1,
        "only the input the user explicitly queued stays pending"
    );
    assert_eq!(
        pending[0].control_id,
        queued.lock().unwrap().as_ref().unwrap().control_id
    );
}

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
        conversation.run_turn("go", &mut |event| {
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
        })
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
    let outcome = conversation
        .run_turn("continue after crash", &mut sink)
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

/// 终态已正常提交后的重启测试：验证正常闭合的会话在重开时不执行额外修复，
/// 会话 ledger 与重启前严格逐条一致，终态依然恰好为单条 completed。
#[test]
fn committed_terminal_survives_reopen_without_repair() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let catalog = fixture.catalog();
    let provider = Arc::new(singularity_model::test_support::ScriptedProvider::ok(
        "finished work",
    ));
    let runner = fixture.runner(Some(
        provider as Arc<dyn singularity_model::Provider + Send + Sync>,
    ));
    let thread = catalog
        .create_thread(std::env::current_dir().unwrap().to_str().unwrap(), None)
        .expect("create thread");
    let thread_id = thread.thread_id.clone();
    let cwd = thread.cwd.clone();
    let path = sessions.join(format!("{thread_id}.jsonl"));
    let conversation = Conversation::new(Arc::clone(&runner), thread);
    let mut sink = |_event| {};
    conversation
        .run_turn("do the work", &mut sink)
        .expect("turn completes");

    let before = SessionData::open(&path).expect("reopen before resume");
    let entries_before = before.entries().len();
    let ids_before: Vec<String> = before
        .entries()
        .iter()
        .map(|entry| entry.id().to_string())
        .collect();
    drop(before);

    catalog
        .resume_thread(&thread_id, &cwd)
        .expect("resume a cleanly finished thread");
    assert_eq!(
        catalog
            .read_thread_summary(&thread_id)
            .expect("summary projection")
            .status,
        Some(singularity_protocol::TurnStatus::Completed)
    );

    let after = SessionData::open(&path).expect("reopen after resume");
    assert_eq!(
        after.entries().len(),
        entries_before,
        "a committed terminal outcome is never re-repaired"
    );
    let ids_after: Vec<String> = after
        .entries()
        .iter()
        .map(|entry| entry.id().to_string())
        .collect();
    assert_eq!(ids_before, ids_after, "no entry is rewritten or appended");
    let terminals = after
        .ledger_records()
        .iter()
        .filter(|record| matches!(record, LedgerRecord::OperationFinished { .. }))
        .count();
    assert_eq!(terminals, 1);
}
