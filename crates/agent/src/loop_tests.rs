use super::*;
use crate::message::AgentMessage;
use crate::session::SessionEntry;
use serde_json::{Value, json};
use singularity_model::{
    ModelMessage, ModelToolCall, ModelToolParseStatus, ProviderApiProtocol, ProviderAttemptEvent,
    ProviderAttemptStarted, ProviderReasoningReplay, ProviderStreamingCapability,
};
use std::collections::VecDeque;
use std::sync::Mutex;
struct FakeProvider {
    steps: Mutex<VecDeque<FakeStep>>,
    requests: Mutex<Vec<ModelTurnRequest>>,
    contract: ProviderProtocolContract,
}
struct FakeStep {
    text: String,
    tool_calls: Vec<ModelToolCall>,
    usage: ModelUsage,
}
impl FakeProvider {
    fn new(contract: ProviderProtocolContract, steps: Vec<FakeStep>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            requests: Mutex::new(Vec::new()),
            contract,
        }
    }

    fn pop(&self) -> std::result::Result<FakeStep, ProviderError> {
        self.steps.lock().unwrap().pop_front().ok_or_else(|| {
            ProviderError::from_model_error(ModelError::new(
                ModelErrorKind::UnknownProviderError,
                "no scripted steps remaining",
            ))
        })
    }

    fn respond(&self, request: &ModelTurnRequest, step: &FakeStep) -> ModelTurnResponse {
        let mut assistant = ModelMessage::assistant_tool_calls(step.tool_calls.clone());
        assistant.content = step.text.clone();
        ModelTurnResponse {
            request_id: request.request_id.clone(),
            response_id: format!("resp-{}", Uuid::new_v4().simple()),
            status: ModelTurnStatus::Success,
            assistant_message: Some(assistant),
            usage: step.usage.clone(),
            finish_reason: Some(if step.tool_calls.is_empty() {
                "stop".to_string()
            } else {
                "tool_calls".to_string()
            }),
            validation: None,
            error: None,
            provider_name: Some("fake".to_string()),
            model_name: Some("fake-model".to_string()),
            provider_reasoning_history: Vec::new(),
        }
    }
}
impl Provider for FakeProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        self.contract.clone()
    }

    fn streaming_capability(
        &self,
        _selected_protocol: singularity_model::ProviderApiProtocol,
    ) -> ProviderStreamingCapability {
        ProviderStreamingCapability::OutputTextDelta
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> std::result::Result<ModelTurnResponse, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        let step = self.pop()?;
        if !step.text.is_empty() {
            on_event(ProviderStreamEvent::OutputTextDelta {
                delta: step.text.clone(),
            });
        }
        Ok(self.respond(request, &step))
    }

    fn complete_stream_observed(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
    ) -> std::result::Result<ModelTurnResponse, ProviderError> {
        on_attempt(ProviderAttemptEvent::Started(ProviderAttemptStarted {
            provider_name: "fake".to_string(),
            model_name: "fake-model".to_string(),
            actual_api_protocol: ProviderApiProtocol::Declared,
            started_at_unix_ms: 1,
        }));
        self.complete_stream(request, cancellation, on_event)
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurnResponse, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        let step = self.pop()?;
        Ok(self.respond(request, &step))
    }
}
fn fake_contract() -> ProviderProtocolContract {
    ProviderProtocolContract {
        supports_tools: true,
        supports_strict_tool_schema: false,
        tool_reasoning_mode: singularity_model::ProviderToolReasoningMode::Unspecified,
        max_tools_per_request: 8,
        supports_system_message: false,
        max_context_tokens: Some(128_000),
        max_output_tokens: 4_096,
    }
}
fn tool_call(id: &str, name: &str, args: Value) -> ModelToolCall {
    ModelToolCall {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        arguments: args.clone(),
        raw_arguments: serde_json::to_string(&args).unwrap(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    }
}
fn usage(input: u64, output: u64) -> ModelUsage {
    ModelUsage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input + output,
        cached_input_tokens: 0,
        reasoning_tokens: 0,
        usage_present: true,
    }
}

fn compaction_test_config() -> AgentConfig {
    AgentConfig {
        system_prompt: String::new(),
        context_window: 6_000,
        max_output_tokens: 10,
        compaction: CompactionConfig {
            reserve_tokens: 1_000,
            retain_ratio: 0.005,
            summary_max_tokens: 10,
        },
        ..AgentConfig::default()
    }
}
fn setup(steps: Vec<FakeStep>) -> (Agent, tempfile::TempDir, Arc<FakeProvider>) {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let provider = Arc::new(FakeProvider::new(fake_contract(), steps));
    let agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig::default(),
        session,
    )
    .unwrap();
    (agent, dir, provider)
}
type OnCallHook = Box<dyn Fn(usize) + Send>;
struct ErrReturningProvider {
    /// 每次 `complete_stream` 弹出的结果：`Err(provider_error)` 或 `Ok(text)`。
    steps: Mutex<VecDeque<std::result::Result<String, ProviderError>>>,
    calls: std::sync::atomic::AtomicUsize,
    contract: ProviderProtocolContract,
    on_call: Mutex<Option<OnCallHook>>,
}
impl ErrReturningProvider {
    fn new(
        contract: ProviderProtocolContract,
        steps: Vec<std::result::Result<String, ModelError>>,
    ) -> Self {
        Self::with_provider_errors(
            contract,
            steps
                .into_iter()
                .map(|step| step.map_err(ProviderError::from_model_error))
                .collect(),
        )
    }

