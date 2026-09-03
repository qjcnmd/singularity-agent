#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! T038 [US3]：单发上下文溢出恢复与原始根因保留。
//!
//! 不变量（FR-008、data-model Operation.overflow_recovery_used）：provider
//! 明确报告 ContextOverflow 时至多一次强制压缩 + 请求重建；预算按 turn 计，
//! 跨模型步共享；再次溢出不再压缩，以原始根因收敛。Pi 同形：overflow
//! recovery 是 operation 级事实（reducer.ts:587-593 由 step_attempt 记录导出）。

use std::sync::Arc;

use singularity_core::CancellationToken;
use singularity_model::{
    ModelConfigurationSnapshot, ModelErrorKind, Provider, TurnRetryPolicy,
    test_support::{ScriptedAttempt, ScriptedProvider},
};

use super::{Agent, AgentConfig, AgentError, AgentEvent, AgentEvents, TurnInbox};
use crate::compaction::CompactionConfig;
use crate::message::{AgentMessage, AgentMessageRole};
use crate::session::test_support::{SessionFixture, WorkspaceFixture};
use crate::session::{LedgerRecord, OperationKind, SessionEntry, SessionManager, lock_writer};
use crate::tools::ToolRegistrySnapshot;

fn model_snapshot() -> ModelConfigurationSnapshot {
    ScriptedProvider::ok("").model_configuration()
}

fn overflow() -> ScriptedAttempt {
    ScriptedAttempt::failure_kind(
        ModelErrorKind::ContextLengthExceeded,
        "context length exceeded",
    )
}

/// 测试 Agent 的唯一构造点：隔离会话 + 一条 run operation 起始记录，
/// `seed` 在 Agent 接管写者前补充会话前置内容。
fn spawn_agent(
    provider: Arc<dyn Provider + Send + Sync>,
    workspace: &WorkspaceFixture,
    model: &ModelConfigurationSnapshot,
    session_id: &str,
    operation_id: &str,
    keep_recent_tokens: u64,
    seed: impl FnOnce(&mut SessionManager),
) -> (SessionFixture, Agent) {
    let fixture = SessionFixture::new();
    let mut session: SessionManager = fixture
        .create_session(workspace.path(), session_id)
        .expect("create session");
    session
        .append_record(LedgerRecord::OperationStarted {
            operation_id: operation_id.to_string(),
            kind: OperationKind::Run,
            turn_id: Some("turn-1".to_string()),
        })
        .expect("operation started");
    seed(&mut session);
    let writer: crate::session::SessionWriter = std::sync::Arc::new(std::sync::Mutex::new(session));
    let agent = Agent::new(
        TurnInbox::default_handle(),
        provider,
        model.clone(),
        ToolRegistrySnapshot::new(),
        AgentConfig {
            system_prompt: "test prompt".to_string(),
            compaction: CompactionConfig {
                reserve_tokens: 100_000,
                keep_recent_tokens,
            },
        },
        writer,
        std::sync::Arc::default(),
    )
    .expect("agent");
    (fixture, agent)
}

/// 构造带前置历史（可被强制压缩摘要）的会话；reserve 取大值确保主动压缩
/// 不触发，keep_recent 取 1 让强制压缩总有可摘要历史。
fn agent_with_history(
    attempts: impl IntoIterator<Item = ScriptedAttempt>,
    workspace: &WorkspaceFixture,
) -> (SessionFixture, Agent) {
    let provider: Arc<dyn Provider + Send + Sync> = Arc::new(ScriptedProvider::new(attempts));
    let model = model_snapshot();
    spawn_agent(
        provider,
        workspace,
        &model,
        "01914f6b-0000-7000-8000-0000000000e1",
        "op-test",
        1,
        |session| {
            session
                .append_message(AgentMessage::text(
                    AgentMessageRole::User,
                    "old question about the project",
                ))
                .expect("append old user");
            session
                .append_message(AgentMessage::text(
                    AgentMessageRole::Assistant,
                    "old answer with details",
                ))
                .expect("append old assistant");
        },
    )
}

fn overflow_compactions(session: &SessionManager) -> usize {
    session
        .entries()
        .iter()
        .filter(|entry| matches!(entry, SessionEntry::Compaction { .. }))
        .count()
}

