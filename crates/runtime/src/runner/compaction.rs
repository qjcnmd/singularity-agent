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
    ) -> Result<CompactionOutcome, CompactionRunError> {
        let runner = Arc::clone(self);
        let start_thread = thread.clone();
        let start_writer = Arc::clone(&writer);
        let started = tokio::task::spawn_blocking(move || {
            let registry = ToolRegistrySnapshot::default();
            let (provider, config, model) = runner
                .resolve_agent_runtime(&start_thread, &registry)
                .map_err(CompactionRunError::Preparation)?;
            let operation_id = Uuid::now_v7().to_string();
            let agent = Agent::new(
                TurnInbox::default_handle(),
                provider,
                model,
                registry,
                config,
                Arc::clone(&start_writer),
            )
            .map_err(CompactionRunError::AgentPreparation)?;
            lock_writer(&start_writer)
                .append_record(LedgerRecord::OperationStarted {
                    operation_id: operation_id.clone(),
                    kind: OperationKind::Compaction,
                    turn_id: None,
                })
                .map_err(CompactionRunError::Start)?;
            Ok::<_, CompactionRunError>((agent, operation_id))
        })
        .await;
        let (mut agent, operation_id) = match started {
            Ok(result) => result?,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => {
                return Err(CompactionRunError::Preparation(TurnRunError::Preparation(
                    format!("compaction preparation task failed: {error}"),
                )));
            }
        };
        let outcome = agent.compact_now(&mut |_| {}, &window.cancellation).await;
        // 测试注入点：只生效一次，取值时就把它取走。
        #[cfg(any(test, feature = "test-support"))]
        #[allow(clippy::expect_used)]
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
                operation_id,
                turn_id: None,
                outcome: terminal_status,
                error: error.clone(),
                user_stopped,
            },
        )
        .await
        .map_err(CompactionRunError::Terminalization)?;
        Ok(CompactionOutcome {
            status: terminal_status,
            reduced: matches!(
                outcome,
                Ok(singularity_agent::compaction::CompactionOutcome::Reduced)
            ),
            error,
        })
    }
}