    fn with_provider_errors(
        contract: ProviderProtocolContract,
        steps: Vec<std::result::Result<String, ProviderError>>,
    ) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            contract,
            on_call: Mutex::new(None),
        }
    }

    fn with_on_call(self, hook: Box<dyn Fn(usize) + Send>) -> Self {
        *self.on_call.lock().unwrap() = Some(hook);
        self
    }

    fn try_respond(
        &self,
        request: &ModelTurnRequest,
    ) -> std::result::Result<ModelTurnResponse, ProviderError> {
        let call_index = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(hook) = self.on_call.lock().unwrap().as_ref() {
            hook(call_index);
        }
        match self.steps.lock().unwrap().pop_front() {
            Some(Err(error)) => Err(error),
            Some(Ok(text)) => {
                let mut response = ModelTurnResponse::completed(
                    request.request_id.clone(),
                    format!("ok-{}", Uuid::new_v4().simple()),
                    text,
                );
                response.usage = usage(10, 2);
                Ok(response)
            }
            // 脚本耗尽：视作瞬时类网络错误，触发重试直至次数耗尽。
            None => Err(ProviderError::from_model_error(ModelError::new(
                ModelErrorKind::NetworkError,
                "no scripted steps remaining",
            ))),
        }
    }
}
impl Provider for ErrReturningProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        self.contract.clone()
    }

    fn streaming_capability(
        &self,
        _selected_protocol: singularity_model::ProviderApiProtocol,
    ) -> ProviderStreamingCapability {
        ProviderStreamingCapability::OutputTextDelta
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
        _on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> std::result::Result<ModelTurnResponse, ProviderError> {
        self.try_respond(request)
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurnResponse, ProviderError> {
        self.try_respond(request)
    }
}

/// 可重试 provider 错误（NetworkError）重试上限内成功：agent 调用 3 次后成功收敛。
#[test]
fn retryable_provider_error_retries_then_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let provider = Arc::new(ErrReturningProvider::new(
        fake_contract(),
        vec![
            Err(ModelError::new(
                ModelErrorKind::NetworkError,
                "net failure 1",
            )),
            Err(ModelError::new(
                ModelErrorKind::NetworkError,
                "net failure 2",
            )),
            Ok("success".to_string()),
        ],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig {
            retry: TurnRetryConfig {
                max_retries: 3,
                base_delay_ms: 1,
            },
            ..AgentConfig::default()
        },
        session,
    )
    .unwrap();
    let outcome = agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .expect("retryable errors must recover after retries");
    assert_eq!(outcome.turns, 1);
    assert_eq!(outcome.terminal_reason, AgentTerminalReason::Completed);
    // 前两次失败 + 一次成功 = 3 次调用。
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
}

/// 可重试 provider 错误耗尽后收敛为失败（无 progress 时原样传播 Provider 错误）。
#[test]
fn retryable_provider_error_exhausts_and_fails() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let provider = Arc::new(ErrReturningProvider::new(
        fake_contract(),
        vec![
            Err(ModelError::new(
                ModelErrorKind::NetworkError,
                "net failure 1",
            )),
            Err(ModelError::new(
                ModelErrorKind::NetworkError,
                "net failure 2",
            )),
            Err(ModelError::new(
                ModelErrorKind::NetworkError,
                "net failure 3",
            )),
        ],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig {
            retry: TurnRetryConfig {
                max_retries: 2,
                base_delay_ms: 1,
            },
            ..AgentConfig::default()
        },
        session,
    )
    .unwrap();
    let error = agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .expect_err("retryable errors must exhaust after max retries");
    assert!(
        error.to_string().contains("net failure 3"),
        "exhausted retries must propagate the final provider error, got: {error}"
    );
    // 初始调用 + 2 次重试耗尽 = 3 次调用。
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[test]
fn persistent_rate_limit_makes_at_most_four_provider_calls() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let provider = Arc::new(ErrReturningProvider::new(
        fake_contract(),
        vec![
            Err(ModelError::new(
                ModelErrorKind::RateLimited,
                "rate limited 1",
            )),
            Err(ModelError::new(
                ModelErrorKind::RateLimited,
                "rate limited 2",
            )),
            Err(ModelError::new(
                ModelErrorKind::RateLimited,
                "rate limited 3",
            )),
            Err(ModelError::new(
                ModelErrorKind::RateLimited,
                "rate limited 4",
            )),
        ],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig {
            retry: TurnRetryConfig {
                max_retries: 3,
                base_delay_ms: 1,
            },
            ..AgentConfig::default()
        },
        session,
    )
    .unwrap();

    agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .expect_err("persistent rate limiting exhausts the turn retry budget");
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 4);
}

#[test]
fn retry_after_controls_the_agent_retry_wait() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let provider = Arc::new(ErrReturningProvider::with_provider_errors(
        fake_contract(),
        vec![
            Err(ProviderError::from_model_error(ModelError::new(
                ModelErrorKind::RateLimited,
                "rate limited",
            ))
            .with_retry_after(Some(std::time::Duration::from_millis(80)))),
            Ok("success".to_string()),
        ],
    ));
    let mut agent = Agent::new(
        provider,
        ToolRegistry::new(),
        AgentConfig {
            retry: TurnRetryConfig {
                max_retries: 1,
                base_delay_ms: 1,
            },
            ..AgentConfig::default()
        },
        session,
    )
    .unwrap();

    let started = std::time::Instant::now();
    agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .expect("retry succeeds");
    assert!(started.elapsed() >= std::time::Duration::from_millis(70));
}