/// 首次溢出：恰好一次强制压缩，重建请求后成功收敛。
#[test]
fn overflow_recovers_with_exactly_one_forced_compaction() {
    let workspace = WorkspaceFixture::new();
    let (fixture, mut agent) = agent_with_history(
        [
            overflow(),
            ScriptedAttempt::success("## Goal\ncompacted history"),
            ScriptedAttempt::success("recovered answer"),
        ],
        &workspace,
    );
    let cancellation = CancellationToken::new();
    let outcome = agent
        .run(
            "current question",
            &mut AgentEvents::default(),
            &cancellation,
        )
        .expect("overflow recovery succeeds");
    assert_eq!(outcome.final_text, "recovered answer");
    assert_eq!(outcome.turns, 1);

    let session = agent.session.clone();
    assert_eq!(
        overflow_compactions(&lock_writer(&session)),
        1,
        "exactly one forced overflow compaction"
    );
    assert!(
        lock_writer(&session)
            .entries()
            .iter()
            .any(|entry| matches!(entry, crate::session::SessionEntry::Compaction { .. })),
        "the compaction entry is durable"
    );
    drop(session);
    drop(fixture);
}

/// 第二次溢出（同一步重建后仍超限）：不再压缩，以原始根因失败。
#[test]
fn second_overflow_fails_with_the_original_cause_and_no_second_compaction() {
    let workspace = WorkspaceFixture::new();
    let (_fixture, mut agent) = agent_with_history(
        [
            overflow(),
            ScriptedAttempt::success("## Goal\ncompacted history"),
            overflow(),
        ],
        &workspace,
    );
    let cancellation = CancellationToken::new();
    let error = agent
        .run(
            "current question",
            &mut AgentEvents::default(),
            &cancellation,
        )
        .expect_err("second overflow must fail the turn");
    assert!(
        matches!(
            &error,
            AgentError::Provider(provider)
                if provider.error.kind == ModelErrorKind::ContextLengthExceeded
        ),
        "original overflow cause must be preserved, got {error:?}"
    );
    let session = agent.session.clone();
    assert_eq!(
        overflow_compactions(&lock_writer(&session)),
        1,
        "the recovery budget is consumed once, never twice"
    );
}

/// 预算按 turn 计而非按模型步计：第一步已用掉恢复预算后，后续模型步
/// 再溢出不得触发第二次强制压缩（FR-008 / data-model at-most-once-per-turn）。
#[test]
fn overflow_budget_is_per_turn_not_per_step() {
    let workspace = WorkspaceFixture::new();
    workspace.write_file("notes.txt", "project notes\n");
    let (_fixture, mut agent) = agent_with_history(
        [
            overflow(),
            ScriptedAttempt::success("## Goal\ncompacted history"),
            ScriptedAttempt::tool_call("call-1", "read", serde_json::json!({"path": "notes.txt"})),
            overflow(),
        ],
        &workspace,
    );
    let cancellation = CancellationToken::new();
    let error = agent
        .run(
            "current question",
            &mut AgentEvents::default(),
            &cancellation,
        )
        .expect_err("a later step overflowing after the budget is spent must fail");
    assert!(
        matches!(
            &error,
            AgentError::RunFailed { error, .. }
                if matches!(
                    error.as_ref(),
                    AgentError::Provider(provider)
                        if provider.error.kind == ModelErrorKind::ContextLengthExceeded
                )
        ),
        "progress-bearing failure must keep the overflow root cause, got {error:?}"
    );
    let session = agent.session.clone();
    assert_eq!(
        overflow_compactions(&lock_writer(&session)),
        1,
        "one turn consumes at most one forced overflow recovery"
    );
    // 第一步的恢复确实发生过：工具结果已落盘（read 是 safe 工具，正常执行）。
    assert!(
        lock_writer(&session).entries().iter().any(|entry| {
            matches!(entry, crate::session::SessionEntry::Message { message, .. }
                    if message.role() == AgentMessageRole::ToolResult)
        }),
        "the recovered step's tool batch executed"
    );
}

/// 带自定义模型快照（重试策略等）的会话构造，与 `agent_with_history` 同一
/// 骨架；attempt 观测类测试需要精确控制 provider 与策略。返回的 fixture
/// 守卫会话临时目录的生命周期。
fn agent_with_provider(
    provider: Arc<dyn Provider + Send + Sync>,
    workspace: &WorkspaceFixture,
    model: ModelConfigurationSnapshot,
) -> (SessionFixture, Agent) {
    spawn_agent(
        provider,
        workspace,
        &model,
        "01914f6b-0000-7000-8000-0000000000e2",
        "op-attempt",
        1_000,
        |_| {},
    )
}

