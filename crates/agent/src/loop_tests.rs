use super::*;
use crate::message::AgentMessage;
use crate::session::{CompactionEntry, SessionEntryType};
use crate::tools::ToolSpec;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use singularity_model::{ModelToolCall, ModelToolParseStatus, ProviderStreamingCapability};

static PARALLEL_ACTIVE: AtomicUsize = AtomicUsize::new(0);
static PARALLEL_MAX_ACTIVE: AtomicUsize = AtomicUsize::new(0);
static PARALLEL_TEST_LOCK: Mutex<()> = Mutex::new(());

fn record_max(maximum: &AtomicUsize, value: usize) {
    let mut current = maximum.load(Ordering::SeqCst);
    while value > current {
        match maximum.compare_exchange(current, value, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

fn delay_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "delay_ms": { "type": "integer" }
        },
        "required": ["id"],
        "additionalProperties": false
    })
}

fn delay_execute(mut ctx: ExecuteContext<'_>) -> std::result::Result<ToolExecution, ToolError> {
    let id = ctx
        .args
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if let Some(update) = ctx.on_update.as_deref_mut() {
        update(&format!("partial:{id}"));
    }
    let delay = ctx
        .args
        .get("delay_ms")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    std::thread::sleep(Duration::from_millis(delay));
    Ok(ToolExecution {
        content: id.to_string(),
        is_error: false,
    })
}

fn counted_delay_execute(
    mut ctx: ExecuteContext<'_>,
) -> std::result::Result<ToolExecution, ToolError> {
    let id = ctx
        .args
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let active = PARALLEL_ACTIVE.fetch_add(1, Ordering::SeqCst) + 1;
    record_max(&PARALLEL_MAX_ACTIVE, active);
    if let Some(update) = ctx.on_update.as_deref_mut() {
        update(&format!("partial:{id}"));
    }
    std::thread::sleep(Duration::from_millis(40));
    PARALLEL_ACTIVE.fetch_sub(1, Ordering::SeqCst);
    Ok(ToolExecution {
        content: id.to_string(),
        is_error: false,
    })
}

fn failure_execute(_ctx: ExecuteContext<'_>) -> std::result::Result<ToolExecution, ToolError> {
    Ok(ToolExecution {
        content: "intentional tool failure".to_string(),
        is_error: true,
    })
}

fn cancellation_execute(ctx: ExecuteContext<'_>) -> std::result::Result<ToolExecution, ToolError> {
    for _ in 0..200 {
        if ctx.signal.is_some_and(|signal| signal.is_cancelled()) {
            return Ok(ToolExecution {
                content: "Operation aborted".to_string(),
                is_error: true,
            });
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(ToolExecution {
        content: "cancellation was not observed".to_string(),
        is_error: true,
    })
}

fn custom_spec(
    name: &'static str,
    execute: for<'a> fn(ExecuteContext<'a>) -> std::result::Result<ToolExecution, ToolError>,
    parameters: Value,
) -> ToolSpec {
    ToolSpec {
        name,
        description: "parallelism test tool",
        parameters,
        execute,
    }
}

/// 脚本化 FakeProvider：按脚本顺序弹出响应；`complete_stream` 以单次文本增量
/// 投递 assistant 文本（覆盖流式路径），`complete` 无增量（覆盖回退/compaction 路径）。
struct FakeProvider {
    steps: Mutex<VecDeque<FakeStep>>,
    requests: Mutex<Vec<ModelTurnRequest>>,
    contract: ProviderProtocolContract,
}

#[derive(Clone)]
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
            tool_calls: step.tool_calls.clone(),
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
            provider_attempt_metadata: None,
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
        supports_parallel_tool_calls: true,
        supports_required_tool_choice: false,
        supports_strict_tool_schema: false,
        tool_reasoning_mode: singularity_model::ProviderToolReasoningMode::Unspecified,
        max_tools_per_request: 8,
        supports_json_mode: false,
        supports_system_message: false,
        supports_developer_message: true,
        max_parallel_tool_calls: 1,
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

/// 失败脚本假 provider：按脚本顺序返回失败 turn（status=Failed）；`calls`
/// 统计模型调用总次数，用于验证失败路径不重试。
struct FailingProvider {
    steps: Mutex<VecDeque<FailStep>>,
    calls: std::sync::atomic::AtomicUsize,
    contract: ProviderProtocolContract,
}

#[derive(Clone)]
enum FailStep {
    /// 返回 status=Failed + 给定 error 的 turn。
    Fail(ModelError),
}

impl FailingProvider {
    fn new(contract: ProviderProtocolContract, steps: Vec<FailStep>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            contract,
        }
    }

    fn try_respond(&self, request: &ModelTurnRequest) -> ModelTurnResponse {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let step = self.steps.lock().unwrap().pop_front();
        match step {
            Some(FailStep::Fail(error)) => ModelTurnResponse {
                request_id: request.request_id.clone(),
                response_id: format!("fail-{}", Uuid::new_v4().simple()),
                status: ModelTurnStatus::Failed,
                assistant_message: None,
                tool_calls: Vec::new(),
                usage: ModelUsage::default(),
                finish_reason: None,
                validation: None,
                error: Some(error),
                provider_name: Some("failing".to_string()),
                model_name: Some("fake-model".to_string()),
                provider_attempt_metadata: None,
                provider_reasoning_history: Vec::new(),
            },
            // 脚本耗尽：视作未知错误（非瞬时类文本不触发任何重试语义）。
            None => ModelTurnResponse {
                request_id: request.request_id.clone(),
                response_id: format!("empty-{}", Uuid::new_v4().simple()),
                status: ModelTurnStatus::Failed,
                assistant_message: None,
                tool_calls: Vec::new(),
                usage: ModelUsage::default(),
                finish_reason: None,
                validation: None,
                error: Some(ModelError::new(
                    ModelErrorKind::UnknownProviderError,
                    "no scripted steps remaining",
                )),
                provider_name: Some("failing".to_string()),
                model_name: Some("fake-model".to_string()),
                provider_attempt_metadata: None,
                provider_reasoning_history: Vec::new(),
            },
        }
    }
}