#[test]
fn replay_unsafe_provider_error_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let provider = Arc::new(ErrReturningProvider::with_provider_errors(
        fake_contract(),
        vec![Err(ProviderError::from_model_error(ModelError::new(
            ModelErrorKind::NetworkError,
            "stream failed after visible output",
        ))
        .without_automatic_retry())],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig {
            retry: TurnRetryConfig {
                max_retries: 3,
                base_delay_ms: 1,
            },
            ..AgentConfig::default()
        },
        session,
    )
    .unwrap();

    agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .expect_err("a replay-unsafe failure is terminal");
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn cancellation_interrupts_retry_after_wait() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let provider = Arc::new(ErrReturningProvider::with_provider_errors(
        fake_contract(),
        vec![Err(ProviderError::from_model_error(ModelError::new(
            ModelErrorKind::RateLimited,
            "rate limited",
        ))
        .with_retry_after(Some(std::time::Duration::from_secs(5))))],
    ));
    let mut agent = Agent::new(
        provider,
        ToolRegistry::new(),
        AgentConfig::default(),
        session,
    )
    .unwrap();
    let cancellation = CancellationToken::new();
    let trigger = cancellation.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(40));
        trigger.cancel();
    });

    let started = std::time::Instant::now();
    let outcome = agent
        .run("task", &mut AgentEvents::new(), &cancellation)
        .expect("cancellation converges to an aborted outcome");
    canceller.join().unwrap();
    assert_eq!(outcome.terminal_reason, AgentTerminalReason::Aborted);
    assert!(started.elapsed() < std::time::Duration::from_millis(500));
}

/// 2. 工具调用序列：tool call → 工具执行 → 结果回写 session → 下一轮。
#[test]
fn tool_call_executes_and_results_feed_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let provider = Arc::new(FakeProvider::new(
        fake_contract(),
        vec![
            FakeStep {
                text: String::new(),
                tool_calls: vec![tool_call(
                    "call_1",
                    "write",
                    json!({ "path": "hello.txt", "content": "hello" }),
                )],
                usage: usage(50, 10),
            },
            FakeStep {
                text: "done".to_string(),
                tool_calls: Vec::new(),
                usage: usage(120, 20),
            },
        ],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig::default(),
        session,
    )
    .unwrap();
    let mut events = AgentEvents::new();
    let mut started: Vec<(String, String)> = Vec::new();
    let on_event = &mut |event: AgentEvent| {
        if let AgentEvent::ToolExecutionStarted {
            tool_name,
            arguments,
            ..
        } = event
        {
            started.push((tool_name, arguments.to_string()));
        }
    };
    events.on_event = Some(on_event);
    let outcome = agent
        .run("create hello.txt", &mut events, &CancellationToken::new())
        .unwrap();
    assert_eq!(outcome.turns, 2);
    assert_eq!(outcome.final_text, "done");
    assert_eq!(outcome.usage.input_tokens, 170);
    assert_eq!(outcome.usage.output_tokens, 30);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].0, "write");
    // 工具真实执行：文件已创建。
    assert_eq!(
        std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
        "hello"
    );
    // 第二轮请求上下文重放 assistant tool_calls（session 投影）→ 真实 wire 形态。
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let second = &requests[1];
    assert_eq!(second.messages[1].role, ModelRole::Assistant);
    assert_eq!(second.messages[1].tool_calls.len(), 1);
    assert_eq!(second.messages[1].tool_calls[0].tool_call_id, "call_1");
    assert_eq!(second.messages[1].tool_calls[0].tool_name, "write");
    assert_eq!(second.messages[2].role, ModelRole::Tool);
    assert_eq!(second.messages[2].tool_call_id.as_deref(), Some("call_1"));
    assert!(second.messages[2].content.contains("Successfully wrote"));
}

