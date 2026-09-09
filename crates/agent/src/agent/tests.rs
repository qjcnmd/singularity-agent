#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! 单轮上下文溢出恢复与原始根因保留测试。
//!
//! 核心不变量：当模型提供方明确返回上下文溢出（ContextOverflow）时，单个 Turn 内
//! 至多执行一次强制压缩并重建请求；溢出恢复预算按 Turn 计量，跨模型步共享。
//! 若压缩重试后依然溢出，则停止重复压缩，向调用方准确抛出原始根因。

use std::sync::Arc;

use singularity_core::CancellationToken;
use singularity_model::{
    ModelConfigurationSnapshot, ModelErrorKind, Provider, TurnRetryPolicy,
    test_support::{ScriptedAttempt, ScriptedProvider},
};

use super::{Agent, AgentConfig, AgentError, AgentEvent, AgentEvents, TurnInbox};
use crate::compaction::CompactionConfig;
use crate::message::{AgentMessage, AgentMessageRole, ContentBlock};
use crate::session::context::ContextView;
use crate::session::test_support::{SessionFixture, WorkspaceFixture};
use crate::session::{
    LedgerRecord, OperationKind, SessionData, SessionEntry, SessionManager, lock_writer,
};
use crate::tools::ToolRegistrySnapshot;

fn model_snapshot() -> ModelConfigurationSnapshot {
    ScriptedProvider::ok("").model_configuration()
}

#[test]
fn mutation_receipt_excludes_diff_from_model_but_preserves_it_for_replay() {
    let workspace = WorkspaceFixture::new();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::tool_call(
            "write-1",
            "write",
            serde_json::json!({
                "path": "note.txt", "content": "unique file body\n"
            }),
        ),
        ScriptedAttempt::success("done"),
    ]));
    let (_fixture, mut agent) = agent_with_provider(provider.clone(), &workspace, model_snapshot());
    let mut observed_diff = None;
    let mut on_event = |event| {
        if let AgentEvent::ToolExecutionEnded { execution, .. } = event {
            observed_diff = execution.diff;
        }
    };
    agent
        .run(
            "write it",
            &mut AgentEvents {
                on_event: Some(&mut on_event),
            },
            &CancellationToken::new(),
        )
        .unwrap();
    let requests = provider.requests();
    let receipt = requests[1].messages.last().unwrap();
    assert!(receipt.content.contains("Successfully wrote"));
    assert!(!receipt.content.contains("unique file body"));
    assert!(
        observed_diff
            .as_deref()
            .unwrap()
            .contains("+unique file body")
    );
    let path = lock_writer(&agent.session).path().to_path_buf();
    drop(agent);
    let reopened = SessionData::open(&path).unwrap();
    let saved = reopened
        .entries()
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Message { message, .. }
                if message.tool_call_id().is_some_and(|id| id == "write-1") =>
            {
                Some(message)
            }
            _ => None,
        })
        .unwrap();
    assert!(matches!(saved, AgentMessage::ToolResult { diff, .. } if *diff == observed_diff));
    assert_eq!(saved.content_text(), receipt.content);
}

#[test]
fn completed_tool_is_already_durable_when_event_is_delivered() {
    let workspace = WorkspaceFixture::new();
    workspace.write_file("note.txt", "persist me");
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::tool_call("read-1", "read", serde_json::json!({"path":"note.txt"})),
        ScriptedAttempt::success("done"),
    ]));
    let (_fixture, mut agent) = agent_with_provider(provider, &workspace, model_snapshot());
    let writer = agent.session.clone();
    let mut checked = false;
    let mut on_event = |event| {
        if let AgentEvent::ToolExecutionEnded { tool_call_id, .. } = event {
            let session = lock_writer(&writer);
            assert!(session.entries().iter().any(|entry| matches!(entry, SessionEntry::Message { message, .. } if message.tool_call_id() == Some(&tool_call_id))));
            checked = true;
        }
    };
    agent
        .run(
            "read it",
            &mut AgentEvents {
                on_event: Some(&mut on_event),
            },
            &CancellationToken::new(),
        )
        .unwrap();
    assert!(checked);
}

