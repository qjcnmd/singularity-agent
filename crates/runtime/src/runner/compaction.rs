use super::*;

impl TurnRunner {
    /// 在 turn 之外压缩已有的 Thread：以独立的 compaction operation 落盘
    /// （operation_started/operation_finished，不绑定 turn）。`window` 和普通 turn 共用同一个
    /// 停止接受窗口，边界之前接受的停止进入终态裁决，但不会改写真实的失败原因。
    pub(crate) async fn compact_thread(
        self: &Arc<Self>,
        thread: &Thread,
        window: &CancelWindow,
        writer: SessionWriter,
    ) -> Result<(), CompactionRunError> {
        let runner = Arc::clone(self);
        let start_thread = thread.clone();
        let start_writer = Arc::clone(&writer);
        let mut agent = tokio::task::spawn_blocking(move || {
            let (provider, config) = runner
                .resolve_agent_runtime(&start_thread)
                .map_err(CompactionRunError::Preparation)?;
            let agent = Agent::new(
                TurnInbox::default_handle(),
                provider,
                config,
                Arc::clone(&start_writer),
            );
            lock_writer(&start_writer)
                .append_record(LedgerRecord::OperationStarted { turn_id: None })
                .map_err(CompactionRunError::Start)?;
            Ok::<_, CompactionRunError>(agent)
        })
        .await
        .expect("compaction preparation completes while the runtime is running")?;
        let outcome = agent.compact_now(&mut |_| {}, &window.cancellation).await;
        // 测试注入点：只生效一次，取值时就把它取走。
        #[cfg(any(test, feature = "test-support"))]
        if let Some(pause) = self
            .compaction_commit_pause
            .lock()
            .expect("compaction commit pause lock poisoned")
            .take()
        {
            pause();
        }
        // 提交边界：先冻结「是否接受过停止」，再落盘终态。已经接受过停止的压缩和
        // 普通 turn 一样收敛为 Interrupted；取消在 Agent 层已经归约成 Aborted
        // （provider 的 Cancelled 类型到不了这里），其余失败一律 Failed。
        let user_stopped = window.freeze();
        let terminal_status = match &outcome {
            Ok(_) if user_stopped => TurnStatus::Interrupted,
            Ok(_) => TurnStatus::Completed,
            Err(AgentError::Aborted) => TurnStatus::Interrupted,
            Err(_) => TurnStatus::Failed,
        };
        // 独立压缩的失败原因随同一份 operation 终态一起落盘：进程重启后仍能查到这次
        // 压缩为什么失败，而不是只看到一次 provider 请求和一个没有原因的 Failed。
        let error = outcome
            .as_ref()
            .err()
            .filter(|_| terminal_status == TurnStatus::Failed)
            .map(turn_error_detail);
        append_record_async(
            &writer,
            LedgerRecord::OperationFinished {
                turn_id: None,
                outcome: terminal_status,
                error,
                user_stopped,
            },
        )
        .await
        .map_err(CompactionRunError::Terminalization)?;
        Ok(())
    }
}