#[test]
fn tool_events_pair_around_each_serial_execution_and_preflight_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let provider = Arc::new(FakeProvider::new(
        fake_contract(),
        vec![
            FakeStep {
                text: String::new(),
                tool_calls: vec![
                    tool_call(
                        "call_write_1",
                        "write",
                        json!({ "path": "one.txt", "content": "one" }),
                    ),
                    tool_call("call_rejected", "unknown_tool", json!({})),
                    tool_call(
                        "call_write_2",
                        "write",
                        json!({ "path": "two.txt", "content": "two" }),
                    ),
                ],
                usage: usage(50, 10),
            },
            FakeStep {
                text: "done".to_string(),
                tool_calls: Vec::new(),
                usage: usage(120, 20),
            },
        ],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig::default(),
        session,
    )
    .unwrap();
    let mut observed = Vec::new();
    let mut events = AgentEvents::new();
    let mut on_event = |event| match event {
        AgentEvent::ToolExecutionStarted { tool_call_id, .. } => {
            observed.push(format!("start:{tool_call_id}"));
        }
        AgentEvent::ToolExecutionEnded {
            tool_call_id,
            execution,
            ..
        } => {
            observed.push(format!("end:{tool_call_id}:{}", execution.is_error));
        }
        _ => {}
    };
    events.on_event = Some(&mut on_event);

    let outcome = agent
        .run("run the batch", &mut events, &CancellationToken::new())
        .unwrap();

    assert_eq!(outcome.final_text, "done");
    assert_eq!(
        observed,
        [
            "start:call_write_1",
            "end:call_write_1:false",
            "start:call_rejected",
            "end:call_rejected:true",
            "start:call_write_2",
            "end:call_write_2:false",
        ]
    );
    let requests = provider.requests.lock().unwrap();
    let rejected_result = requests[1]
        .messages
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("call_rejected"))
        .expect("preflight rejection must remain model-visible");
    assert!(rejected_result.content.contains("tool execution failed"));
}

/// 4. 停止窗口内到达的转向输入：文本响应后箱内非空 → 继续一轮再停止。
#[test]
fn steer_at_stop_continues_one_more_turn() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let inbox_slot: Arc<Mutex<Option<TurnInboxHandle>>> = Arc::new(Mutex::new(None));
    let hook_slot = Arc::clone(&inbox_slot);
    let provider = Arc::new(
        ErrReturningProvider::new(
            fake_contract(),
            vec![
                Ok("first answer".to_string()),
                Ok("second answer".to_string()),
            ],
        )
        .with_on_call(Box::new(move |call_index| {
            // 仅第一次调用进行中注入：输入落在自然停止窗口内，代理取出后
            // 继续一轮再停止。
            if call_index == 0
                && let Some(inbox) = hook_slot.lock().unwrap().as_ref()
            {
                inbox.lock().unwrap().enqueue("please continue");
            }
        })),
    );
    let mut agent = Agent::new(
        provider,
        ToolRegistry::new(),
        AgentConfig::default(),
        session,
    )
    .unwrap();
    *inbox_slot.lock().unwrap() = Some(agent.inbox_handle());
    let outcome = agent
        .run(
            "question",
            &mut AgentEvents::new(),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(outcome.turns, 2);
    assert_eq!(outcome.final_text, "second answer");
    assert!(
        agent.session.entries().iter().any(|entry| matches!(
            &entry,
            SessionEntry::Message { message, .. }
                if message.role() == AgentMessageRole::User
                    && message.content_text() == "please continue"
        )),
        "stop-window input must join the session context before the extra round"
    );
}

