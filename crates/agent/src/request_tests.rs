#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
use super::*;
use crate::agent::{Agent, AgentConfig};
use crate::message::{AgentMessage, ContentBlock};
use crate::session::SessionManager;
use serde_json::json;
use singularity_model::{
    ModelToolSchema, ProviderProtocolContract, ProviderToolReasoningMode,
    test_support::ScriptedProvider,
};
use std::sync::Arc;

#[test]
fn response_room_is_reserved_before_ninety_percent_and_can_reach_zero() {
    let reserve = response_reserve(32768, 0.9, 8192);
    assert!(output_token_budget(32768, 29000, 8192) < reserve);
    assert!(29000 < (32768.0 * 0.9) as u64);
    assert_eq!(output_token_budget(32768, 32768, 8192), 0);
    assert!(output_token_budget(4000, 1000, 8192) >= response_reserve(4000, 0.9, 8192));
}

fn assistant_with_replay(
    call_id: &str,
    replay: Option<singularity_model::ProviderReasoningReplay>,
) -> AgentMessage {
    AgentMessage::Assistant {
        content: vec![ContentBlock::ToolCall {
            id: call_id.to_string(),
            name: "read".to_string(),
            args: json!({"path": "a"}),
        }],
        stop_reason: None,
        provider_reasoning_replay: replay,
    }
}

fn chat_replay(
    provider: &str,
    model: &str,
    call_id: &str,
) -> singularity_model::ProviderReasoningReplay {
    singularity_model::ProviderReasoningReplay::Chat {
        provider_name: provider.to_string(),
        model_name: model.to_string(),
        reasoning_effort: None,
        tool_call_ids: vec![call_id.to_string()],
        reasoning_content: "private continuation trace".to_string(),
    }
}

fn agent_with(provider: Arc<dyn Provider + Send + Sync>, session: SessionManager) -> Agent {
    let model = provider.model_configuration();
    let registry = crate::tools::ToolRegistrySnapshot::new();
    let config = AgentConfig {
        instruction_home: None,
        system_prompt: "you are a coding agent".to_string(),
        compaction: crate::compaction::CompactionConfig::default(),
    };
    Agent::new(
        crate::agent::TurnInbox::default_handle(),
        provider,
        model,
        registry,
        config,
        std::sync::Arc::new(std::sync::Mutex::new(session)),
        std::sync::Arc::default(),
    )
    .expect("agent")
}

/// 装配单轮请求的模型名、工具 schema 与输出上限全部出自同一 provider 快照与
/// 同一注册表快照：不存在第二处 selector 解析或工具名单派生。
#[test]
fn request_model_tools_and_output_all_derive_from_one_snapshot() {
    let dir = tempfile::tempdir().expect("temp");
    let session =
        SessionManager::create(dir.path(), &dir.path().join("sessions")).expect("session");
    let provider = Arc::new(ScriptedProvider::ok("unused"));
    let snapshot = provider.model_configuration();
    let agent = agent_with(provider, session);

    let registry = crate::tools::ToolRegistrySnapshot::new();
    let spec = TurnRequestSpec {
        tools: registry.provider_schemas(&snapshot.capabilities),
        turn: 0,
    };
    let request = agent.build_request(&spec);

    // 模型身份取自快照，不解析 selector 字符串。
    assert_eq!(
        request.model_preferences.model_name.as_deref(),
        Some(snapshot.model.as_str())
    );
    // 工具 schema 与注册表快照同源，名称集合一致。
    let request_tools: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
    let registry_tools: Vec<String> = registry
        .provider_schemas(&snapshot.capabilities)
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(
        request_tools, registry_tools,
        "one registry snapshot drives schemas"
    );
    // 输出上限不超过快照声明。
    assert!(
        u64::from(request.model_preferences.max_output_tokens.unwrap_or(0))
            <= snapshot.max_output_tokens(),
        "effective output cap respects the snapshot"
    );
}