#[test]
fn manual_and_model_skills_share_body_and_survive_context_rebuild() {
    let workspace = WorkspaceFixture::new();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("skills")).unwrap();
    std::fs::write(
        home.path().join("skills/review.md"),
        "---\nname: review\ndescription: Review changes\n---\nSkill body for review",
    )
    .unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::tool_call("skill-1", "skill", serde_json::json!({"name":"review"})),
        ScriptedAttempt::success("done"),
    ]));
    let (_fixture, mut agent) = agent_with_provider(provider, &workspace, model_snapshot());
    agent.config.instruction_home = Some(home.path().to_path_buf());
    agent.registry.skills =
        singularity_core::skills::SkillCatalog::discover(workspace.path(), home.path());
    agent
        .run(
            "/review this change",
            &mut AgentEvents::default(),
            &CancellationToken::new(),
        )
        .unwrap();
    let session = lock_writer(&agent.session);
    let manual = session
        .entries()
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Record {
                record: LedgerRecord::SkillInstructions { text },
                ..
            } => Some(text),
            _ => None,
        })
        .unwrap();
    let automatic = session
        .entries()
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Message { message, .. }
                if message.tool_call_id().is_some_and(|id| id == "skill-1") =>
            {
                Some(message.content_text())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(manual, &automatic);
    assert!(manual.contains("Skill body for review"));
    let restored = ContextView::derive(&session).unwrap();
    assert_eq!(restored.entries(), agent.context.entries());
}

fn overflow() -> ScriptedAttempt {
    ScriptedAttempt::failure_kind(
        ModelErrorKind::ContextLengthExceeded,
        "context length exceeded",
    )
}

/// 测试 Agent 的唯一构造点：隔离会话 + 一条 run operation 起始记录，
/// seed 在 Agent 接管写者前补充会话前置内容。
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
            instruction_home: None,
            system_prompt: "test prompt".to_string(),
            compaction: CompactionConfig {
                threshold_ratio: 0.9,
                retain_ratio: keep_recent_tokens as f64 / model.context_window() as f64,
            },
        },
        writer,
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
                    "old question about the project ".repeat(100),
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
/// 再溢出不得触发第二次强制压缩（单个 turn 至多触发一次）。
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
            AgentError::Provider(provider)
                if provider.error.kind == ModelErrorKind::ContextLengthExceeded
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

/// 带自定义模型快照（重试策略等）的会话构造，与 agent_with_history 同一
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
/// 伪装成「没有输出过」重发。同时钉住：durable provider_attempt 携带
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

#[test]
fn file_instructions_reload_after_compaction_without_changing_system_prompt() {
    let workspace = WorkspaceFixture::new();
    workspace.write_file("AGENTS.md", "project rules v1");
    let provider = Arc::new(ScriptedProvider::new([ScriptedAttempt::success(
        "checkpoint",
    )]));
    let (fixture, mut agent) = agent_with_provider(provider.clone(), &workspace, model_snapshot());
    std::fs::write(fixture.home().join("AGENTS.md"), "global rules").unwrap();
    agent.config.instruction_home = Some(fixture.home().to_path_buf());
    agent.refresh_instructions().unwrap();
    let before_count = lock_writer(&agent.session).entries().len();
    agent.refresh_instructions().unwrap();
    assert_eq!(
        lock_writer(&agent.session).entries().len(),
        before_count,
        "visible unchanged instructions are not duplicated"
    );
    {
        let mut session = lock_writer(&agent.session);
        session
            .append_message(AgentMessage::text(
                AgentMessageRole::User,
                "old work ".repeat(500),
            ))
            .unwrap();
        session
            .append_message(AgentMessage::text(
                AgentMessageRole::Assistant,
                "recent response",
            ))
            .unwrap();
    }
    agent.context.rebuild(&lock_writer(&agent.session)).unwrap();
    workspace.write_file("AGENTS.md", "project rules v2");
    agent
        .compact_now(&mut AgentEvents::default(), &CancellationToken::new())
        .unwrap();
    let requests = provider.requests();
    assert_eq!(requests[0].messages[0].content, "test prompt");
    assert!(
        requests[0]
            .messages
            .iter()
            .any(|message| message.content.contains("project rules v1"))
    );
    let messages = agent.assemble_messages();
    assert_eq!(messages[0].content, "test prompt");
    assert!(
        messages
            .iter()
            .any(|message| message.content.contains("global rules")
                && message.content.contains("project rules v2"))
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.content.contains("project rules v1"))
    );
    assert!(matches!(
        messages.last().unwrap().role,
        singularity_model::ModelRole::User
    ));
}