/// 6. 会话落盘：run 后 session 文件可重开，消息完整（树链正确）。
#[test]
fn session_file_roundtrip_after_run() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let session = SessionManager::create(dir.path(), &sessions).unwrap();
    let file = session.path().to_path_buf();
    let provider = Arc::new(FakeProvider::new(
        fake_contract(),
        vec![
            FakeStep {
                text: String::new(),
                tool_calls: vec![tool_call(
                    "call_1",
                    "write",
                    json!({ "path": "out.txt", "content": "x" }),
                )],
                usage: usage(10, 5),
            },
            FakeStep {
                text: "finished".to_string(),
                tool_calls: Vec::new(),
                usage: usage(20, 5),
            },
        ],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig::default(),
        session,
    )
    .unwrap();
    agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .unwrap();
    drop(agent);

    let reopened = SessionManager::open_existing(&file).unwrap();
    let entries = reopened.build_context_entries().unwrap();
    let messages: Vec<&AgentMessage> = entries
        .iter()
        .filter_map(|entry| match &entry {
            SessionEntry::Message { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].role(), AgentMessageRole::User);
    assert_eq!(messages[0].content_text(), "task");
    assert_eq!(messages[1].role(), AgentMessageRole::Assistant);
    let ContentBlock::ToolCall { id, name, args } =
        messages[1].tool_calls().next().expect("tool call block")
    else {
        panic!("expected tool call block");
    };
    assert_eq!(id, "call_1");
    assert_eq!(name, "write");
    assert_eq!(*args, json!({ "path": "out.txt", "content": "x" }));
    assert_eq!(messages[2].role(), AgentMessageRole::ToolResult);
    assert_eq!(messages[2].tool_call_id(), Some(&"call_1".to_string()));
    assert!(messages[2].content_text().contains("Successfully wrote"));
    assert_eq!(messages[3].role(), AgentMessageRole::Assistant);
    assert_eq!(messages[3].content_text(), "finished");
    // 线性序列：会话事实源按落盘次序推进，条目 id 必须唯一且顺序与上下文一致。
    let ids: Vec<&str> = entries.iter().map(|entry| entry.id()).collect();
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "session entry ids must be unique");
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.id())
            .collect::<Vec<_>>(),
        reopened
            .entries()
            .iter()
            .map(|entry| entry.id())
            .collect::<Vec<_>>(),
        "context entries preserve linear file order"
    );
}
struct OverflowProvider {
    stream_calls: std::sync::atomic::AtomicUsize,
    complete_calls: std::sync::atomic::AtomicUsize,
    overflow_times: usize,
    /// true 时摘要生成（`complete`）直接失败，用于验证强制压缩失败的降级路径。
    fail_summary: bool,
    /// 每次流式请求的消息文本（按调用顺序），用于断言重试携带压缩后上下文。
    request_texts: std::sync::Mutex<Vec<String>>,
    contract: ProviderProtocolContract,
}

/// 交错提供者:第一次流式调用返回可重试瞬时错误,第二次返回 ContextOverflow,
/// 其后成功。用于验证「瞬态重试(重发同一请求)→ 溢出强制压缩 → 重建重发」的
/// 交错恢复路径。摘要生成(`complete`)恒成功并返回压缩摘要。
struct InterleaveProvider {
    stream_calls: std::sync::atomic::AtomicUsize,
    complete_calls: std::sync::atomic::AtomicUsize,
    request_texts: std::sync::Mutex<Vec<String>>,
    contract: ProviderProtocolContract,
}

#[test]
fn previous_provider_usage_triggers_compaction_before_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    session
        .append_message(AgentMessage::text(AgentMessageRole::User, "u".repeat(600)))
        .unwrap();
    session
        .append_message(AgentMessage::text(
            AgentMessageRole::Assistant,
            "a".repeat(600),
        ))
        .unwrap();
    let provider = Arc::new(FakeProvider::new(
        fake_contract(),
        vec![
            FakeStep {
                text: String::new(),
                tool_calls: vec![tool_call("call-1", "missing", json!({}))],
                usage: usage(5_500, 500),
            },
            FakeStep {
                text: "summary".to_string(),
                tool_calls: Vec::new(),
                usage: usage(20, 5),
            },
            FakeStep {
                text: "done".to_string(),
                tool_calls: Vec::new(),
                usage: usage(30, 5),
            },
        ],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        compaction_test_config(),
        session,
    )
    .unwrap();

    let outcome = agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .unwrap();

    assert_eq!(outcome.final_text, "done");
    assert!(outcome.compacted);
    assert!(
        agent
            .session
            .entries()
            .iter()
            .any(|entry| matches!(entry, SessionEntry::Compaction { compaction: _, .. }))
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 3);
}

#[test]
fn first_request_without_previous_usage_falls_back_to_the_assembled_estimate() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    session
        .append_message(AgentMessage::text(
            AgentMessageRole::User,
            "old user context ".repeat(1_000),
        ))
        .unwrap();
    session
        .append_message(AgentMessage::text(
            AgentMessageRole::Assistant,
            "old assistant context ".repeat(1_000),
        ))
        .unwrap();
    let provider = Arc::new(FakeProvider::new(
        fake_contract(),
        vec![
            FakeStep {
                text: "summary".to_string(),
                tool_calls: Vec::new(),
                usage: usage(20, 5),
            },
            FakeStep {
                text: "done".to_string(),
                tool_calls: Vec::new(),
                usage: usage(30, 5),
            },
        ],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        compaction_test_config(),
        session,
    )
    .unwrap();

    let outcome = agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .unwrap();

    assert_eq!(outcome.final_text, "done");
    assert!(outcome.compacted);
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
}

