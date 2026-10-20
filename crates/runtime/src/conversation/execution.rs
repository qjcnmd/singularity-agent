use super::*;
use uuid::Uuid;

impl Conversation {
    /// 宿主持有会话发布锁时结算，后续提交或后台通知即可预订下一轮。
    pub fn finish_operation(&self) {
        self.lock_state().turn = TurnLifecycle::Idle;
    }

    /// 在预订窗口里执行一次输入；准备与写入错误直接结束执行。
    pub(super) async fn run_single_turn(
        self: &Arc<Self>,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnOutcome, TurnRunError> {
        let conversation = Arc::clone(self);
        let opened = tokio::task::spawn_blocking(move || {
            let (thread, inbox) = {
                let state = conversation.lock_state();
                // 执行期间始终由 OperationReservation 持有预订窗口。
                assert!(matches!(state.turn, TurnLifecycle::Reserved), "turn runs under its reservation");
                (state.thread.clone(), Arc::clone(&state.steering_inbox))
            };
            let writer = Arc::clone(&conversation.writer);
            let controls = Arc::new(TurnControls::new(Uuid::new_v4().to_string(), inbox, writer));
            let prepared =
                conversation
                    .runner
                    .prepare_turn(&thread, &controls, Arc::clone(&conversation.bash_tasks))?;
            // 准备成功后在状态临界区内开放控制窗口。
            let mut state = conversation.lock_state();
            state.last_context_window = Some(prepared.0.context_window());
            state.turn = TurnLifecycle::Running(Arc::clone(&controls));
            Ok::<_, TurnRunError>((controls, prepared))
        })
        .await
        .expect("turn writer completes while the runtime is running");
        let (controls, prepared) = opened?;
        let result = TurnRunner::run(&controls, prepared, sink).await;
        self.lock_state().turn = TurnLifecycle::Reserved;
        result
    }
}
