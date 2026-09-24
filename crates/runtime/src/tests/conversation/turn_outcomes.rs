use super::*;

#[test]
fn resume_thread_conflicts_with_active_writer_and_succeeds_after_release() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let thread_id = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    let shared = coordinator();
    let session = SessionManager::create_with_id_with_coordinator(
        Path::new("."),
        &sessions,
        thread_id,
        &shared,
    )
    .expect("create session file");

    // 同一会话已有存活写者（模拟另一进程持有锁）：resume 必须快速失败。
    let cwd = session.cwd_string();
    let catalog = ThreadCatalog::new(sessions, shared);
    assert!(matches!(
        catalog.resume_thread(thread_id, &cwd),
        Err(crate::thread_catalog::CatalogError::WriterActive)
    ));

    // 写者释放后 resume 恢复正常。
    drop(session);
    let resumed = catalog
        .resume_thread(thread_id, &cwd)
        .expect("resume after release");
    assert_eq!(resumed.thread_id, thread_id);
}

#[test]
fn reused_provider_tool_ids_have_distinct_live_and_historical_items() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let first = fixture.home().join("first.txt");
    let second = fixture.home().join("second.txt");
    std::fs::write(&first, "first output").unwrap();
    std::fs::write(&second, "second output").unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::tool_call("reused", "read", serde_json::json!({"path": first})),
        ScriptedAttempt::tool_call("reused", "read", serde_json::json!({"path": second})),
        ScriptedAttempt::success("done"),
    ]));
    let conversation = new_conversation(&fixture, provider.clone(), None);
    let mut completed = Vec::new();
    crate::test_support::run_async(conversation.run_turn("read both", &mut |event| {
        if let TurnEvent::ToolExecutionEnd { item, output, .. } = event {
            completed.push((item.item_id, output));
        }
    }))
    .unwrap();
    assert_eq!(completed.len(), 2);
    assert_ne!(completed[0].0, completed[1].0);
    let catalog = ThreadCatalog::new(sessions, Arc::clone(&fixture.coordinator));
    let snapshot = catalog
        .read_snapshot(&conversation.thread().thread_id)
        .unwrap();
    let page = snapshot.page(40, None).unwrap();
    let results = page
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .filter_map(|item| {
            if let singularity_protocol::HistoryItem::ToolResult { id, output, .. } = item {
                Some((id.as_str(), output.as_str()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        results,
        vec![
            (completed[0].0.as_str(), "first output"),
            (completed[1].0.as_str(), "second output")
        ]
    );
    let requests = provider.requests();
    let raw_ids = requests[2]
        .messages
        .iter()
        .filter_map(|message| message.tool_call_id.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(
        raw_ids,
        vec!["reused", "reused"],
        "provider replay keeps its original wire IDs"
    );
}

/// model_configuration 可变的 scripted provider：模拟配置刷新只改变后续
/// turn 解析出的有效窗口，用于核对活动 turn 的冻结事实不受其影响。
struct MutableLimitsProvider {
    inner: ScriptedProvider,
    context_tokens: std::sync::atomic::AtomicU32,
}

impl MutableLimitsProvider {
    fn set_context_tokens(&self, tokens: u32) {
        self.context_tokens
            .store(tokens, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Provider for MutableLimitsProvider {
    fn model_configuration(&self) -> singularity_model::ModelConfigurationSnapshot {
        singularity_model::ModelConfigurationSnapshot {
            max_context_tokens: self
                .context_tokens
                .load(std::sync::atomic::Ordering::SeqCst),
            max_output_tokens: 4_096,
        }
    }

    fn complete_stream<'a>(
        &'a self,
        request: &'a singularity_model::ModelTurnRequest,
        cancellation: &'a tokio_util::sync::CancellationToken,
        observer: &'a mut dyn singularity_model::ProviderObserver,
    ) -> singularity_model::ProviderFuture<'a> {
        Provider::complete_stream(&self.inner, request, cancellation, observer)
    }
}

/// 运行中的 turn 报告其冻结的有效上下文窗口：配置刷新不改变当前执行的
/// 解释，后续执行采用新值；空闲后保留最近一次执行的事实。
#[test]
fn running_turn_keeps_its_frozen_window_across_configuration_refresh() {
    use std::sync::atomic::AtomicU32;

    let fixture = SessionsFixture::new();
    let provider = Arc::new(MutableLimitsProvider {
        inner: ScriptedProvider::new([
            ScriptedAttempt::success("first"),
            ScriptedAttempt::success("second"),
        ]),
        context_tokens: AtomicU32::new(100_000),
    });
    let (gated, started) = GatedProvider::new(provider.clone());
    let (release, release_receiver) = std::sync::mpsc::channel::<()>();
    gated.with_release(release_receiver);
    let conversation = new_conversation(&fixture, gated, None);
    let sink = |_event: TurnEvent| {};
    let running = {
        let conversation = Arc::clone(&conversation);
        let mut sink = sink;
        std::thread::spawn(move || {
            crate::test_support::run_async(conversation.run_turn("first", &mut sink)).unwrap()
        })
    };
    started
        .recv()
        .expect("the first request reaches the provider");
    assert_eq!(
        conversation.snapshot().model_context_window,
        Some(100_000),
        "the running turn reports the window frozen at its start"
    );
    provider.set_context_tokens(200_000);
    assert_eq!(
        conversation.snapshot().model_context_window,
        Some(100_000),
        "a configuration refresh never reinterprets the running execution"
    );
    release.send(()).expect("release the gated request");
    let outcome = running.join().unwrap();
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    assert_eq!(
        conversation.snapshot().model_context_window,
        Some(100_000),
        "the latest executed turn's window stays observable while idle"
    );
    crate::test_support::run_async(conversation.run_turn("second", &mut |_event: TurnEvent| {}))
        .unwrap();
    assert_eq!(
        conversation.snapshot().model_context_window,
        Some(200_000),
        "the next execution resolves and freezes the refreshed configuration"
    );
}

/// 首次请求成功并携带 usage（调用未注册工具迫使循环续接），第二次请求失败：
/// 失败终态事件必须报告本轮已记录的 usage（回归：失败终态曾以空 usage 出口）。
#[test]
fn failed_turn_reports_usage_recorded_before_the_failure() {
    let fixture = SessionsFixture::new();
    let provider = ScriptedProvider::new([
        ScriptedAttempt::ToolCalls {
            text: "calling a tool".to_string(),
            calls: vec![singularity_model::ModelToolCall {
                tool_call_id: "call-1".to_string(),
                tool_name: "definitely-not-a-registered-tool".to_string(),
                arguments: serde_json::json!({}),
            }],
            usage: Some(singularity_model::ModelUsage {
                input_tokens: 10,
                output_tokens: 32,
                total_tokens: 42,
                usage_present: true,
                ..Default::default()
            }),
        },
        // 网络错误可自动重试：按重试预算（initial + 2 retries）逐次失败收敛。
        ScriptedAttempt::failure_kind(ModelErrorKind::NetworkError, "connection reset"),
        ScriptedAttempt::failure_kind(ModelErrorKind::NetworkError, "connection reset"),
        ScriptedAttempt::failure_kind(ModelErrorKind::NetworkError, "connection reset"),
    ]);
    let conversation = new_conversation(&fixture, Arc::new(provider), None);
    let mut sink = |_event: TurnEvent| {};
    let outcome = crate::test_support::run_async(conversation.run_turn("go", &mut sink))
        .expect("a converged failed terminal is a trusted Ok outcome");
    assert_eq!(outcome.turn_status, TurnStatus::Failed);
    let usage = outcome
        .usage
        .usage_present
        .then_some(&outcome.usage)
        .expect("the failed outcome carries the usage recorded before the failure");
    assert_eq!(usage.total_tokens, 42);
    let error = outcome
        .error
        .expect("failed terminal carries protocol error detail");
    assert_eq!(
        error.cause,
        crate::TurnFailureCause::ProviderNetwork,
        "the error detail names the real provider cause"
    );
}

/// 读取指定 thread 会话文件的全部 ledger 记录（只读，不修复）。
#[test]
fn interruption_at_tool_boundary_converges_interrupted_and_next_input_runs() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::tool_call(
            "call-bash",
            "bash",
            serde_json::json!({"command": "echo ready; sleep 30"}),
        ),
        ScriptedAttempt::success("next turn done"),
    ]));
    let conversation = new_conversation(
        &fixture,
        provider as Arc<dyn Provider + Send + Sync>,
        Some("openai_compatible/base-model"),
    );
    let thread_id = conversation.thread().thread_id;

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let ready_tx = std::sync::Mutex::new(Some(ready_tx));
    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            let mut sink = move |event: TurnEvent| {
                if let TurnEvent::ToolExecutionUpdate {
                    ref partial_result, ..
                } = event
                    && partial_result.contains("ready")
                    && let Some(sender) = ready_tx.lock().expect("ready lock").take()
                {
                    let _ = sender.send(());
                }
            };
            let outcome = crate::test_support::run_async(
                conversation.run_turn("run a long command", &mut sink),
            );
            (conversation, outcome)
        })
    };
    ready_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("tool is executing and has streamed output");
    conversation.abort().expect("abort active turn");
    let (conversation, outcome) = worker.join().expect("worker");

    let outcome = outcome.expect("tool-boundary interruption converges as interrupted");
    assert_eq!(outcome.turn_status, TurnStatus::Interrupted);

    let session = SessionData::open(&sessions.join(format!("{thread_id}.jsonl"))).expect("reopen");
    let records = session.ledger_records();
    let aborted_results = session
        .entries()
        .iter()
        .filter(|entry| {
            matches!(entry,
                singularity_agent::session::SessionEntry::Message { message, .. }
                    if matches!(message, singularity_agent::message::AgentMessage::ToolResult { .. })
                        && message.content_text().contains("Operation aborted"))
        })
        .count();
    assert_eq!(
        aborted_results, 1,
        "the interrupted tool closes with exactly one model-visible failure"
    );
    let terminals: Vec<_> = records
        .iter()
        .filter(|record| {
            matches!(
                record,
                singularity_agent::session::LedgerRecord::OperationFinished { .. }
            )
        })
        .collect();
    assert_eq!(terminals.len(), 1, "exactly one durable terminal outcome");
    assert!(matches!(
        terminals[0],
        singularity_agent::session::LedgerRecord::OperationFinished {
            outcome: TurnStatus::Interrupted,
            ..
        }
    ));

    // 中断不破坏协调器：下一条输入作为新 turn 正常完成。
    let mut sink = |_event: TurnEvent| {};
    let next = crate::test_support::run_async(conversation.run_turn("continue", &mut sink))
        .expect("next input runs after a tool-boundary interruption");
    assert_eq!(next.turn_status, TurnStatus::Completed);
    let completed = SessionData::open(&sessions.join(format!("{thread_id}.jsonl")))
        .expect("reopen completed turn");
    assert!(completed.entries().iter().any(|entry| matches!(entry,
        singularity_agent::session::SessionEntry::Message { message, .. }
        if message.content_text() == "next turn done"
    )));
}