#[test]
fn missing_provider_usage_falls_back_to_the_next_assembled_estimate() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    session
        .append_message(AgentMessage::text(
            AgentMessageRole::User,
            "old user ".repeat(100),
        ))
        .unwrap();
    session
        .append_message(AgentMessage::text(
            AgentMessageRole::Assistant,
            "old assistant ".repeat(100),
        ))
        .unwrap();
    let provider = Arc::new(FakeProvider::new(
        fake_contract(),
        vec![
            FakeStep {
                text: String::new(),
                tool_calls: vec![tool_call(
                    "call-1",
                    "missing",
                    json!({ "payload": "x".repeat(24_000) }),
                )],
                usage: ModelUsage::default(),
            },
            FakeStep {
                text: "summary".to_string(),
                tool_calls: Vec::new(),
                usage: usage(20, 5),
            },
            FakeStep {
                text: "done".to_string(),
                tool_calls: Vec::new(),
                usage: usage(30, 5),
            },
        ],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        compaction_test_config(),
        session,
    )
    .unwrap();

    let outcome = agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .unwrap();

    assert_eq!(outcome.final_text, "done");
    assert!(outcome.compacted);
    assert_eq!(provider.requests.lock().unwrap().len(), 3);
}

#[test]
fn assembled_fallback_estimate_includes_provider_reasoning_replay() {
    let (agent, _dir, _provider) = setup(Vec::new());
    let replay = ProviderReasoningReplay::Chat {
        provider_name: "fake".to_string(),
        model_name: "fake-model".to_string(),
        reasoning_effort: None,
        tool_call_ids: vec!["call-1".to_string()],
        reasoning_content: "reasoning ".repeat(300),
    };

    let without_replay = agent.estimate_assembled(&[], &[], &[], 0);
    let with_replay = agent.estimate_assembled(&[], &[], &[replay], 0);

    assert!(with_replay > without_replay + 500);
}

#[test]
fn manual_compaction_keeps_the_configured_recent_twenty_percent() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    session
        .append_message(AgentMessage::text(
            AgentMessageRole::User,
            "first user ".repeat(40),
        ))
        .unwrap();
    session
        .append_message(AgentMessage::text(
            AgentMessageRole::Assistant,
            "first assistant ".repeat(40),
        ))
        .unwrap();
    let expected_first_kept = session
        .append_message(AgentMessage::text(
            AgentMessageRole::User,
            "recent user ".repeat(40),
        ))
        .unwrap();
    session
        .append_message(AgentMessage::text(
            AgentMessageRole::Assistant,
            "recent assistant ".repeat(40),
        ))
        .unwrap();
    let provider = Arc::new(FakeProvider::new(
        fake_contract(),
        vec![
            FakeStep {
                text: "summary".to_string(),
                tool_calls: Vec::new(),
                usage: usage(20, 5),
            },
            FakeStep {
                text: "turn prefix".to_string(),
                tool_calls: Vec::new(),
                usage: usage(20, 5),
            },
        ],
    ));
    let config = AgentConfig {
        system_prompt: String::new(),
        context_window: 1_000,
        max_output_tokens: 10,
        compaction: CompactionConfig {
            reserve_tokens: 100,
            retain_ratio: 0.20,
            summary_max_tokens: 10,
        },
        ..AgentConfig::default()
    };
    let mut agent = Agent::new(provider, ToolRegistry::new(), config, session).unwrap();

    let outcome = agent.compact_now(&CancellationToken::new()).unwrap();

    assert!(matches!(
        outcome,
        CompactionOutcome::Compacted {
            first_kept_entry_id,
            ..
        } if first_kept_entry_id == expected_first_kept
    ));
}

