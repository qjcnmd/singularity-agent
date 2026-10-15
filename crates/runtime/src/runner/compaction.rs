use super::*;

impl TurnRunner {
    /// 独立压缩占用会话直到摘要、新定义和操作结果保存完毕，不接受用户停止。
    pub(crate) async fn compact_thread(
        self: &Arc<Self>,
        thread: &Thread,
        writer: SessionWriter,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<(), TurnRunError> {
        let runner = Arc::clone(self);
        let start_thread = thread.clone();
        let start_writer = Arc::clone(&writer);
        let mut agent = tokio::task::spawn_blocking(move || {
            let provider = runner.resolve_provider(&start_thread)?;
            let instruction_home = runner.instruction_home();
            let initial_instructions = if lock_writer(&start_writer).latest_request_definitions().is_none() {
                singularity_core::load_agent_instructions(
                    std::path::Path::new(&start_thread.cwd),
                    &instruction_home,
                )
                .map_err(TurnRunError::Preparation)?
            } else {
                None
            };
            let config = AgentConfig { instruction_home, initial_instructions };
            let agent = Agent::new(
                SteeringInbox::default_handle(),
                provider,
                config,
                Arc::clone(&start_writer),
                Arc::clone(&runner.mcp),
            );
            let agent = if runner.user_questions {
                agent.with_user_questions(Arc::new(singularity_agent::agent::UserQuestions::default()))
            } else {
                agent
            };
            lock_writer(&start_writer)
                .append_record(LedgerRecord::OperationStarted { turn_id: None })
                .map_err(|error| TurnRunError::Preparation(error.to_string()))?;
            Ok::<_, TurnRunError>(agent)
        })
        .await
        .expect("compaction preparation completes while the runtime is running")?;
        let mut on_event = |event| match event {
            AgentEvent::ProviderAttempt { observation } => sink(TurnEvent::ProviderAttempt {
                thread_id: thread.thread_id.clone(),
                turn_id: None,
                observation,
            }),
            AgentEvent::Diagnostic(diagnostic) => sink(TurnEvent::Diagnostic {
                thread_id: thread.thread_id.clone(),
                turn_id: None,
                severity: diagnostic.severity,
                code: diagnostic.code,
                message: diagnostic.message,
            }),
            _ => {}
        };
        let outcome = agent.compact_now(&mut on_event).await;
        let (terminal_status, error) = match outcome {
            Ok(_) => (TurnStatus::Completed, None),
            Err(error) => {
                let cause = classify_agent_error(&error);
                let detail = TurnErrorDetail { cause, message: error.to_string() };
                if cause == TurnFailureCause::Store {
                    return Err(TurnRunError::Execution(detail));
                }
                (TurnStatus::Failed, Some(detail))
            }
        };
        append_record_async(
            &writer,
            LedgerRecord::OperationFinished {
                turn_id: None,
                outcome: terminal_status,
                error: error.clone(),
                user_stopped: false,
            },
        )
        .await
        .map_err(|storage| TurnRunError::Terminalization {
            execution: error.clone(),
            storage: storage.to_string(),
        })?;
        match error {
            Some(error) => Err(TurnRunError::Execution(error)),
            None => Ok(()),
        }
    }
}