#[test]
fn settings_survive_reopen_without_a_turn_and_failed_saves_preserve_selection() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let conversation = new_conversation(
        &fixture,
        Arc::new(ScriptedProvider::ok("ok")),
        Some("openai_compatible/base-model"),
    );
    let id = conversation.thread().thread_id;
    conversation
        .update_settings("openai_compatible/base-model-2")
        .unwrap();
    let catalog = ThreadCatalog::new(sessions, Arc::clone(&fixture.coordinator));
    let cwd = conversation.thread().cwd;
    assert_eq!(
        catalog.resume_thread(&id, &cwd).unwrap().model.as_deref(),
        Some("openai_compatible/base-model-2")
    );
    let writer = conversation
        .runner_handle()
        .open_turn_writer(&conversation.thread())
        .unwrap();
    let failed = conversation.update_settings("openai_compatible/base-model");
    assert!(failed.is_err());
    assert_eq!(
        conversation.thread().model.as_deref(),
        Some("openai_compatible/base-model-2")
    );
    drop(writer);
    assert_eq!(
        catalog.resume_thread(&id, &cwd).unwrap().model,
        conversation.thread().model
    );
}

#[test]
fn interrupted_output_reloads_for_display_without_entering_the_next_request() {
    let fixture = SessionsFixture::new();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::visible_then_fail(
            "interrupted output",
            ProviderError::new(ModelErrorKind::JsonSchemaViolation, "invalid stream"),
        ),
        ScriptedAttempt::success("next answer"),
    ]));
    let conversation = new_conversation(&fixture, provider.clone(), None);
    let outcome =
        crate::test_support::run_async(conversation.run_turn("first", &mut |_| {})).unwrap();
    assert_eq!(outcome.turn_status, TurnStatus::Failed);
    let thread_id = conversation.thread().thread_id;
    let snapshot = fixture.catalog().read_snapshot(&thread_id).unwrap();
    let page = snapshot.page(10, None).unwrap();
    assert!(
        page.turns
            .iter()
            .flat_map(|turn| &turn.items)
            .any(|item| matches!(item,
        singularity_protocol::HistoryItem::Message { role, text, .. }
        if role == "assistant" && text == "interrupted output"))
    );
    crate::test_support::run_async(conversation.run_turn("continue", &mut |_| {})).unwrap();
    assert!(
        !provider.requests()[1]
            .messages
            .iter()
            .any(|message| message.content.contains("interrupted output"))
    );
}
