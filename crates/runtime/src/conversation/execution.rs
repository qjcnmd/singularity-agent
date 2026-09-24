use super::*;
use singularity_agent::agent::TurnInbox;
use uuid::Uuid;

impl Conversation {
    /// 执行一轮 turn 直到终态，随后自动按先进先出消费已接受的后续输入：本轮显式输入的那一轮
    /// 先行（此前残留的已接受 followUp 更早执行），turn 到达可信终态
    /// （completed/failed/interrupted）后更新 Thread 投影，再把已接受的 followUp 启动成各自
    /// 有独立 turn id 的新 turn，直到队列清空；执行期间新提交的 followUp 同样会被消费。
    /// 同一时刻只允许一个活动 turn，执行期间客户端从其他线程通过共享的 TurnControls 做
    /// steer 和取消。
    ///
    /// 失败语义：只要终态已经落盘就返回 Ok（失败终态携带
    /// singularity_protocol::TurnErrorDetail，不阻断队列里其余的 followUp）；终态化失败
    /// （没有可信终态）或准备阶段失败返回 Err 并中止链条，没执行的 followUp 原样保留。
    /// 返回值是最后一个到达终态的 turn 结果。
    pub async fn run_turn(
        self: &Arc<Self>,
        input: &str,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnOutcome, ConversationError> {
        let mut reservation = self.reserve_start()?;
        reservation.run(input, sink).await
    }

    pub(super) async fn run_chain(
        self: &Arc<Self>,
        input: ControlRequest,
        input_first: bool,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnOutcome, ConversationError> {
        {
            let mut state = self.lock_state();
            if input_first {
                state.pending_inputs.push_front(input);
            } else {
                state.pending_inputs.push_back(input);
            }
        }
        let mut last = None;
        while let Some(current) = self.take_one_pending_input() {
            let TurnRunResult {
                result,
                undelivered,
                cancel_accepted,
            } = self.run_single_turn(current, sink).await;
            // 归还输入和分类结果都用 Runner 冻结的停止事实：停止会取消本轮未交付的
            // 输入，而先前明确排队的输入仍留在队列里。准备或存储失败会中止链条；
            // 可信的 Failed 终态只要没有停止，就继续消费后续输入。
            if !cancel_accepted {
                self.requeue_inputs(undelivered);
            }
            last = Some(result?);
            if cancel_accepted {
                break;
            }
        }
        #[allow(clippy::expect_used)]
        Ok(last.expect("run_turn executes at least one turn"))
    }

    /// 在预订窗口里执行一次输入，并把 Runner 的完整交接原样返回。写者打开失败和 Runner
    /// 准备失败不做区分：两者都用同一个 TurnRunResult 表达，未被消费的已接受输入随之归还。
    async fn run_single_turn(
        self: &Arc<Self>,
        current: ControlRequest,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> TurnRunResult {
        let conversation = Arc::clone(self);
        let opened = tokio::task::spawn_blocking(move || {
            let _window = conversation.lock_writer_window();
            let thread = {
                let state = conversation.lock_state();
                // 链执行期间始终由 TurnReservation 持有预订窗口；这是内部不变量，
                // 不再另造一套面向并发用户的失败路径。
                assert!(
                    matches!(state.turn, TurnLifecycle::Reserved),
                    "turn chain runs under its reservation"
                );
                state.thread.clone()
            };
            let writer = conversation.runner.open_turn_writer(&thread)?;
            let controls = Arc::new(TurnControls::new(
                Uuid::new_v4().to_string(),
                TurnInbox::default_handle(),
                writer,
            ));
            conversation.lock_state().turn = TurnLifecycle::Running(Arc::clone(&controls));
            Ok::<_, TurnRunError>((thread, controls))
        })
        .await;
        let (thread_snapshot, controls) = match opened {
            Ok(Ok(opened)) => opened,
            Ok(Err(error)) => {
                // 写者还没打开：本轮没有能接受停止的控制面，输入按原规则归还。
                return TurnRunResult {
                    result: Err(error),
                    undelivered: vec![current.unbound()],
                    cancel_accepted: false,
                };
            }
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => {
                return TurnRunResult {
                    result: Err(TurnRunError::Preparation(format!(
                        "turn writer task failed: {error}"
                    ))),
                    undelivered: vec![current.unbound()],
                    cancel_accepted: false,
                };
            }
        };
        let result = self
            .runner
            .run(current, &thread_snapshot, &controls, sink)
            .await;
        {
            // Running → Reserved 的交接在同一个写者窗口内完成：先替换生命周期、释放
            // 本函数持有的控制句柄，旧写者的守卫随之在窗口内关闭。之后任何写者打开
            // （下一轮 turn，或空闲时临时打开）都在本窗口之后观察到已经释放的写者；
            // 控制命令经状态锁串行，不可能跨过交接点还持有旧句柄。
            let _window = self.lock_writer_window();
            let mut state = self.lock_state();
            state.turn = TurnLifecycle::Reserved;
            state.last_context_window = controls.context_window();
            drop(controls);
        }
        result
    }
    /// 测试用的观察入口：生产的控制路径一律在生命周期临界区里借用当前控制面。
    #[cfg(test)]
    pub(crate) fn active_controls(&self) -> Option<Arc<TurnControls>> {
        self.lock_state().turn.controls()
    }

    /// 在状态锁里取下一条待执行输入。
    fn take_one_pending_input(&self) -> Option<ControlRequest> {
        self.lock_state().pending_inputs.pop_front()
    }

    /// 把没执行的输入放回队列，维持「每条待执行输入恰好执行一次」的不变量。归还的输入保留
    /// 原来的 channel、身份和接受序号，但解除 turn 关联：它不再属于任何已开始的 turn，而是
    /// 在下一轮开始时和新的 turn 关联，因此这里也可能包含未交付的 steer。插入在一次状态锁内
    /// 按接受序号完成，channel 不决定等待位置。
    pub(super) fn requeue_inputs(&self, inputs: Vec<ControlRequest>) {
        if inputs.is_empty() {
            return;
        }
        let mut state = self.lock_state();
        for input in inputs {
            insert_by_sequence(&mut state.pending_inputs, input.unbound());
        }
    }
}
