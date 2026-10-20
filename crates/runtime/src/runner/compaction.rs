use super::*;

impl TurnRunner {
    /// 独立压缩复用会话写者和停止信号。
    pub(crate) async fn compact_thread(
        self: &Arc<Self>,
        thread: &Thread,
        writer: SessionWriter,
        bash_tasks: Arc<singularity_agent::tools::BashTasks>,
        cancellation: &tokio_util::sync::CancellationToken,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<(), TurnRunError> {
        let runner = Arc::clone(self);
        let start_thread = thread.clone();
        let mut agent = tokio::task::spawn_blocking(move || {
            runner.prepare_agent(
                &start_thread,
                SteeringInbox::default_handle(),
                Arc::new(UserQuestions::default()),
                writer,
                singularity_agent::agent::AgentExecutionContext { bash_tasks, turn_id: String::new() },
            )
        })
        .await
        .expect("compaction preparation completes while the runtime is running")?;
        let mut events = AgentEventProjector::new(None);
        let mut on_event = |event| events.project(sink, event);
        let outcome = agent.compact_now(&mut on_event, cancellation).await;
        if let singularity_agent::agent::CompactionOutcome::Failed(error) = &outcome
            && error.cause() == TurnFailureCause::Store
        {
            return Err(TurnRunError::Execution(error.detail()));
        }
        Ok(())
    }
}