impl Provider for FailingProvider {
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
        Ok(self.try_respond(request))
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurnResponse, ProviderError> {
        Ok(self.try_respond(request))
    }
}

/// 从 `complete_stream` 直接返回 `Err(ProviderError)` 的假 provider：
/// 模拟传输层重试（`MAX_PROVIDER_ATTEMPTS`）耗尽后仍失败的路径——在修复前这部分
/// 是死代码，`stream_completion` 直接以 `Err(AgentError::Provider)` 向外传播，
/// AgentLoop 不做整轮重试；脚本按序在若干次失败后返回一次成功，用于覆盖
/// provider 传输层耗尽后仍失败的 typed 传播路径。
struct ErrReturningProvider {
    /// 每次 `complete_stream` 弹出的结果：`Err(model_error)` 或 `Ok(text)`。
    steps: Mutex<VecDeque<std::result::Result<String, ModelError>>>,
    calls: std::sync::atomic::AtomicUsize,
    contract: ProviderProtocolContract,
}

impl ErrReturningProvider {
    fn new(
        contract: ProviderProtocolContract,
        steps: Vec<std::result::Result<String, ModelError>>,
    ) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            contract,
        }
    }

    fn try_respond(
        &self,
        request: &ModelTurnRequest,
    ) -> std::result::Result<ModelTurnResponse, ProviderError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match self.steps.lock().unwrap().pop_front() {
            Some(Err(error)) => Err(ProviderError::from_model_error(error)),
            Some(Ok(text)) => Ok(ModelTurnResponse::completed(
                request.request_id.clone(),
                format!("ok-{}", Uuid::new_v4().simple()),
                text,
            )),
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

/// 恒返回 `ModelTurnStatus::Invalid` 的假 provider：校验失败直接 typed 传播。
struct InvalidStatusProvider {
    contract: ProviderProtocolContract,
    calls: std::sync::atomic::AtomicUsize,
}

impl InvalidStatusProvider {
    fn new(contract: ProviderProtocolContract) -> Self {
        Self {
            contract,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl Provider for InvalidStatusProvider {
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
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ModelTurnResponse {
            request_id: request.request_id.clone(),
            response_id: format!("invalid-{}", Uuid::new_v4().simple()),
            status: ModelTurnStatus::Invalid,
            assistant_message: None,
            tool_calls: Vec::new(),
            usage: ModelUsage::default(),
            finish_reason: None,
            validation: Some(singularity_model::ModelValidationResult {
                valid: false,
                errors: vec!["dropped duplicate tool call".to_string()],
                warnings: Vec::new(),
            }),
            error: Some(ModelError::new(
                ModelErrorKind::JsonSchemaViolation,
                "response validation failed: dropped duplicate tool call",
            )),
            provider_name: Some("failing".to_string()),
            model_name: Some("fake-model".to_string()),
            provider_attempt_metadata: None,
            provider_reasoning_history: Vec::new(),
        })
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurnResponse, ProviderError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ModelTurnResponse {
            request_id: request.request_id.clone(),
            response_id: format!("invalid-{}", Uuid::new_v4().simple()),
            status: ModelTurnStatus::Invalid,
            assistant_message: None,
            tool_calls: Vec::new(),
            usage: ModelUsage::default(),
            finish_reason: None,
            validation: Some(singularity_model::ModelValidationResult {
                valid: false,
                errors: vec!["dropped duplicate tool call".to_string()],
                warnings: Vec::new(),
            }),
            error: Some(ModelError::new(
                ModelErrorKind::JsonSchemaViolation,
                "response validation failed",
            )),
            provider_name: Some("failing".to_string()),
            model_name: Some("fake-model".to_string()),
            provider_attempt_metadata: None,
            provider_reasoning_history: Vec::new(),
        })
    }
}

/// Provider 失败路径：`Err(ProviderError)` 中不可重试错误（挂起超时）不被转换为
/// `Ok(Failed)`，保持 `Err(AgentError::Provider)` 传播，agent 直接失败且一次尝试。
#[test]
fn non_retryable_err_provider_error_fails_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let provider = Arc::new(ErrReturningProvider::new(
        fake_contract(),
        vec![Err(ModelError::new(
            ModelErrorKind::Timeout,
            "request hung and timed out",
        ))],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig::default(),
        session,
    )
    .unwrap();
    let err = agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .unwrap_err();
    // 不可重试错误原样传播为 Provider 错误（含原 kind 消息）。
    assert!(
        err.to_string().contains("request hung and timed out"),
        "non-retryable Err must propagate as provider error, got: {}",
        err
    );
    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "Timeout Err(ProviderError) must not be retried"
    );
}

