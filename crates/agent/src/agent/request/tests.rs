#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
use super::*;
use crate::agent::{Agent, AgentConfig, AgentEvent};
use crate::message::{AgentMessage, ContentBlock};
use crate::session::SessionManager;
use serde_json::json;
use singularity_model::Provider;
use singularity_model::test_support::ScriptedProvider;
use std::sync::Arc;

fn assistant_with_replay(
    call_id: &str,
    replay: Option<singularity_model::ProviderReasoningReplay>,
) -> AgentMessage {
    AgentMessage::Assistant {
        content: vec![ContentBlock::ToolCall(singularity_model::ModelToolCall {
            tool_call_id: call_id.to_string(),
            tool_name: "read".to_string(),
            arguments: json!({"path": "a"}),
        })],
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
        reasoning_field: "reasoning_content".to_string(),
        reasoning_details: Vec::new(),
    }
}

fn agent_with(provider: Arc<dyn Provider + Send + Sync>, session: SessionManager) -> Agent {
    let model = provider.model_configuration();
    let registry = crate::tools::ToolRegistrySnapshot::default();
    let config = AgentConfig {
        instruction_home: None,
        initial_instructions: None,
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
    )
    .expect("agent")
}

#[test]
fn pruning_covers_the_whole_history_and_reopens_identically() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    for calls in [vec!["old"], vec!["recent-a", "recent-b"]] {
        session
            .append_message(AgentMessage::Assistant {
                content: calls
                    .iter()
                    .map(|id| {
                        ContentBlock::ToolCall(singularity_model::ModelToolCall {
                            tool_call_id: (*id).into(),
                            tool_name: "read".into(),
                            arguments: json!({"path":id}),
                        })
                    })
                    .collect(),
                stop_reason: None,
                provider_reasoning_replay: None,
            })
            .unwrap();
        for id in calls {
            session
                .append_message(crate::message::tool_result_text(
                    id,
                    format!("{}important-{id}{}", "a".repeat(5000), "z".repeat(5000)),
                    false,
                ))
                .unwrap();
        }
    }
    let path = session.path().to_path_buf();
    let mut agent = agent_with(Arc::new(ScriptedProvider::ok("unused")), session);
    assert!(agent.prune_tool_results(&CancellationToken::new()).unwrap());
    let messages = agent.assemble_messages();
    let result = |id: &str| {
        messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some(id))
            .unwrap()
            .content
            .as_str()
    };
    // 剪枝覆盖整个活动历史：近期工具结果同样被剪。
    for id in ["old", "recent-a", "recent-b"] {
        assert!(!result(id).contains(&format!("important-{id}")));
        assert!(result(id).contains("[... tool result middle pruned ...]"));
    }
    assert!(!agent.prune_tool_results(&CancellationToken::new()).unwrap());
    drop(agent);
    let reopened = agent_with(
        Arc::new(ScriptedProvider::ok("unused")),
        SessionManager::open_existing(&path).unwrap(),
    );
    assert_eq!(reopened.assemble_messages(), messages);
}

/// 历史与恢复只传递原消息的私有数据；切换模型的兼容处理在 Provider 层。
#[test]
fn history_preserves_continuation_attached_to_each_message() {
    let dir = tempfile::tempdir().expect("temp");
    let mut session =
        SessionManager::create(dir.path(), &dir.path().join("sessions")).expect("session");
    for (id, provider) in [("same", "scripted"), ("other", "someone-else")] {
        session
            .append_message(assistant_with_replay(
                id,
                Some(chat_replay(provider, "scripted-model", id)),
            ))
            .expect("append");
    }
    let agent = agent_with(Arc::new(ScriptedProvider::ok("unused")), session);
    let messages = agent.assemble_messages();
    let attached: Vec<_> = messages
        .iter()
        .filter(|message| message.provider_reasoning_replay.is_some())
        .map(|message| message.tool_calls[0].tool_call_id.as_str())
        .collect();
    assert_eq!(attached, ["same", "other"]);
    assert!(
        !serde_json::to_string(&messages)
            .unwrap()
            .contains("private continuation trace")
    );
}

