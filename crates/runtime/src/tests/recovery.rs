#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! Runner 的存储失败与终态发布测试。

use std::sync::Arc;

use crate::Conversation;
use crate::test_support::SessionsFixture;
use singularity_agent::session::{LedgerRecord, SessionData};
use singularity_model::Provider;

#[test]
fn terminal_write_failure_after_assistant_completion_publishes_no_turn_terminal() {
    use crate::conversation::ConversationError;
    use crate::error::TurnRunError;
    use singularity_protocol::{DiagnosticSeverity, TurnEvent, diagnostic_code};

    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let runner = fixture
        .runner(Some(Arc::new(singularity_model::test_support::ScriptedProvider::ok("finished work"))));
    let thread = fixture.catalog().create_thread(fixture.home().to_str().unwrap(), None).unwrap();
    let path = sessions.join(singularity_agent::session::session_file_name(&thread.thread_id));
    let permissions = std::fs::metadata(&path).unwrap().permissions();
    let conversation = Conversation::new(Arc::clone(&runner), thread);
    let mut events = Vec::new();
    let mut blocked_terminal = false;
    let result = crate::test_support::run_async(conversation.run_turn("go", &mut |event| {
        // assistant 结果已提交；只剩 turn 终态未落盘。
        if matches!(event, TurnEvent::ItemCompleted { .. }) && !blocked_terminal {
            let mut readonly = permissions.clone();
            readonly.set_readonly(true);
            std::fs::set_permissions(&path, readonly).unwrap();
            blocked_terminal = true;
        }
        events.push(event);
    }));
    std::fs::set_permissions(&path, permissions).unwrap();

    assert!(blocked_terminal);
    assert!(matches!(result, Err(ConversationError::Turn(TurnRunError::Terminalization { .. }))));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TurnEvent::TurnCompleted { .. } | TurnEvent::TurnFailed { .. }))
    );
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
    assert!(
        !saved
            .ledger_records()
            .iter()
            .any(|record| matches!(record, LedgerRecord::OperationFinished { .. }))
    );
}

/// 执行期工具结果提交失败（存储故障）测试：副作用已经发生，但结果无法落盘。
/// 断言不产生普通可信终态、链条停止、未执行输入留队。
#[test]
fn session_commit_failure_during_tool_results_stops_the_chain_without_a_trusted_terminal() {
    use crate::conversation::ConversationError;
    use crate::error::TurnRunError;
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
        crate::test_support::run_async(conversation.run_turn("go", &mut |event| {
            // 工具已经执行、结果提交之前把会话文件置为只读：提交本身失败。
            if matches!(event, TurnEvent::ToolExecutionStart { .. }) && !blocked {
                let mut readonly = permissions.clone();
                readonly.set_readonly(true);
                std::fs::set_permissions(&path, readonly).unwrap();
                blocked = true;
                conversation.submit_follow_up("must stay queued").unwrap();
                *queued.lock().unwrap() = conversation.snapshot().pending_input;
            }
            events.push(event);
        }))
    };
    std::fs::set_permissions(&path, permissions).unwrap();

    assert!(blocked);
    assert!(
        matches!(
            result,
            Err(ConversationError::Turn(TurnRunError::Execution(ref error))) if error.cause == TurnFailureCause::Store
        ),
        "an execution-time session failure must not be reported as a trusted terminal: {result:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TurnEvent::TurnCompleted { .. } | TurnEvent::TurnFailed { .. }))
    );
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
    let pending = conversation.snapshot().pending_input.unwrap();
    assert_eq!(pending.control_id, queued.lock().unwrap().as_ref().unwrap().control_id);

    let saved = SessionData::open(&path).unwrap();
    assert!(
        !saved
            .ledger_records()
            .iter()
            .any(|record| matches!(record, LedgerRecord::OperationFinished { .. })),
        "no trusted terminal record is written for an execution-time storage failure"
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
    let result = crate::test_support::run_async(conversation.run_turn("go", &mut |event| {
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
    }));
    std::fs::set_permissions(&path, permissions).unwrap();

    assert!(blocked);
    let Err(ConversationError::Turn(error)) = &result else {
        panic!("terminalization must fail without a trusted terminal: {result:?}");
    };
    let TurnRunError::Terminalization { execution: Some(execution), storage } = error else {
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
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TurnEvent::TurnCompleted { .. } | TurnEvent::TurnFailed { .. }))
    );
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
    use singularity_protocol::{TurnEvent, TurnFailureCause};

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
        crate::test_support::run_async(conversation.run_turn("go", &mut |event| {
            if matches!(event, TurnEvent::ToolExecutionStart { .. }) && !blocked {
                // 停止窗口内先接受一条 steer，再接受停止；随后把会话文件置为
                // 只读，使工具结果的提交本身失败（副作用已发生、结果无法落盘）。
                conversation.steer("cancelled steer").expect("steer is accepted");
                conversation.submit_follow_up("must stay queued").expect("a queued follow-up is accepted");
                *queued.lock().unwrap() = conversation.snapshot().pending_input;
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
            Err(ConversationError::Turn(TurnRunError::Execution(ref error))) if error.cause == TurnFailureCause::Store
        ),
        "the storage failure still stops the chain without a trusted terminal: {result:?}"
    );
    let pending = conversation.snapshot().pending_input.unwrap();
    assert_eq!(pending.control_id, queued.lock().unwrap().as_ref().unwrap().control_id);
}

mod stop_and_replay;