/// 非瞬时类（挂起超时、账户限额、校验失败）不重试，直接失败。
#[test]
fn non_retryable_errors_fail_immediately_without_retry() {
    // 挂起超时（120s fail-fast 决策）：不重试。
    let timeout_dir = tempfile::tempdir().unwrap();
    let timeout_session =
        SessionManager::create(timeout_dir.path(), &timeout_dir.path().join("sessions")).unwrap();
    let timeout_provider = Arc::new(FailingProvider::new(
        fake_contract(),
        vec![FailStep::Fail(ModelError::new(
            ModelErrorKind::Timeout,
            "request hung and timed out",
        ))],
    ));
    let mut timeout_agent = Agent::new(
        timeout_provider.clone(),
        ToolRegistry::new(),
        AgentConfig::default(),
        timeout_session,
    )
    .unwrap();
    let timeout_err = timeout_agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .unwrap_err();
    assert!(timeout_err.to_string().contains("request hung"));
    assert_eq!(
        timeout_provider
            .calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "timeout must not retry"
    );

    // 账户限额文本（quota）：不重试。
    let quota_dir = tempfile::tempdir().unwrap();
    let quota_session =
        SessionManager::create(quota_dir.path(), &quota_dir.path().join("sessions")).unwrap();
    let quota_provider = Arc::new(FailingProvider::new(
        fake_contract(),
        vec![FailStep::Fail(ModelError::new(
            ModelErrorKind::UnknownProviderError,
            "insufficient_quota: account balance exhausted",
        ))],
    ));
    let mut quota_agent = Agent::new(
        quota_provider.clone(),
        ToolRegistry::new(),
        AgentConfig::default(),
        quota_session,
    )
    .unwrap();
    let quota_err = quota_agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .unwrap_err();
    assert!(quota_err.to_string().contains("insufficient_quota"));
    assert_eq!(
        quota_provider
            .calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "quota must not retry"
    );

    // 瞬时 kind（RateLimited）携带 quota 文本：限额守卫优先，不重试。
    let mixed_dir = tempfile::tempdir().unwrap();
    let mixed_session =
        SessionManager::create(mixed_dir.path(), &mixed_dir.path().join("sessions")).unwrap();
    let mixed_provider = Arc::new(FailingProvider::new(
        fake_contract(),
        vec![FailStep::Fail(ModelError::new(
            ModelErrorKind::RateLimited,
            "429 insufficient_quota: monthly usage limit reached",
        ))],
    ));
    let mut mixed_agent = Agent::new(
        mixed_provider.clone(),
        ToolRegistry::new(),
        AgentConfig::default(),
        mixed_session,
    )
    .unwrap();
    let mixed_err = mixed_agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .unwrap_err();
    assert!(mixed_err.to_string().contains("insufficient_quota"));
    assert_eq!(
        mixed_provider
            .calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "RateLimited kind with quota text must not retry"
    );

    // 校验失败（Invalid 状态）：不重试。
    let invalid_dir = tempfile::tempdir().unwrap();
    let invalid_session =
        SessionManager::create(invalid_dir.path(), &invalid_dir.path().join("sessions")).unwrap();
    let invalid_provider = Arc::new(InvalidStatusProvider::new(fake_contract()));
    let mut invalid_agent = Agent::new(
        invalid_provider.clone(),
        ToolRegistry::new(),
        AgentConfig::default(),
        invalid_session,
    )
    .unwrap();
    let invalid_err = invalid_agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .unwrap_err();
    assert!(invalid_err.to_string().contains("response validation"));
    assert_eq!(
        invalid_provider
            .calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "invalid status must not retry"
    );
}

/// 瞬时类失败不在 AgentLoop 层整轮重试（N3 单层归属）：一次调用后 typed 传播原错误。
#[test]
fn transient_failure_propagates_typed_after_single_call() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let provider = Arc::new(FailingProvider::new(
        fake_contract(),
        vec![FailStep::Fail(ModelError::new(
            ModelErrorKind::ProviderOverloaded,
            "provider overloaded",
        ))],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig::default(),
        session,
    )
    .unwrap();
    let err = agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .unwrap_err();
    assert!(
        matches!(err, AgentError::Provider(_)),
        "transient provider failure must propagate typed, got: {err:?}"
    );
    assert!(err.to_string().contains("provider overloaded"));
    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "no run-level retry after N3"
    );
}

