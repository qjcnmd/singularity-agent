use super::*;

/// 接受停止与终态落盘故障同时发生：终态无法提交时未送达输入同样不归还，
/// 处置事件与持久终态路径一致。
#[test]
fn an_accepted_stop_survives_a_terminal_write_failure() {
    use crate::conversation::ConversationError;
    use crate::error::TurnRunError;
    use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};
    use singularity_protocol::{ProviderAttemptStatus, TurnEvent};

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
    let mut blocked = false;
    let mut queued = None;
    let result = {
        let conversation = Arc::clone(&conversation);
        crate::test_support::run_async(conversation.run_turn("go", &mut |event| {
            if let TurnEvent::ProviderAttempt { observation, .. } = &event
                && observation.status == ProviderAttemptStatus::Error
                && !blocked
            {
                // 真实失败已经确定：接受停止并让终态记录写不进去。
                conversation.steer("cancelled steer").expect("steer is accepted");
                conversation.submit_follow_up("must stay queued").expect("a queued follow-up is accepted");
                queued = conversation.snapshot().pending_input;
                conversation.abort().expect("the stop is accepted");
                let mut readonly = permissions.clone();
                readonly.set_readonly(true);
                std::fs::set_permissions(&path, readonly).unwrap();
                blocked = true;
            }
        }))
    };
    std::fs::set_permissions(&path, permissions).unwrap();

    assert!(blocked);
    assert!(
        matches!(result, Err(ConversationError::Turn(TurnRunError::Terminalization { .. }))),
        "a terminal write failure keeps its own error shape: {result:?}"
    );
    let pending = conversation.snapshot().pending_input.unwrap();
    assert_eq!(pending.control_id, queued.as_ref().unwrap().control_id);
}