impl Provider for OverflowProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        self.contract.clone()
    }

    fn streaming_capability(
        &self,
        _selected_protocol: singularity_model::ProviderApiProtocol,
    ) -> ProviderStreamingCapability {
        ProviderStreamingCapability::OutputTextDelta
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
        _on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> std::result::Result<ModelTurnResponse, ProviderError> {
        let call = self
            .stream_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.request_texts.lock().unwrap().push(
            request
                .messages
                .iter()
                .map(|message| message.content.clone())
                .collect::<Vec<_>>()
                .join(" | "),
        );
        if call < self.overflow_times {
            return Err(ProviderError::from_model_error(ModelError::new(
                ModelErrorKind::ContextLengthExceeded,
                "context window overflow",
            )));
        }
        let mut assistant = ModelMessage::assistant_tool_calls(Vec::new());
        assistant.content = "done after compact".to_string();
        Ok(ModelTurnResponse {
            request_id: request.request_id.clone(),
            response_id: "overflow-ok".to_string(),
            status: ModelTurnStatus::Success,
            assistant_message: Some(assistant),
            usage: ModelUsage::default(),
            finish_reason: Some("stop".to_string()),
            validation: None,
            error: None,
            provider_name: Some("overflow".to_string()),
            model_name: Some("overflow-model".to_string()),
            provider_reasoning_history: Vec::new(),
        })
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurnResponse, ProviderError> {
        self.complete_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_summary {
            return Err(ProviderError::from_model_error(ModelError::new(
                ModelErrorKind::UnknownProviderError,
                "summary generation failed",
            )));
        }
        let mut assistant = ModelMessage::assistant_tool_calls(Vec::new());
        assistant.content = "## Goal\ncompacted".to_string();
        Ok(ModelTurnResponse {
            request_id: request.request_id.clone(),
            response_id: "compaction-ok".to_string(),
            status: ModelTurnStatus::Success,
            assistant_message: Some(assistant),
            usage: ModelUsage::default(),
            finish_reason: Some("stop".to_string()),
            validation: None,
            error: None,
            provider_name: Some("overflow".to_string()),
            model_name: Some("overflow-model".to_string()),
            provider_reasoning_history: Vec::new(),
        })
    }
}

impl Provider for InterleaveProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        self.contract.clone()
    }

    fn streaming_capability(
        &self,
        _selected_protocol: singularity_model::ProviderApiProtocol,
    ) -> ProviderStreamingCapability {
        ProviderStreamingCapability::OutputTextDelta
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
        _on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> std::result::Result<ModelTurnResponse, ProviderError> {
        let call = self
            .stream_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.request_texts.lock().unwrap().push(
            request
                .messages
                .iter()
                .map(|message| message.content.clone())
                .collect::<Vec<_>>()
                .join(" | "),
        );
        match call {
            // 第一次:可重试瞬时错误,触发重试包装重发同一请求。
            0 => Err(ProviderError::from_model_error(ModelError::new(
                ModelErrorKind::NetworkError,
                "transient network failure",
            ))),
            // 第二次:上下文溢出,触发强制压缩与重建重发。
            1 => Err(ProviderError::from_model_error(ModelError::new(
                ModelErrorKind::ContextLengthExceeded,
                "context window overflow",
            ))),
            _ => {
                let mut assistant = ModelMessage::assistant_tool_calls(Vec::new());
                assistant.content = "done after recovery".to_string();
                Ok(ModelTurnResponse {
                    request_id: request.request_id.clone(),
                    response_id: "interleave-ok".to_string(),
                    status: ModelTurnStatus::Success,
                    assistant_message: Some(assistant),
                    usage: ModelUsage::default(),
                    finish_reason: Some("stop".to_string()),
                    validation: None,
                    error: None,
                    provider_name: Some("interleave".to_string()),
                    model_name: Some("interleave-model".to_string()),
                    provider_reasoning_history: Vec::new(),
                })
            }
        }
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurnResponse, ProviderError> {
        self.complete_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut assistant = ModelMessage::assistant_tool_calls(Vec::new());
        assistant.content = "## Goal\ncompacted".to_string();
        Ok(ModelTurnResponse {
            request_id: request.request_id.clone(),
            response_id: "compaction-ok".to_string(),
            status: ModelTurnStatus::Success,
            assistant_message: Some(assistant),
            usage: ModelUsage::default(),
            finish_reason: Some("stop".to_string()),
            validation: None,
            error: None,
            provider_name: Some("interleave".to_string()),
            model_name: Some("interleave-model".to_string()),
            provider_reasoning_history: Vec::new(),
        })
    }
}

#[test]
fn context_overflow_forces_one_compaction_retry_then_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    session
        .append_message(AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: "old user".to_string(),
            }],
        })
        .unwrap();
    session
        .append_message(AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "old assistant".to_string(),
            }],
            stop_reason: None,
            provider_reasoning_replay: None,
        })
        .unwrap();
    let provider = Arc::new(OverflowProvider {
        stream_calls: std::sync::atomic::AtomicUsize::new(0),
        complete_calls: std::sync::atomic::AtomicUsize::new(0),
        overflow_times: 1,
        fail_summary: false,
        request_texts: std::sync::Mutex::new(Vec::new()),
        contract: fake_contract(),
    });
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig::default(),
        session,
    )
    .unwrap();
    let outcome = agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .unwrap();
    assert_eq!(outcome.turns, 1);
    assert_eq!(outcome.final_text, "done after compact");
    assert_eq!(
        provider
            .stream_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    // 强制压缩后的重试必须携带压缩后的上下文：第二次请求包含摘要、
    // 不再包含被压缩掉的原始会话内容。
    let request_texts = provider.request_texts.lock().unwrap();
    assert_eq!(request_texts.len(), 2, "first overflow + one retry");
    assert!(
        request_texts[0].contains("old user"),
        "first request carries the pre-compaction context"
    );
    assert!(
        request_texts[1].contains("## Goal"),
        "retry carries the compaction summary: {}",
        request_texts[1]
    );
    assert!(
        !request_texts[1].contains("old user"),
        "retry must not carry the overflowed context: {}",
        request_texts[1]
    );
    assert_eq!(
        provider
            .complete_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert!(
        agent
            .session
            .build_context_entries()
            .unwrap()
            .iter()
            .any(|entry| matches!(entry, SessionEntry::Compaction { compaction: _, .. }))
    );
    let compaction = agent
        .session
        .entries()
        .iter()
        .find_map(|entry| match &entry {
            SessionEntry::Compaction { compaction, .. } => Some(compaction),
            _ => None,
        })
        .unwrap();
    let first_kept = compaction.first_kept_entry_id.as_deref().unwrap();
    assert!(agent.session.entries().iter().any(|entry| {
        entry.id() == first_kept
            && matches!(
                &entry,
                SessionEntry::Message { message, .. }
                    if message.role() == AgentMessageRole::User
                        && message.content_text() == "task"
            )
    }));
}

/// 交错回归:瞬态可重试错误触发重试(重发同一请求)后,下一发送遇到
/// ContextOverflow,轮步层强制压缩、重建请求并恰好一次重发成功。
#[test]
fn transient_retry_then_overflow_recovers_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    session
        .append_message(AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: "old user".to_string(),
            }],
        })
        .unwrap();
    session
        .append_message(AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: "old assistant".to_string(),
            }],
            stop_reason: None,
            provider_reasoning_replay: None,
        })
        .unwrap();
    let provider = Arc::new(InterleaveProvider {
        stream_calls: std::sync::atomic::AtomicUsize::new(0),
        complete_calls: std::sync::atomic::AtomicUsize::new(0),
        request_texts: std::sync::Mutex::new(Vec::new()),
        contract: fake_contract(),
    });
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig {
            // 瞬时错误重试一次(≤MAX_TURN_RETRIES 预算的轻量验证)。
            retry: TurnRetryConfig {
                max_retries: 1,
                base_delay_ms: 1,
            },
            ..AgentConfig::default()
        },
        session,
    )
    .unwrap();
    let outcome = agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .unwrap();
    assert_eq!(outcome.turns, 1);
    assert_eq!(outcome.final_text, "done after recovery");
    assert!(outcome.compacted, "overflow must force a compaction");
    assert_eq!(
        provider
            .stream_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        3,
        "initial + transient retry + overflow resend"
    );
    // 瞬态重试必须重发同一请求;溢出恢复后的重发必须携带压缩摘要。
    let request_texts = provider.request_texts.lock().unwrap();
    assert_eq!(request_texts.len(), 3, "one per stream call");
    assert_eq!(
        request_texts[0], request_texts[1],
        "transient retry must resend the identical request"
    );
    assert!(
        request_texts[2].contains("## Goal"),
        "overflow recovery must carry the compaction summary: {}",
        request_texts[2]
    );
    assert!(
        !request_texts[2].contains("old user"),
        "overflow recovery must drop the overflowed context: {}",
        request_texts[2]
    );
    assert_eq!(
        provider
            .complete_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one forced compaction summary"
    );
}