/// 1. 单轮文本响应 → 停止，usage 聚合正确。
#[test]
fn single_text_turn_stops_with_usage() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let provider = Arc::new(FakeProvider::new(
        fake_contract(),
        vec![FakeStep {
            text: "hello from model".to_string(),
            tool_calls: Vec::new(),
            usage: usage(100, 50),
        }],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig {
            system_prompt: "be helpful".to_string(),
            ..AgentConfig::default()
        },
        session,
    )
    .unwrap();
    let mut events = AgentEvents::new();
    let mut deltas = String::new();
    let mut on_message_update = |delta: &str| deltas.push_str(delta);
    events.on_message_update = Some(&mut on_message_update);
    let outcome = agent
        .run("hi", &mut events, &CancellationToken::new())
        .unwrap();
    assert_eq!(outcome.turns, 1);
    assert_eq!(outcome.final_text, "hello from model");
    assert_eq!(outcome.usage.input_tokens, 100);
    assert_eq!(outcome.usage.output_tokens, 50);
    assert_eq!(outcome.usage.total_tokens, 150);
    assert!(!outcome.compacted);
    assert!(!outcome.aborted);
    assert_eq!(deltas, "hello from model");
    // 请求包含 system prompt（developer 角色）+ user 输入。
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages[0].role, ModelRole::Developer);
    assert_eq!(requests[0].messages[0].content, "be helpful");
    assert_eq!(requests[0].messages[1].role, ModelRole::User);
    assert_eq!(requests[0].messages[1].content, "hi");
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
    let mut on_tool_execution_start = |name: &str, _call_id: &str, args: &Value| {
        started.push((name.to_string(), args.to_string()))
    };
    events.on_tool_execution_start = Some(&mut on_tool_execution_start);
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
fn tool_lifecycle_callbacks_carry_pi_fields() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let provider = Arc::new(FakeProvider::new(
        fake_contract(),
        vec![
            FakeStep {
                text: String::new(),
                tool_calls: vec![tool_call(
                    "call_bash",
                    "bash",
                    json!({ "command": "printf hi" }),
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
        provider,
        ToolRegistry::new(),
        AgentConfig::default(),
        session,
    )
    .unwrap();
    let mut events = AgentEvents::new();
    let mut starts = Vec::new();
    let mut updates = Vec::new();
    let mut ends = Vec::new();
    let mut on_start = |name: &str, id: &str, args: &Value| {
        starts.push((name.to_string(), id.to_string(), args.clone()));
    };
    let mut on_update = |name: &str, id: &str, args: &Value, partial: &str| {
        updates.push((
            name.to_string(),
            id.to_string(),
            args.clone(),
            partial.to_string(),
        ));
    };
    let mut on_end = |name: &str, id: &str, result: &ToolExecution| {
        ends.push((
            name.to_string(),
            id.to_string(),
            result.content.clone(),
            result.is_error,
        ));
    };
    events.on_tool_execution_start = Some(&mut on_start);
    events.on_tool_execution_update = Some(&mut on_update);
    events.on_tool_execution_end = Some(&mut on_end);

    agent
        .run("run bash", &mut events, &CancellationToken::new())
        .unwrap();

    assert_eq!(
        starts,
        vec![(
            "bash".to_string(),
            "call_bash".to_string(),
            json!({ "command": "printf hi" }),
        )]
    );
    assert!(updates.iter().any(|(name, id, args, partial)| {
        name == "bash"
            && id == "call_bash"
            && args == &json!({ "command": "printf hi" })
            && partial.contains("hi")
    }));
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].0, "bash");
    assert_eq!(ends[0].1, "call_bash");
    assert!(!ends[0].3);
    assert!(ends[0].2.contains("hi"));
}

#[test]
fn parallel_tool_batch_overlaps_and_preserves_source_order() {
    let _guard = PARALLEL_TEST_LOCK.lock().unwrap();
    PARALLEL_ACTIVE.store(0, Ordering::SeqCst);
    PARALLEL_MAX_ACTIVE.store(0, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(custom_spec(
        "overlap",
        counted_delay_execute,
        delay_parameters(),
    ));
    let mut contract = fake_contract();
    contract.max_parallel_tool_calls = 8;
    let provider = Arc::new(FakeProvider::new(
        contract,
        vec![
            FakeStep {
                text: String::new(),
                tool_calls: vec![
                    tool_call("call_a", "overlap", json!({ "id": "a" })),
                    tool_call("call_b", "overlap", json!({ "id": "b" })),
                ],
                usage: usage(50, 10),
            },
            FakeStep {
                text: "done".to_string(),
                tool_calls: Vec::new(),
                usage: usage(100, 20),
            },
        ],
    ));
    let mut agent =
        Agent::new(provider.clone(), registry, AgentConfig::default(), session).unwrap();
    let mut events = AgentEvents::new();
    let mut starts = Vec::new();
    let mut updates = Vec::new();
    let mut ends = Vec::new();
    let mut on_start = |name: &str, id: &str, args: &Value| {
        starts.push((name.to_string(), id.to_string(), args.clone()));
    };
    let mut on_update = |name: &str, id: &str, args: &Value, partial: &str| {
        updates.push((
            name.to_string(),
            id.to_string(),
            args.clone(),
            partial.to_string(),
        ));
    };
    let mut on_end = |name: &str, id: &str, result: &ToolExecution| {
        ends.push((
            name.to_string(),
            id.to_string(),
            result.content.clone(),
            result.is_error,
        ));
    };
    events.on_tool_execution_start = Some(&mut on_start);
    events.on_tool_execution_update = Some(&mut on_update);
    events.on_tool_execution_end = Some(&mut on_end);
    let outcome = agent
        .run("parallel", &mut events, &CancellationToken::new())
        .unwrap();
    assert_eq!(outcome.final_text, "done");
    assert!(
        PARALLEL_MAX_ACTIVE.load(Ordering::SeqCst) >= 2,
        "parallel tools did not overlap"
    );
    assert_eq!(
        starts
            .iter()
            .map(|(_, id, _)| id.as_str())
            .collect::<Vec<_>>(),
        vec!["call_a", "call_b"]
    );
    assert_eq!(updates.len(), 2);
    assert_eq!(ends.len(), 2);
    assert!(ends.iter().all(|(_, _, _, is_error)| !is_error));
    let requests = provider.requests.lock().unwrap();
    let tool_results = requests[1]
        .messages
        .iter()
        .filter(|message| message.role == ModelRole::Tool)
        .map(|message| {
            (
                message.tool_call_id.clone().unwrap(),
                message.content.clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        tool_results
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        vec!["call_a", "call_b"]
    );
    assert_eq!(tool_results[0].1, "a");
    assert_eq!(tool_results[1].1, "b");
}

#[test]
fn parallel_tool_batch_respects_provider_concurrency_limit() {
    let _guard = PARALLEL_TEST_LOCK.lock().unwrap();
    PARALLEL_ACTIVE.store(0, Ordering::SeqCst);
    PARALLEL_MAX_ACTIVE.store(0, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(custom_spec(
        "overlap",
        counted_delay_execute,
        delay_parameters(),
    ));
    let mut contract = fake_contract();
    contract.max_parallel_tool_calls = 2;
    let provider = Arc::new(FakeProvider::new(
        contract,
        vec![
            FakeStep {
                text: String::new(),
                tool_calls: (b'a'..=b'e')
                    .map(|suffix| {
                        let id = format!("call_{}", suffix as char);
                        tool_call(&id, "overlap", json!({ "id": id, "delay_ms": 40 }))
                    })
                    .collect(),
                usage: usage(50, 10),
            },
            FakeStep {
                text: "done".to_string(),
                tool_calls: Vec::new(),
                usage: usage(100, 20),
            },
        ],
    ));
    let mut agent = Agent::new(provider, registry, AgentConfig::default(), session).unwrap();
    let outcome = agent
        .run(
            "bounded parallel",
            &mut AgentEvents::new(),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(outcome.final_text, "done");
    assert_eq!(
        PARALLEL_MAX_ACTIVE.load(Ordering::SeqCst),
        2,
        "parallel worker count must honor provider max_parallel_tool_calls"
    );
}

#[test]
fn preflight_rejection_does_not_consume_parallel_worker_slot() {
    let _guard = PARALLEL_TEST_LOCK.lock().unwrap();
    PARALLEL_ACTIVE.store(0, Ordering::SeqCst);
    PARALLEL_MAX_ACTIVE.store(0, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(custom_spec(
        "overlap",
        counted_delay_execute,
        delay_parameters(),
    ));
    let mut contract = fake_contract();
    contract.max_parallel_tool_calls = 2;
    let provider = Arc::new(FakeProvider::new(
        contract,
        vec![
            FakeStep {
                text: String::new(),
                tool_calls: vec![
                    tool_call("rejected", "missing", json!({})),
                    tool_call("call_a", "overlap", json!({ "id": "a" })),
                    tool_call("call_b", "overlap", json!({ "id": "b" })),
                ],
                usage: usage(50, 10),
            },
            FakeStep {
                text: "done".to_string(),
                tool_calls: Vec::new(),
                usage: usage(100, 20),
            },
        ],
    ));
    let mut agent = Agent::new(provider, registry, AgentConfig::default(), session).unwrap();
    let outcome = agent
        .run(
            "preflight rejection",
            &mut AgentEvents::new(),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(outcome.final_text, "done");
    assert_eq!(
        PARALLEL_MAX_ACTIVE.load(Ordering::SeqCst),
        2,
        "preflight rejection must not reduce the executable worker window"
    );
}

#[test]
fn tool_failure_does_not_drop_other_parallel_results() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(custom_spec(
        "fail",
        failure_execute,
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    ));
    registry.register(custom_spec("delay", delay_execute, delay_parameters()));
    let mut contract = fake_contract();
    contract.max_parallel_tool_calls = 8;
    let provider = Arc::new(FakeProvider::new(
        contract,
        vec![
            FakeStep {
                text: String::new(),
                tool_calls: vec![
                    tool_call("fail_call", "fail", json!({})),
                    tool_call("ok_call", "delay", json!({ "id": "ok" })),
                ],
                usage: usage(50, 10),
            },
            FakeStep {
                text: "done".to_string(),
                tool_calls: Vec::new(),
                usage: usage(100, 20),
            },
        ],
    ));
    let mut agent =
        Agent::new(provider.clone(), registry, AgentConfig::default(), session).unwrap();
    let outcome = agent
        .run(
            "failure",
            &mut AgentEvents::new(),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(outcome.final_text, "done");
    let requests = provider.requests.lock().unwrap();
    let results = requests[1]
        .messages
        .iter()
        .filter(|message| message.role == ModelRole::Tool)
        .map(|message| {
            (
                message.tool_call_id.clone().unwrap(),
                message.content.clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(results[0].0, "fail_call");
    assert!(results[0].1.contains("intentional tool failure"));
    assert_eq!(results[1], ("ok_call".to_string(), "ok".to_string()));
}

#[test]
fn cancellation_waits_for_all_parallel_tools_and_persists_results() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(custom_spec(
        "cancel_wait",
        cancellation_execute,
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    ));
    let mut contract = fake_contract();
    contract.max_parallel_tool_calls = 8;
    let provider = Arc::new(FakeProvider::new(
        contract,
        vec![FakeStep {
            text: String::new(),
            tool_calls: vec![
                tool_call("cancel_a", "cancel_wait", json!({})),
                tool_call("cancel_b", "cancel_wait", json!({})),
            ],
            usage: usage(50, 10),
        }],
    ));
    let mut agent =
        Agent::new(provider.clone(), registry, AgentConfig::default(), session).unwrap();
    let cancellation = CancellationToken::new();
    let canceller = cancellation.clone();
    let cancellation_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        canceller.cancel();
    });
    let outcome = agent
        .run("cancel", &mut AgentEvents::new(), &cancellation)
        .unwrap();
    cancellation_thread.join().unwrap();
    assert!(outcome.aborted);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let entries = agent.session.build_context_entries().unwrap();
    let tool_results = entries
        .iter()
        .filter_map(|entry| match &entry.entry_type {
            SessionEntryType::Message(message) if message.role == AgentMessageRole::ToolResult => {
                Some(message.tool_call_id.clone().unwrap())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_results, vec!["cancel_a", "cancel_b"]);
}

#[test]
fn same_file_edits_are_serialized_and_preserve_both_changes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("shared.txt"), "a\nb").unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let mut contract = fake_contract();
    contract.max_parallel_tool_calls = 8;
    let provider = Arc::new(FakeProvider::new(
        contract,
        vec![
            FakeStep {
                text: String::new(),
                tool_calls: vec![
                    tool_call(
                        "edit_a",
                        "edit",
                        json!({
                            "path": "shared.txt",
                            "oldString": "a",
                            "newString": "A"
                        }),
                    ),
                    tool_call(
                        "edit_b",
                        "edit",
                        json!({
                            "path": "shared.txt",
                            "oldString": "b",
                            "newString": "B"
                        }),
                    ),
                ],
                usage: usage(50, 10),
            },
            FakeStep {
                text: "done".to_string(),
                tool_calls: Vec::new(),
                usage: usage(100, 20),
            },
        ],
    ));
    let mut agent = Agent::new(
        provider,
        ToolRegistry::new(),
        AgentConfig::default(),
        session,
    )
    .unwrap();
    agent
        .run(
            "edit both lines",
            &mut AgentEvents::new(),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("shared.txt")).unwrap(),
        "A\nB"
    );
}

/// 3. steer 注入：运行前队列注入 → 会话上下文持久化 → 后续轮次上下文中出现。
#[test]
fn steer_message_appears_in_following_turn_context() {
    let (mut agent, _dir, provider) = setup(vec![
        FakeStep {
            text: String::new(),
            tool_calls: vec![tool_call("call_1", "bash", json!({ "command": "echo hi" }))],
            usage: usage(50, 10),
        },
        FakeStep {
            text: "final".to_string(),
            tool_calls: Vec::new(),
            usage: usage(100, 10),
        },
    ]);
    agent.steer("please use a different approach");
    let outcome = agent
        .run(
            "do the task",
            &mut AgentEvents::new(),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(outcome.final_text, "final");
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    // 第一轮请求：user(input) 后紧跟 steer 消息。
    let texts: Vec<&str> = requests[0]
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect();
    assert_eq!(texts, &["do the task", "please use a different approach"]);
    // 第二轮请求（工具执行后）：上下文重放中仍包含 steer 消息。
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content == "please use a different approach")
    );
}

/// 4. follow_up：文本响应后 follow_up 队列非空 → 继续一轮再停止。
#[test]
fn follow_up_continues_one_more_turn() {
    let (mut agent, _dir, provider) = setup(vec![
        FakeStep {
            text: "first answer".to_string(),
            tool_calls: Vec::new(),
            usage: usage(10, 5),
        },
        FakeStep {
            text: "second answer".to_string(),
            tool_calls: Vec::new(),
            usage: usage(20, 5),
        },
    ]);
    agent.follow_up("please continue");
    let outcome = agent
        .run(
            "question",
            &mut AgentEvents::new(),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(outcome.turns, 2);
    assert_eq!(outcome.final_text, "second answer");
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content == "please continue")
    );
}

/// 4b. steer_handle：run 期间经共享队列注入 → 下一轮上下文出现。
#[test]
fn steer_handle_injects_during_run() {
    let (mut agent, _dir, provider) = setup(vec![
        FakeStep {
            text: String::new(),
            tool_calls: vec![tool_call("call_1", "bash", json!({ "command": "echo hi" }))],
            usage: usage(50, 10),
        },
        FakeStep {
            text: "final".to_string(),
            tool_calls: Vec::new(),
            usage: usage(100, 10),
        },
    ]);
    let handle = agent.steer_handle();
    let mut events = AgentEvents::new();
    // 工具执行开始时（run 期间）从外部句柄注入转向消息。
    let mut on_tool_execution_start = |_name: &str, _call_id: &str, _args: &Value| {
        handle
            .lock()
            .unwrap()
            .push_back("steer during run".to_string());
    };
    events.on_tool_execution_start = Some(&mut on_tool_execution_start);
    let outcome = agent
        .run("do the task", &mut events, &CancellationToken::new())
        .unwrap();
    assert_eq!(outcome.final_text, "final");
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    // 工具执行后的下一轮请求包含运行中注入的消息。
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.content == "steer during run")
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

    let reopened = SessionManager::open(&file).unwrap();
    let entries = reopened.build_context_entries().unwrap();
    let messages: Vec<&AgentMessage> = entries
        .iter()
        .filter_map(|entry| match &entry.entry_type {
            SessionEntryType::Message(message) => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].role, AgentMessageRole::User);
    assert_eq!(messages[0].content_text(), "task");
    assert_eq!(messages[1].role, AgentMessageRole::Assistant);
    let ContentBlock::ToolCall { id, name, args } =
        messages[1].tool_calls().first().expect("tool call block")
    else {
        panic!("expected tool call block");
    };
    assert_eq!(id, "call_1");
    assert_eq!(name, "write");
    assert_eq!(*args, json!({ "path": "out.txt", "content": "x" }));
    assert_eq!(messages[2].role, AgentMessageRole::ToolResult);
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
    assert!(messages[2].content_text().contains("Successfully wrote"));
    assert_eq!(messages[3].role, AgentMessageRole::Assistant);
    assert_eq!(messages[3].content_text(), "finished");
    // 树链：每条 parent = 前一条 id，首条为根。
    for (index, entry) in entries.iter().enumerate() {
        if index == 0 {
            assert_eq!(entry.parent_id, "");
        } else {
            assert_eq!(entry.parent_id, entries[index - 1].id);
        }
    }
}

/// 7. compaction 触发：极小 context_window + 超过 keep_recent 的上下文
///    → run 中出现 CompactionEntry。
#[test]
fn tiny_context_window_triggers_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    // 第一轮带工具调用且 assistant 文本超大（> keep_recent 20000 tokens ≈ 80000 字符），
    // 使切点落在第一条消息之后、存在可摘要内容。
    let big_text = "x".repeat(100_000);
    let provider = Arc::new(FakeProvider::new(
        fake_contract(),
        vec![
            // 第一轮：工具调用 + 大 usage → 触发 compaction → 消费一条摘要脚本。
            FakeStep {
                text: big_text.clone(),
                tool_calls: vec![tool_call(
                    "call_1",
                    "write",
                    json!({ "path": "out.txt", "content": "x" }),
                )],
                usage: ModelUsage {
                    input_tokens: 19_000,
                    output_tokens: 1_000,
                    total_tokens: 20_000,
                    cached_input_tokens: 0,
                    reasoning_tokens: 0,
                    usage_present: true,
                },
            },
            // compaction 摘要调用（CompactionEngine 走 complete）。
            FakeStep {
                text: "## Goal\ncompacted summary".to_string(),
                tool_calls: Vec::new(),
                usage: ModelUsage::default(),
            },
            // 第二轮：小 usage，compaction 后不再产生新摘要。
            FakeStep {
                text: "second".to_string(),
                tool_calls: Vec::new(),
                usage: usage(0, 0),
            },
        ],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig {
            context_window: 30_000,
            max_output_tokens: 1,
            ..AgentConfig::default()
        },
        session,
    )
    .unwrap();
    let outcome = agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .unwrap();
    assert!(outcome.compacted);
    assert_eq!(outcome.turns, 2);
    assert_eq!(outcome.final_text, "second");
    // 会话中出现 CompactionEntry，且上下文以摘要包裹的 user 消息开头。
    let entries = agent.session.build_context_entries().unwrap();
    let compaction_entries: Vec<&CompactionEntry> = entries
        .iter()
        .filter_map(|entry| match &entry.entry_type {
            SessionEntryType::Compaction(compaction) => Some(compaction),
            _ => None,
        })
        .collect();
    assert_eq!(compaction_entries.len(), 1);
    assert!(compaction_entries[0].summary.contains("compacted summary"));
    assert!(compaction_entries[0].first_kept_entry_id.is_some());
    let context = agent.session.build_session_context().unwrap();
    assert_eq!(context.messages[0].role, ModelRole::User);
    assert!(
        context.messages[0]
            .content
            .starts_with(crate::message::COMPACTION_SUMMARY_PREFIX)
    );
}

/// 7b. compaction 摘要请求必须继承有效输出上限：不绑定时会按 reserve
///     派生超过模型 max_output_tokens 的请求（真实链路被 Provider 400 拒绝）。
#[test]
fn compaction_summarization_respects_model_output_limit() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let big_text = "x".repeat(100_000);
    let provider = Arc::new(FakeProvider::new(
        fake_contract(),
        vec![
            FakeStep {
                text: big_text,
                tool_calls: vec![tool_call(
                    "call_1",
                    "write",
                    json!({ "path": "out.txt", "content": "x" }),
                )],
                usage: ModelUsage {
                    input_tokens: 19_000,
                    output_tokens: 1_000,
                    total_tokens: 20_000,
                    cached_input_tokens: 0,
                    reasoning_tokens: 0,
                    usage_present: true,
                },
            },
            FakeStep {
                text: "## Goal\nsummary".to_string(),
                tool_calls: Vec::new(),
                usage: ModelUsage::default(),
            },
            FakeStep {
                text: "done".to_string(),
                tool_calls: Vec::new(),
                usage: usage(0, 0),
            },
        ],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig {
            // 配置远大于 fake_contract 的 4096：有效上限必须取两者较小值。
            context_window: 30_000,
            max_output_tokens: 1_000_000,
            ..AgentConfig::default()
        },
        session,
    )
    .unwrap();
    let outcome = agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .unwrap();
    assert!(outcome.compacted);
    let requests = provider.requests.lock().unwrap();
    let summarization = requests
        .iter()
        .find(|request| request.request_id.starts_with("compaction-"))
        .expect("summarization request recorded");
    assert_eq!(
        summarization.model_preferences.max_output_tokens,
        Some(4_096),
        "summarization output limit must be capped by the model capability"
    );
}

#[test]
fn instruction_message_adapts_to_provider_roles() {
    let developer = ProviderProtocolContract {
        supports_developer_message: true,
        supports_system_message: true,
        ..ProviderProtocolContract::default()
    };
    let system = ProviderProtocolContract {
        supports_developer_message: false,
        supports_system_message: true,
        ..ProviderProtocolContract::default()
    };
    let neither = ProviderProtocolContract {
        supports_developer_message: false,
        supports_system_message: false,
        ..ProviderProtocolContract::default()
    };
    assert_eq!(
        instruction_message(&developer, "x").unwrap().role,
        ModelRole::Developer
    );
    assert_eq!(
        instruction_message(&system, "x").unwrap().role,
        ModelRole::System
    );
    assert_eq!(
        instruction_message(&neither, "x").unwrap().role,
        ModelRole::User
    );
    assert!(instruction_message(&developer, "").is_none());
}

struct OverflowProvider {
    stream_calls: std::sync::atomic::AtomicUsize,
    complete_calls: std::sync::atomic::AtomicUsize,
    overflow_times: usize,
    /// true 时摘要生成（`complete`）直接失败，用于验证强制压缩失败的降级路径。
    fail_summary: bool,
    contract: ProviderProtocolContract,
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
            tool_calls: Vec::new(),
            usage: ModelUsage::default(),
            finish_reason: Some("stop".to_string()),
            validation: None,
            error: None,
            provider_name: Some("overflow".to_string()),
            model_name: Some("overflow-model".to_string()),
            provider_attempt_metadata: None,
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
            tool_calls: Vec::new(),
            usage: ModelUsage::default(),
            finish_reason: Some("stop".to_string()),
            validation: None,
            error: None,
            provider_name: Some("overflow".to_string()),
            model_name: Some("overflow-model".to_string()),
            provider_attempt_metadata: None,
            provider_reasoning_history: Vec::new(),
        })
    }
}

#[test]
fn context_overflow_forces_one_compaction_retry_then_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    session
        .append_message(AgentMessage {
            role: AgentMessageRole::User,
            content: vec![ContentBlock::Text {
                text: "old user".to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        })
        .unwrap();
    session
        .append_message(AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: "old assistant".to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        })
        .unwrap();
    let provider = Arc::new(OverflowProvider {
        stream_calls: std::sync::atomic::AtomicUsize::new(0),
        complete_calls: std::sync::atomic::AtomicUsize::new(0),
        overflow_times: 1,
        fail_summary: false,
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
            .any(|entry| matches!(entry.entry_type, SessionEntryType::Compaction(_)))
    );
}

#[test]
fn second_context_overflow_fails_without_retrying_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    session
        .append_message(AgentMessage {
            role: AgentMessageRole::User,
            content: vec![ContentBlock::Text {
                text: "old user".to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        })
        .unwrap();
    session
        .append_message(AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: "old assistant".to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        })
        .unwrap();
    let provider = Arc::new(OverflowProvider {
        stream_calls: std::sync::atomic::AtomicUsize::new(0),
        complete_calls: std::sync::atomic::AtomicUsize::new(0),
        overflow_times: 2,
        fail_summary: false,
        contract: fake_contract(),
    });
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig::default(),
        session,
    )
    .unwrap();
    let error = agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .expect_err("second overflow fails");
    assert!(error.to_string().contains("context window overflow"));
    assert_eq!(
        provider
            .stream_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(
        provider
            .complete_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[test]
fn failed_force_compaction_returns_original_overflow_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    session
        .append_message(AgentMessage {
            role: AgentMessageRole::User,
            content: vec![ContentBlock::Text {
                text: "old user".to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        })
        .unwrap();
    let provider = Arc::new(OverflowProvider {
        stream_calls: std::sync::atomic::AtomicUsize::new(0),
        complete_calls: std::sync::atomic::AtomicUsize::new(0),
        overflow_times: 1,
        // 强制压缩的摘要生成失败：应保留原始上下文溢出错误（真实因果），
        // 不得把失败掩盖为压缩错误。
        fail_summary: true,
        contract: fake_contract(),
    });
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig::default(),
        session,
    )
    .unwrap();
    let error = agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .expect_err("overflow with failed compaction must fail");
    assert!(
        matches!(error, AgentError::Provider(_)),
        "original overflow error must be preserved, got: {error:?}"
    );
    assert!(error.to_string().contains("context window overflow"));
    assert_eq!(
        provider
            .complete_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[test]
fn preflight_compacts_before_first_normal_request() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let provider = Arc::new(FakeProvider::new(
        fake_contract(),
        vec![
            FakeStep {
                text: "summary".to_string(),
                tool_calls: Vec::new(),
                usage: usage(0, 0),
            },
            FakeStep {
                text: "never sent".to_string(),
                tool_calls: Vec::new(),
                usage: usage(0, 0),
            },
        ],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig {
            context_window: 500,
            max_output_tokens: 1,
            ..AgentConfig::default()
        },
        session,
    )
    .unwrap();
    let error = agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .expect_err("request does not fit even after compaction");
    assert!(error.to_string().contains("still exceeds window"));
    let requests = provider.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|request| request.request_id.starts_with("compaction-")),
        "no normal turn request may be sent: {requests:?}"
    );
}

#[test]
fn preflight_budgets_historical_tool_call_raw_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    // 历史 tool call：content 很小，raw_arguments 巨大。content-only 预算
    // 看不见它，但 provider 会按 wire 重放 id/name/raw_arguments。
    let big_arguments = "x".repeat(20_000);
    session
        .append_message(AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "call write".to_string(),
                },
                ContentBlock::ToolCall {
                    id: "call_big_1".to_string(),
                    name: "write".to_string(),
                    args: json!({ "path": "big.txt", "content": big_arguments }),
                },
            ],
            tool_call_id: Some("call_big_1".to_string()),
            tool_name: Some("write".to_string()),
            provider_reasoning_replay: None,
            is_error: None,
            timestamp: None,
        })
        .unwrap();
    session
        .append_message(AgentMessage {
            role: AgentMessageRole::ToolResult,
            content: vec![ContentBlock::Text {
                text: "wrote".to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: Some("call_big_1".to_string()),
            tool_name: Some("write".to_string()),
            is_error: None,
            timestamp: None,
        })
        .unwrap();
    let provider = Arc::new(FakeProvider::new(
        fake_contract(),
        vec![
            FakeStep {
                text: "summary".to_string(),
                tool_calls: Vec::new(),
                usage: usage(0, 0),
            },
            FakeStep {
                text: "never sent".to_string(),
                tool_calls: Vec::new(),
                usage: usage(0, 0),
            },
        ],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig {
            context_window: 3000,
            max_output_tokens: 1,
            ..AgentConfig::default()
        },
        session,
    )
    .unwrap();
    let error = agent
        .run("task", &mut AgentEvents::new(), &CancellationToken::new())
        .expect_err("large tool-call arguments must be budgeted before the request");
    assert!(error.to_string().contains("still exceeds window"));
    let requests = provider.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|request| request.request_id.starts_with("compaction-")),
        "no normal turn request may carry un-budgeted tool arguments: {requests:?}"
    );
}

#[test]
fn responses_replay_is_recovered_from_durable_assistant_entry() {
    let dir = tempfile::tempdir().unwrap();
    let mut contract = fake_contract();
    contract.tool_reasoning_mode = ProviderToolReasoningMode::ReplayResponsesItems;
    let provider = Arc::new(FakeProvider::new(
        contract,
        vec![FakeStep {
            text: "done".to_string(),
            tool_calls: Vec::new(),
            usage: usage(0, 0),
        }],
    ));
    let replay = ProviderReasoningReplay::Responses {
        provider_name: "provider".to_string(),
        model_name: "model".to_string(),
        reasoning_effort: "high".to_string(),
        tool_call_ids: vec!["call_1".to_string()],
        items: vec![
            json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "opaque"}),
            json!({"type": "function_call", "call_id": "call_1", "name": "write", "arguments": "{}"}),
        ],
    };
    let mut session =
        SessionManager::create(&dir.path().join("project"), &dir.path().join("sessions")).unwrap();
    session
        .append_message(AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: "call_1".to_string(),
                name: "write".to_string(),
                args: json!({"path": "out.txt", "content": "x"}),
            }],
            provider_reasoning_replay: Some(replay.clone()),
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        })
        .unwrap();
    let agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig {
            model: "provider/model#high".to_string(),
            ..AgentConfig::default()
        },
        session,
    )
    .unwrap();
    assert_eq!(agent.reasoning_history_for_request(), vec![replay.clone()]);

    let mut incompatible_session = SessionManager::create(
        &dir.path().join("other-project"),
        &dir.path().join("other-sessions"),
    )
    .unwrap();
    incompatible_session
        .append_message(AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: "call_1".to_string(),
                name: "write".to_string(),
                args: json!({"path": "out.txt", "content": "x"}),
            }],
            provider_reasoning_replay: Some(replay),
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        })
        .unwrap();
    let incompatible = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig {
            model: "other/model#high".to_string(),
            ..AgentConfig::default()
        },
        incompatible_session,
    )
    .unwrap();
    assert!(incompatible.reasoning_history_for_request().is_empty());
}