/// 使用和工作台相同的配置保存入口，经过真实 HTTP/SSE、工具与 Session 恢复。
/// 模型默认思考但没有 effort 元数据时，工具前和最终回复的续接均须保留。
#[test]
fn default_model_setup_replays_continuation_through_tools_and_reopen() {
    use singularity_model::ModelConfigManager;
    use singularity_protocol::{ModelConfigurationInput, ProviderConfigurationInput};

    for format in [
        "reasoning_content",
        "reasoning",
        "reasoning_text",
        "reasoning_details",
        "responses",
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("probe.txt"), "actual tool result").unwrap();
        let (base_url, server) = continuation_server(format);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut owner = ModelConfigManager::open(dir.path().join("home"));
        owner
            .save_provider(
                ProviderConfigurationInput {
                    provider_id: "fixture".into(),
                    display_name: None,
                    base_url,
                    models: vec![ModelConfigurationInput {
                        model_id: "test-model".into(),
                        display_name: None,
                        api_protocol: if format == "responses" {
                            Some("responses".into())
                        } else {
                            Some("chat".into())
                        },
                        max_context_tokens: Some(128_000),
                        max_output_tokens: Some(8192),
                        reasoning_variants: Vec::new(),
                        default_variant: None,
                        thinking_wire_format: None,
                        chat_output_tokens_field: None,
                    }],
                },
                Some("synthetic-key"),
            )
            .unwrap();
        let provider = Arc::new(
            singularity_model::OpenAiProvider::from_snapshot(
                &owner.snapshot(),
                None,
                runtime.handle().clone(),
            )
            .unwrap(),
        );
        let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        let path = session.path().to_path_buf();
        let mut agent = agent_with(provider.clone(), session);
        let mut events = Vec::new();
        agent
            .run(
                "read probe.txt",
                &mut |event| events.push(event),
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::MessageFinished { items, failed: false, .. }
                if items.iter().any(|item| matches!(item,
                    singularity_protocol::HistoryItem::Message { text, .. } if text == "done"))
        )));
        if format == "responses" {
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, AgentEvent::MessageUpdate { .. })),
                "response.completed carries the final content without any text deltas"
            );
        }
        if matches!(format, "reasoning_content" | "reasoning" | "reasoning_text") {
            let writer = lock_writer(&agent.session);
            let thinking: Vec<_> = writer
                .entries()
                .iter()
                .flat_map(|entry| match entry {
                    SessionEntry::Message { message, .. } => message.content(),
                    _ => &[],
                })
                .filter_map(|block| match block {
                    ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(thinking, ["private-1", "private-2"]);
        }
        assert!(
            !serde_json::to_string(&agent.assemble_messages())
                .unwrap()
                .contains("private-")
        );
        drop(agent);

        let mut reopened = agent_with(provider, SessionManager::open_existing(&path).unwrap());
        let mut reopened_events = Vec::new();
        reopened
            .run(
                "continue",
                &mut |event| reopened_events.push(event),
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(reopened_events.iter().any(|event| matches!(
            event,
            AgentEvent::MessageFinished { items, failed: false, .. }
                if items.iter().any(|item| matches!(item,
                    singularity_protocol::HistoryItem::Message { text, .. } if text == "done"))
        )));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3, "{format}");
        assert_eq!(requests[0]["model"], "test-model");
        let output_key = if format == "responses" {
            "max_output_tokens"
        } else {
            "max_tokens"
        };
        assert_eq!(requests[0][output_key], 8192);
        assert!(requests[0]["tools"].as_array().unwrap().iter().any(|tool| {
            tool["name"]
                .as_str()
                .or_else(|| tool["function"]["name"].as_str())
                == Some("read")
        }));
        assert!(
            requests
                .iter()
                .all(|request| request.get("reasoning_effort").is_none()
                    && request.get("thinking").is_none()
                    && request.get("reasoning").is_none())
        );
        let history_key = if format == "responses" {
            "input"
        } else {
            "messages"
        };
        let second = serde_json::to_string(&requests[1][history_key]).unwrap();
        assert!(
            second.contains("actual tool result"),
            "{format}: tool was executed"
        );
        assert!(second.contains("private-1"), "{format}: tool continuation");
        let resumed = serde_json::to_string(&requests[2][history_key]).unwrap();
        assert!(
            resumed.contains("private-1") && resumed.contains("private-2"),
            "{format}: resumed history"
        );
        if format == "responses" {
            assert_eq!(
                requests[0]["include"],
                json!(["reasoning.encrypted_content"])
            );
        } else {
            let assistant = requests[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["role"] == "assistant")
                .unwrap();
            assert!(
                assistant.get(format).is_some(),
                "original field identity: {format}"
            );
        }
    }
}

