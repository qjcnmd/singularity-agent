use super::*;
use uuid::Uuid;

impl Conversation {
    /// 执行一轮 turn 直到终态。
    /// 同一时刻只允许一个活动 turn，执行期间客户端从其他线程经共享的 TurnControls 做
    /// steer 和取消。
    ///
    /// 终态落盘后返回 Ok，失败终态携带 singularity_protocol::TurnErrorDetail；
    /// 准备或终态提交失败返回 Err。
    pub async fn run_turn(
        self: &Arc<Self>,
        input: &str,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnOutcome, ConversationError> {
        let (result, _guard) = self.reserve_start(input)?.execute(sink).await;
        match result {
            OperationResult::Turn(result) => result,
            OperationResult::Compaction(_) => unreachable!("reserved a turn"),
        }
    }

    /// 在预订窗口里执行一次输入；准备与写入错误直接结束执行。
    pub(super) async fn run_single_turn(
        self: &Arc<Self>,
        input: ControlRequest,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnOutcome, TurnRunError> {
        let conversation = Arc::clone(self);
        let opened = tokio::task::spawn_blocking(move || {
            let _window = conversation.lock_writer_window();
            let (thread, inbox) = {
                let state = conversation.lock_state();
                // 执行期间始终由 OperationReservation 持有预订窗口。
                assert!(matches!(state.turn, TurnLifecycle::Reserved), "turn runs under its reservation");
                (state.thread.clone(), Arc::clone(&state.steering_inbox))
            };
            let writer = conversation.runner.open_turn_writer(&thread)?;
            let controls = Arc::new(TurnControls::new(Uuid::new_v4().to_string(), inbox, writer));
            let prepared = conversation.runner.prepare_turn(&thread, &controls)?;
            // 准备成功后在状态临界区内开放控制窗口。
            let mut state = conversation.lock_state();
            controls.lock_inbox().open();
            state.last_context_window = Some(prepared.0.context_window());
            state.turn = TurnLifecycle::Running(Arc::clone(&controls));
            Ok::<_, TurnRunError>((thread, controls, input, prepared))
        })
        .await
        .expect("turn writer completes while the runtime is running");
        let (thread_snapshot, controls, current, prepared) = opened?;
        let result = TurnRunner::run(current, &thread_snapshot, &controls, prepared, sink).await;
        {
            // Running → Reserved 的交接在同一个写者窗口内完成：先替换生命周期，再释放
            // 本函数持有的控制句柄，旧写者的守卫随之关闭。之后的写者打开（下一轮 turn，
            // 或空闲时临时打开）都会看到已释放的写者；控制命令经状态锁串行，不可能跨过
            // 交接点持有旧句柄。
            let _window = self.lock_writer_window();
            let mut state = self.lock_state();
            state.turn = TurnLifecycle::Reserved;
            drop(controls);
        }
        result
    }
}