#[test]
fn orphaned_tool_call_reopens_without_executing_tool_again() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let target = workspace.join("should-not-exist.txt");
    let mut session = SessionManager::create(&workspace, &dir.path().join("sessions")).unwrap();
    session
        .append_message(AgentMessage {
            role: AgentMessageRole::Assistant,
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
            tool_call_id: Some("orphan_write_1".to_string()),
            tool_name: Some("write".to_string()),
            provider_reasoning_replay: None,
            is_error: None,
            timestamp: None,
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
            &entry.entry_type,
            SessionEntryType::Message(message)
                if message.role == AgentMessageRole::ToolResult
                    && message.tool_call_id.as_deref() == Some("orphan_write_1")
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
    assert!(outcome.aborted);
    assert_eq!(outcome.turns, 0);
    // 已取消时不发起任何 provider 调用。
    assert!(provider.requests.lock().unwrap().is_empty());
}

/// 工具执行中途取消：bash 完成前观察到取消 → aborted。
#[test]
fn cancellation_during_tool_execution_aborts() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let session_path = session.path().to_path_buf();
    let provider = Arc::new(FakeProvider::new(
        fake_contract(),
        vec![FakeStep {
            text: String::new(),
            tool_calls: vec![tool_call(
                "call_1",
                "bash",
                json!({ "command": "echo should-not-run" }),
            )],
            usage: usage(10, 5),
        }],
    ));
    let mut agent = Agent::new(
        provider.clone(),
        ToolRegistry::new(),
        AgentConfig::default(),
        session,
    )
    .unwrap();
    // 在工具执行回调中取消：bash 工具在信号检查点观察到取消。
    let cancellation = CancellationToken::new();
    let mut events = AgentEvents::new();
    let mut ended = Vec::new();
    let mut on_tool_execution_end = |name: &str, id: &str, result: &ToolExecution| {
        ended.push((name.to_string(), id.to_string(), result.clone()));
    };
    events.on_tool_execution_end = Some(&mut on_tool_execution_end);
    let canceller = cancellation.clone();
    let mut on_tool_execution_start =
        move |_name: &str, _call_id: &str, _args: &Value| canceller.cancel();
    events.on_tool_execution_start = Some(&mut on_tool_execution_start);
    let outcome = agent.run("go", &mut events, &cancellation).unwrap();
    assert!(outcome.aborted);
    assert_eq!(outcome.turns, 1);
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].0, "bash");
    assert_eq!(ended[0].1, "call_1");
    assert!(ended[0].2.is_error);
    assert!(ended[0].2.content.contains("Command aborted"));
    let session_text = std::fs::read_to_string(session_path).unwrap();
    assert!(session_text.contains("\"role\":\"toolResult\""));
    assert!(session_text.contains("Command aborted"));
}