fn continuation_server(
    format: &'static str,
) -> (String, std::thread::JoinHandle<Vec<serde_json::Value>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for step in 1..=3 {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("fixture HTTP request {step} was not received: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut content_length = 0;
            loop {
                let mut header = String::new();
                assert_ne!(reader.read_line(&mut header).unwrap(), 0);
                if header == "\r\n" {
                    break;
                }
                if let Some((key, value)) = header.split_once(':')
                    && key.eq_ignore_ascii_case("content-length")
                {
                    content_length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            requests.push(serde_json::from_slice(&body).unwrap());
            let data = continuation_response(format, step);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", data.len(), data).unwrap();
        }
        requests
    });
    (url, server)
}

fn continuation_response(format: &str, step: usize) -> String {
    let private = format!("private-{step}");
    if format == "responses" {
        let mut output = vec![
            json!({"type":"reasoning", "id":format!("r-{step}"), "encrypted_content":private, "summary":[]}),
        ];
        output.push(if step == 1 {
            json!({"type":"function_call", "id":"f-1", "call_id":"read-1", "name":"read", "arguments":"{\"path\":\"probe.txt\"}"})
        } else {
            json!({"type":"message", "id":format!("m-{step}"), "role":"assistant", "content":[{"type":"output_text","text":"done"}]})
        });
        return format!(
            "event: response.completed\ndata: {}\n\n",
            json!({"type":"response.completed", "response":{"id":format!("response-{step}"), "status":"completed", "output":output, "usage":{"input_tokens":20,"output_tokens":5,"total_tokens":25}}})
        );
    }
    let mut delta = json!({"role":"assistant"});
    delta[format] = if format == "reasoning_details" {
        json!([{"type":"reasoning.encrypted","data":private,"id":format!("r-{step}")}])
    } else {
        json!(private)
    };
    if step == 1 {
        delta["tool_calls"] = json!([{"index":0,"id":"read-1","type":"function","function":{"name":"read","arguments":"{\"path\":\"probe.txt\"}"}}]);
    } else {
        delta["content"] = json!("done");
    }
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":if step == 1 {"tool_calls"} else {"stop"}}],"usage":{"prompt_tokens":20,"completion_tokens":5,"total_tokens":25}})
    )
}

#[test]
fn execute_request_stops_before_transport_when_request_record_exceeds_limit() {
    use singularity_model::{ModelMessage, ModelRole};

    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let scripted = Arc::new(ScriptedProvider::ok("must not send"));
    let provider: Arc<dyn Provider + Send + Sync> = scripted.clone();
    let mut agent = agent_with(provider, session);
    let mut request = ModelTurnRequest::new(
        "",
        vec![ModelMessage::text(
            ModelRole::System,
            "x".repeat(16 * 1024 * 1024),
        )],
    );
    let cancellation = CancellationToken::new();
    let mut events = |_| {};
    let result = crate::request_execution::execute_request(
        agent.provider.as_ref(),
        &agent.session,
        &mut agent.accounting,
        &mut request,
        &mut events,
        &cancellation,
        1,
        singularity_protocol::RequestPurpose::Generation,
    );
    assert!(matches!(
        result,
        Err(AgentError::Session(
            crate::session::SessionError::AppendLimitExceeded { .. }
        ))
    ));
    assert!(scripted.requests().is_empty());
}