/// 可见流之后不得透明重试（contracts/control-provider-tools.md）：attempt
/// 已交付可见文本再失败时，即使错误类别本身可重试也必须原样上抛——绝不
/// 伪装成「没有输出过」重发。同时钉住：durable `provider_attempt` 携带
/// 真实观测到的时长与分类词（来自同一份 attempt 观测，而非事后拼凑）。
#[test]
fn visible_stream_failure_is_never_retried_and_keeps_one_terminal_observation() {
    let workspace = WorkspaceFixture::new();
    let error =
        singularity_model::ProviderError::from_model_error(singularity_model::ModelError::new(
            ModelErrorKind::NetworkError,
            "stream cut after first delta",
        ));
    let provider: Arc<ScriptedProvider> = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::visible_then_fail("partial answer ", error),
        // 若实现退化成重试，会消费这条 attempt 并静默成功——测试即失败。
        ScriptedAttempt::success("must never run"),
    ]));
    let (_fixture, mut agent) = agent_with_provider(
        Arc::clone(&provider) as Arc<dyn Provider + Send + Sync>,
        &workspace,
        model_snapshot(),
    );
    let mut captured_events = Vec::new();
    let mut sink = |event| captured_events.push(event);
    let mut events = AgentEvents {
        on_event: Some(&mut sink),
    };
    let cancellation = CancellationToken::new();
    let failure = agent
        .run("fail after visible text", &mut events, &cancellation)
        .expect_err("a post-visible failure must surface, not retry");
    assert!(
        matches!(
            &failure,
            AgentError::Provider(provider_error)
                if provider_error.error.kind == ModelErrorKind::NetworkError
        ),
        "original typed cause preserved: {failure:?}"
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "no hidden second execution after visible content"
    );
    let session = agent.session.clone();
    let provider_events: Vec<(
        singularity_model::ProviderAttemptStatus,
        u64,
        Option<singularity_model::ModelErrorCategory>,
    )> = captured_events
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::ProviderAttempt {
                event: singularity_model::ProviderAttemptEvent::Finished(occurrence),
                ..
            } => Some((
                occurrence.terminal_status,
                occurrence.attempt_duration_ms,
                occurrence.error_category,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        provider_events,
        vec![(
            singularity_model::ProviderAttemptStatus::Error,
            0u64,
            Some(singularity_model::ModelErrorCategory::Network)
        )],
        "exactly one terminal observation emitted with real duration and category word"
    );
    let visible_assistant_messages: Vec<String> = lock_writer(&session)
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            crate::session::SessionEntry::Message { message, .. }
                if message.role() == AgentMessageRole::Assistant =>
            {
                Some(message.content_text())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        visible_assistant_messages,
        vec!["partial answer ".to_string()],
        "visible streamed text remains in the durable transcript after failure"
    );
}

/// 重试产生连续可观测 attempt：一次限流失败后重试成功，实时面
/// 出现 Error+Ok 两个终态 attempt，attempt 序号单调递增。
#[test]
fn retry_produces_consecutive_attempts_and_emits_telemetry() {
    let workspace = WorkspaceFixture::new();
    let provider: Arc<ScriptedProvider> = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::failure_kind(ModelErrorKind::RateLimited, "slow down"),
        ScriptedAttempt::success("recovered answer"),
    ]));
    let model = ModelConfigurationSnapshot {
        retry: TurnRetryPolicy {
            max_retries: 2,
            base_delay_ms: 1,
        },
        ..model_snapshot()
    };
    let (_fixture, mut agent) = agent_with_provider(
        Arc::clone(&provider) as Arc<dyn Provider + Send + Sync>,
        &workspace,
        model,
    );
    let mut captured_events = Vec::new();
    let mut sink = |event| captured_events.push(event);
    let mut events = AgentEvents {
        on_event: Some(&mut sink),
    };
    let cancellation = CancellationToken::new();
    let outcome = agent
        .run("retry once", &mut events, &cancellation)
        .expect("retry converges");
    assert_eq!(outcome.final_text, "recovered answer");
    assert_eq!(provider.requests().len(), 2);
    let attempts: Vec<singularity_model::ProviderAttemptStatus> = captured_events
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::ProviderAttempt {
                event: singularity_model::ProviderAttemptEvent::Finished(occurrence),
                ..
            } => Some(occurrence.terminal_status),
            _ => None,
        })
        .collect();
    assert_eq!(
        attempts,
        vec![
            singularity_model::ProviderAttemptStatus::Error,
            singularity_model::ProviderAttemptStatus::Ok
        ]
    );
}