#[test]
fn pressure_prunes_old_results_without_summarizing_when_that_is_enough() {
    let workspace = WorkspaceFixture::new();
    let provider = Arc::new(ScriptedProvider::new([ScriptedAttempt::success("done")]));
    let mut model = model_snapshot();
    model.capabilities.max_context_tokens = Some(4000);
    let (_fixture, mut agent) = spawn_agent(
        provider.clone(),
        &workspace,
        &model,
        "01914f6b-0000-7000-8000-0000000000ec",
        "prune",
        100,
        |session| {
            session
                .append_message(AgentMessage::Assistant {
                    content: vec![ContentBlock::ToolCall {
                        id: "one".into(),
                        name: "read".into(),
                        args: serde_json::json!({"path":"a"}),
                    }],
                    stop_reason: None,
                    provider_reasoning_replay: None,
                })
                .unwrap();
            session
                .append_message(AgentMessage::ToolResult {
                    content: vec![ContentBlock::Text {
                        text: "x".repeat(16000),
                    }],
                    tool_call_id: Some("one".into()),
                    tool_name: Some("read".into()),
                    is_error: Some(false),
                    duration_ms: None,
                    diff: None,
                })
                .unwrap();
            session
                .append_message(AgentMessage::text(
                    AgentMessageRole::Assistant,
                    "recent answer ".repeat(100),
                ))
                .unwrap();
        },
    );
    let result = agent
        .run(
            "continue",
            &mut AgentEvents::default(),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(result.final_text, "done");
    assert_eq!(provider.requests().len(), 1);
    assert!(
        provider.requests()[0]
            .messages
            .iter()
            .any(|message| message.content.contains("tool result middle pruned"))
    );
    assert!(lock_writer(&agent.session).entries().iter().any(|entry| matches!(entry, SessionEntry::Message { message: AgentMessage::ToolResult { content, .. }, .. } if matches!(&content[0], ContentBlock::Text { text } if text.len() == 16000))));
}

#[test]
fn summary_usage_and_unknown_overflow_are_included_in_operation_total() {
    let usage = |tokens| singularity_model::ModelUsage {
        input_tokens: tokens,
        total_tokens: tokens,
        usage_present: true,
        ..Default::default()
    };
    let workspace = WorkspaceFixture::new();
    let (_fixture, mut agent) = agent_with_history(
        [
            overflow(),
            ScriptedAttempt::success_with_usage("short checkpoint", usage(100)),
            ScriptedAttempt::success_with_usage("done", usage(10)),
        ],
        &workspace,
    );
    let mut purposes = Vec::new();
    let mut sink = |event| {
        if let AgentEvent::ProviderAttempt {
            purpose,
            event: singularity_model::ProviderAttemptEvent::Finished(_),
            ..
        } = event
        {
            purposes.push(purpose);
        }
    };
    let outcome = agent
        .run(
            "finish",
            &mut AgentEvents {
                on_event: Some(&mut sink),
            },
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(outcome.usage.input_tokens, 110);
    assert!(
        !outcome.usage_complete,
        "the rejected request has unknown usage"
    );
    assert_eq!(
        purposes,
        vec![
            singularity_protocol::RequestPurpose::Generation,
            singularity_protocol::RequestPurpose::Compaction,
            singularity_protocol::RequestPurpose::Generation
        ]
    );
}

#[test]
fn failed_first_summary_keeps_measured_usage_without_an_assistant_turn() {
    let workspace = WorkspaceFixture::new();
    let (_fixture, mut agent) = agent_with_history(
        [ScriptedAttempt::success_with_usage(
            " ",
            singularity_model::ModelUsage {
                input_tokens: 100,
                total_tokens: 100,
                usage_present: true,
                ..Default::default()
            },
        )],
        &workspace,
    );
    assert!(
        agent
            .compact_now(&mut AgentEvents::default(), &CancellationToken::new())
            .is_err()
    );
    assert_eq!(agent.request_usage().0.input_tokens, 100);
    assert!(agent.request_usage().1);
}