/// provider 声明更小的工具上限时，请求工具集随之截断，与 schema 投影同源。
#[test]
fn request_tools_are_capped_by_snapshot_capability() {
    let dir = tempfile::tempdir().expect("temp");
    let session =
        SessionManager::create(dir.path(), &dir.path().join("sessions")).expect("session");
    let contract = ProviderProtocolContract {
        max_tools_per_request: 1,
        ..ProviderProtocolContract::default()
    };
    let provider = Arc::new(ScriptedProvider::ok("unused").with_contract(contract));
    let snapshot = provider.model_configuration();
    let agent = agent_with(provider, session);
    let registry = crate::tools::ToolRegistrySnapshot::new();
    let spec = TurnRequestSpec {
        tools: registry.provider_schemas(&snapshot.capabilities),
        turn: 0,
    };
    let request = agent.build_request(&spec);
    assert_eq!(
        request.tools.len(),
        1,
        "capped to the snapshot's declared maximum"
    );
    assert_eq!(
        request.tools[0],
        ModelToolSchema {
            name: "bash".to_string(),
            description: registry.get("bash").expect("bash").description.to_string(),
            parameters_schema: registry.get("bash").expect("bash").parameters.clone(),
        }
    );
}

/// reasoning replay 只在与本 turn 冻结快照的 (provider, model, variant, mode)
/// 完全一致时才随请求发出；绑定不符的历史 continuation 一律丢弃，绝不以当前
/// 身份重放。
#[test]
fn reasoning_replay_is_dropped_unless_bound_to_the_frozen_snapshot() {
    let dir = tempfile::tempdir().expect("temp");
    let mut session =
        SessionManager::create(dir.path(), &dir.path().join("sessions")).expect("session");
    // 匹配快照的 replay（provider/model 与 ScriptedProvider 一致，mode 对齐）。
    session
        .append_message(assistant_with_replay(
            "call-match",
            Some(chat_replay("scripted", "scripted-model", "call-match")),
        ))
        .expect("append matching");
    // 绑定到另一 provider 的 replay：必须被丢弃。
    session
        .append_message(assistant_with_replay(
            "call-mismatch",
            Some(chat_replay(
                "someone-else",
                "scripted-model",
                "call-mismatch",
            )),
        ))
        .expect("append mismatched");

    let contract = ProviderProtocolContract {
        tool_reasoning_mode: ProviderToolReasoningMode::ReplayReasoningContent,
        ..ProviderProtocolContract::default()
    };
    let provider = Arc::new(ScriptedProvider::ok("unused").with_contract(contract));
    let agent = agent_with(provider, session);

    let (_, replays) = agent.assemble_messages();
    assert_eq!(replays.len(), 1, "only the snapshot-bound replay survives");
    assert!(
        replays[0].matches_tool_call_ids(&["call-match".to_string()]),
        "the kept replay is the matching one"
    );
}

#[test]
fn failed_attempt_preserves_public_thinking_and_text_under_its_result_id_once() {
    let dir = tempfile::tempdir().expect("temp");
    let session =
        SessionManager::create(dir.path(), &dir.path().join("sessions")).expect("session");
    let writer = Arc::new(std::sync::Mutex::new(session));
    let mut attempts = 0;
    let mut ledger = AttemptLedger::new(&writer, &mut attempts);
    ledger.begin();
    let id = ledger.result_entry_id().to_string();
    ledger.persist_visible_assistant("visible text", "visible thinking");
    ledger.persist_visible_assistant("duplicate", "duplicate");
    assert!(ledger.take_store_failure().is_none());
    let writer = lock_writer(&writer);
    let records: Vec<_> = writer
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message {
                id: actual,
                message,
                ..
            } if actual == &id => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].content_text(), "visible text");
    assert!(
        matches!(&records[0].content()[0], ContentBlock::Thinking { thinking, .. } if thinking == "visible thinking")
    );
    assert!(records[0].provider_reasoning_replay().is_none());
}
