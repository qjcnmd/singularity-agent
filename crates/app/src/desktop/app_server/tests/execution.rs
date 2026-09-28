use super::*;

/// 结算保留执行链的可信终态；历史读取失败由会话读取路径独立呈现，
/// 不再把 Completed 改写成 Failed。
#[test]
fn settlement_keeps_the_trusted_terminal_when_history_cannot_be_read() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([
            singularity_model::test_support::ScriptedAttempt::success("done"),
        ]),
    ));
    let (host, _, id) = session_in(&fixture);
    let slot = host.open_slot(&id).unwrap();
    let mut reservation = slot.conversation().reserve_start().unwrap();
    let outcome =
        singularity_runtime::test_support::run_async(reservation.run("first", &mut |_event| {}))
            .unwrap();
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    std::fs::remove_file(
        fixture
            ._sessions
            .home()
            .join("sessions")
            .join(singularity_agent::session::session_file_name(&id)),
    )
    .unwrap();
    host.on_session_settled(&id, &slot, Some(turn_terminal(Ok(outcome))), reservation);
    assert_eq!(
        slot.runtime_from(&slot.lock_state())
            .terminal
            .expect("terminal")
            .status,
        TurnStatus::Completed,
        "the trusted terminal survives a broken history read"
    );
    assert!(
        host.read_session(&id, 40, None).is_err(),
        "the read-side failure stays visible through the session read path"
    );
    assert_eq!(
        slot.runtime_from(&slot.lock_state())
            .terminal
            .expect("terminal")
            .status,
        TurnStatus::Completed,
        "a failed read must not rewrite the terminal"
    );
}