#[test]
fn exhausted_provider_finishes_the_attempt_and_marks_usage_unknown() {
    use crate::events::AgentEvent;
    use singularity_model::{ModelMessage, ModelRole, ProviderAttemptStatus};

    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let mut agent = agent_with(Arc::new(ScriptedProvider::new([])), session);
    let mut request = ModelTurnRequest::new("", vec![ModelMessage::text(ModelRole::User, "hello")]);
    let mut statuses = Vec::new();
    let mut sink = |event| {
        if let AgentEvent::ProviderAttempt { observation, .. } = event {
            statuses.push(observation.status);
        }
    };
    let result = crate::request_execution::execute_request(
        agent.provider.as_ref(),
        &agent.session,
        &mut agent.accounting,
        &mut request,
        &mut sink,
        &CancellationToken::new(),
        1,
        singularity_protocol::RequestPurpose::Generation,
    );
    assert!(matches!(result, Err(AgentError::Provider(error))
        if error.message.contains("ran out of scripted attempts")));
    assert_eq!(
        statuses,
        [ProviderAttemptStatus::Started, ProviderAttemptStatus::Error]
    );
    assert!(
        !agent.request_usage().1,
        "an unmeasured attempt cannot report complete usage"
    );
}

#[test]
fn execute_request_recording_failure_stops_retries_and_preserves_storage_error_and_measured_usage()
{
    use crate::events::AgentEvent;
    use singularity_model::{ModelMessage, ModelRole, test_support::ScriptedAttempt};
    use singularity_protocol::RequestPurpose;

    for purpose in [RequestPurpose::Generation, RequestPurpose::Compaction] {
        for fail_before_start in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
            let path = session.path().to_path_buf();
            let usage = singularity_model::ModelUsage {
                input_tokens: 5,
                output_tokens: 2,
                total_tokens: 7,
                usage_present: true,
                ..singularity_model::ModelUsage::default()
            };
            let scripted = Arc::new(ScriptedProvider::new([
                ScriptedAttempt::success_with_usage("response", usage.clone()),
            ]));
            let provider: Arc<dyn Provider + Send + Sync> = scripted.clone();
            let mut agent = agent_with(provider, session);
            let mut request =
                ModelTurnRequest::new("", vec![ModelMessage::text(ModelRole::User, "hello")]);
            let cancellation = CancellationToken::new();
            let mut observed = Vec::new();
            let mut sink = |event| {
                if let AgentEvent::ProviderAttempt { observation, .. } = &event
                    && observation.status == singularity_protocol::ProviderAttemptStatus::Started
                {
                    // start 记录已落盘，但 finish 记录会失败。
                    std::fs::remove_file(&path).unwrap();
                }
                observed.push(event);
            };
            if fail_before_start {
                std::fs::remove_file(&path).unwrap();
            }
            let result = crate::request_execution::execute_request(
                agent.provider.as_ref(),
                &agent.session,
                &mut agent.accounting,
                &mut request,
                &mut sink,
                &cancellation,
                1,
                purpose,
            );
            assert!(matches!(result, Err(AgentError::Session(
                    crate::session::SessionError::Io(error)
                )) if error.kind() == std::io::ErrorKind::NotFound));
            assert!(!cancellation.is_cancelled());
            assert_eq!(scripted.requests().len(), usize::from(!fail_before_start));
            let (accounting_usage, complete) = agent.request_usage();
            let expected_usage = if fail_before_start {
                singularity_model::ModelUsage::default()
            } else {
                usage
            };
            assert_eq!(accounting_usage, &expected_usage);
            assert!(complete);
            assert!(!observed.iter().any(|event| matches!(event, AgentEvent::ProviderAttempt { observation, .. } if observation.status != singularity_protocol::ProviderAttemptStatus::Started)));
            let streamed = observed
                .iter()
                .any(|event| matches!(event, AgentEvent::MessageUpdate { .. }));
            assert_eq!(
                streamed,
                !fail_before_start && purpose == RequestPurpose::Generation
            );
        }
    }
}