#[test]
fn orphaned_tool_call_reopens_without_executing_tool_again() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let target = workspace.join("should-not-exist.txt");
    let mut session = SessionManager::create(&workspace, &dir.path().join("sessions")).unwrap();
    session
        .append_message(AgentMessage::Assistant {
            content: vec![
                ContentBlock::Text {
                    text: "calling write".to_string(),
                },
                ContentBlock::ToolCall {
                    id: "orphan_write_1".to_string(),
                    name: "write".to_string(),
                    args: json!({"path": target, "content": "must not be written"}),
                },
            ],
            stop_reason: None,
            provider_reasoning_replay: None,
        })
        .unwrap();
    drop(session);
    let file = dir.path().join("sessions").join(
        std::fs::read_dir(dir.path().join("sessions"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name(),
    );
    let mut session = SessionManager::open_existing(&file).unwrap();
    assert_eq!(session.repair_orphaned_tool_calls().unwrap(), 1);
    let provider = Arc::new(FakeProvider::new(
        fake_contract(),
        vec![FakeStep {
            text: "final".to_string(),
            tool_calls: Vec::new(),
            usage: usage(0, 0),
        }],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig::default(),
        session,
    )
    .unwrap();
    let outcome = agent
        .run("resume", &mut AgentEvents::new(), &CancellationToken::new())
        .unwrap();
    assert_eq!(outcome.final_text, "final");
    assert!(
        !target.exists(),
        "reopen repair must not execute the orphaned tool"
    );
    let entries = agent.session.build_context_entries().unwrap();
    assert!(entries.iter().any(|entry| {
        matches!(
            &entry,
            SessionEntry::Message { message, .. }
                if message.role() == AgentMessageRole::ToolResult
                    && message.tool_call_id().is_some_and(|id| id == "orphan_write_1")
                    && message.content_text().contains("do not retry")
        )
    }));
}

/// 8. 中断：取消令牌 → 终止并返回已完成的文本（aborted 语义，不报错）。
#[test]
fn cancelled_run_returns_aborted_outcome() {
    let (mut agent, _dir, provider) = setup(vec![FakeStep {
        text: "never used".to_string(),
        tool_calls: Vec::new(),
        usage: usage(10, 5),
    }]);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let outcome = agent
        .run("task", &mut AgentEvents::new(), &cancellation)
        .unwrap();
    assert_eq!(outcome.terminal_reason, AgentTerminalReason::Aborted);
    assert_eq!(outcome.turns, 0);
    // 已取消时不发起任何 provider 调用。
    assert!(provider.requests.lock().unwrap().is_empty());
}
