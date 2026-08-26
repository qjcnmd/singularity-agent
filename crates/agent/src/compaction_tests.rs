use super::*;
use singularity_model::{ModelTurnResponse, ModelUsage, ProviderProtocolContract};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

fn user(text: &str) -> AgentMessage {
    AgentMessage::User {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

fn assistant(text: &str) -> AgentMessage {
    AgentMessage::Assistant {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        stop_reason: None,
        provider_reasoning_replay: None,
    }
}

fn tool_result(call_id: &str, text: &str) -> AgentMessage {
    AgentMessage::ToolResult {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        tool_call_id: Some(call_id.to_string()),
        tool_name: Some("bash".to_string()),
        is_error: None,
    }
}

fn file_call(tool_name: &str, path: &str) -> AgentMessage {
    AgentMessage::Assistant {
        content: vec![ContentBlock::ToolCall {
            id: "call_file".to_string(),
            name: tool_name.to_string(),
            args: json!({"path": path}),
        }],
        stop_reason: None,
        provider_reasoning_replay: None,
    }
}

fn message_entry(message: AgentMessage) -> SessionEntry {
    SessionEntry {
        id: Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(8)
            .collect(),
        timestamp: None,
        entry_type: SessionEntryType::Message(message),
    }
}

fn entries_of(messages: Vec<AgentMessage>) -> Vec<SessionEntry> {
    messages.into_iter().map(message_entry).collect()
}

fn budget(window: u64, keep_recent: u64) -> CompactionBudget {
    CompactionBudget {
        context_window: window,
        reserve_tokens: window / 10,
        retain_ratio: keep_recent as f64 / window as f64,
    }
}

/// 记录请求并提供固定文本的 mock provider。
#[derive(Clone)]
struct MockProvider {
    texts: Arc<Mutex<VecDeque<String>>>,
    requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
}

impl MockProvider {
    fn new(texts: Vec<String>) -> Self {
        Self {
            texts: Arc::new(Mutex::new(texts.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Vec<ModelTurnRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Provider for MockProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> std::result::Result<ModelTurnResponse, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        let text = self.texts.lock().unwrap().pop_front().unwrap_or_default();
        Ok(ModelTurnResponse::completed(
            request.request_id.clone(),
            "mock-response",
            text,
        ))
    }
}

fn mock_engine(texts: Vec<String>) -> (CompactionEngine, MockProvider) {
    let mock = MockProvider::new(texts);
    let provider: Arc<dyn Provider + Send + Sync> = Arc::new(mock.clone());
    (CompactionEngine::new(provider), mock)
}

/// 2. find_cut_point：全量切点、toolResult 跟随、split turn、keep 边界、元数据回扫。
#[test]
fn find_cut_point_full_history_and_tool_result_following() {
    let (engine, _) = mock_engine(vec![]);
    let messages = vec![
        user("aaaaaaaaaa"),              // 0 user（切点）
        assistant("bbbbbbbbbb"),         // 1 assistant（切点）
        tool_result("c1", "cccccccccc"), // 2 toolResult（非切点）
        user("dddddddddd"),              // 3 user（切点）
        assistant("eeeeeeeeee"),         // 4 assistant（切点）
        tool_result("c2", "ffffffffff"), // 5 toolResult（非切点）
    ];
    let entries = entries_of(messages);

    // 预算足够大：不跨阈值 → 默认保留从第一个消息起（cutPoints[0]）。
    assert_eq!(
        engine.find_cut_point(&entries, &budget(100_000, 100_000)),
        Some(0)
    );

    // 预算 10 token：回走累积在 t0（index 2）跨过 → 切点跳到其后最近的合法切点 u1（index 3）。
    // 切点绝不在 toolResult 上。
    assert_eq!(
        engine.find_cut_point(&entries, &budget(100_000, 10)),
        Some(3)
    );

    // 空条目无切点。
    assert_eq!(engine.find_cut_point(&[], &budget(100_000, 10)), None);
}

/// 5. compact 全流程（mock provider）：触发、摘要、append_compaction 落盘、
///    first_kept_entry_id 正确、重开后 build_context_entries 切片正确。
#[test]
fn compact_full_flow_and_reopen_slicing() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), dir.path()).unwrap();
    // 每条 7000 字符 ≈ 1750 token；keep=4000 时切点落在 u1（index 3）。
    let id_u0 = session.append_message(user(&"u".repeat(7000))).unwrap();
    session
        .append_message(file_call("read", "src/main.rs"))
        .unwrap();
    let id_t0 = session
        .append_message(tool_result("c1", &"t".repeat(7000)))
        .unwrap();
    let id_u1 = session.append_message(user(&"d".repeat(7000))).unwrap();
    let id_a1 = session
        .append_message(assistant(&"e".repeat(7000)))
        .unwrap();
    let id_t1 = session
        .append_message(tool_result("c2", &"f".repeat(7000)))
        .unwrap();

    let (mut engine, mock) = mock_engine(vec!["## Goal\nsummary of history".to_string()]);
    let budget = budget(100_000, 4_000);
    let outcome = engine
        .compact(&mut session, &budget, 90_001, &CancellationToken::new())
        .unwrap();
    assert_eq!(
        outcome,
        CompactionOutcome::Compacted {
            first_kept_entry_id: id_u1.clone(),
            tokens_before: 90_001,
            usage: ModelUsage::default(),
            usage_complete: false,
        }
    );

    // 未触发 → 阈值判定在调用方：低于阈值不发起压缩。
    assert!(!engine.should_compact(10_000, &budget));

    // 摘要请求：1 次，developer + user prompt（ProviderProtocolContract 默认支持
    // developer），含 <conversation> 与初始 prompt，无 previous。
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    let roles: Vec<ModelRole> = requests[0]
        .messages
        .iter()
        .map(|m| m.role.clone())
        .collect();
    assert_eq!(roles, vec![ModelRole::Developer, ModelRole::User]);
    let prompt = &requests[0].messages[1].content;
    assert!(prompt.contains("<conversation>\n"));
    assert!(prompt.contains("Use this EXACT format:"));
    assert!(!prompt.contains("<previous-summary>"));
    assert!(prompt.contains("[User]: "));
    assert!(prompt.contains(r#"[Assistant tool calls]: read(path="src/main.rs")"#));

    // 磁盘 compaction 条目：summary 含文件操作块；details 记录累积文件列表。
    let content = std::fs::read_to_string(session.path()).unwrap();
    let last: Value = serde_json::from_str(content.lines().last().unwrap()).unwrap();
    assert_eq!(last["type"], "compaction");
    assert_eq!(last["firstKeptEntryId"], id_u1);
    assert_eq!(last["tokensBefore"], 90_001);
    let summary = last["summary"].as_str().unwrap();
    assert!(summary.starts_with("## Goal"));
    assert!(summary.ends_with("\n\n<read-files>\nsrc/main.rs\n</read-files>"));
    assert_eq!(last["details"]["readFiles"], json!(["src/main.rs"]));
    assert_eq!(last["details"]["modifiedFiles"], json!([]));

    // 重开视角：上下文 = [compaction, 从 firstKeptEntryId 起的保留条目]，
    // 旧消息被摘要取代。写者自身即最新持久事实，直接以它投影。
    let ctx = session.build_context_entries().unwrap();
    let ctx_ids: Vec<&str> = ctx.iter().map(|entry| entry.id.as_str()).collect();
    assert_eq!(ctx_ids.len(), 4);
    assert!(matches!(ctx[0].entry_type, SessionEntryType::Compaction(_)));
    assert_eq!(
        ctx_ids,
        vec![
            ctx[0].id.as_str(),
            id_u1.as_str(),
            id_a1.as_str(),
            id_t1.as_str()
        ]
    );
    assert!(!ctx_ids.contains(&id_u0.as_str()));
    assert!(!ctx_ids.contains(&id_t0.as_str()));

    // 二次压缩：起点 = 上次 first_kept_entry_id，旧摘要传入 UPDATE 合并。
    let id_u2 = session.append_message(user(&"g".repeat(7000))).unwrap();
    let id_a2 = session
        .append_message(assistant(&"h".repeat(7000)))
        .unwrap();
    let id_t2 = session
        .append_message(tool_result("c3", &"i".repeat(7000)))
        .unwrap();
    let (mut engine, mock) = mock_engine(vec!["## Goal\nupdated summary".to_string()]);
    let outcome = engine
        .compact(&mut session, &budget, 90_001, &CancellationToken::new())
        .unwrap();
    assert_eq!(
        outcome,
        CompactionOutcome::Compacted {
            first_kept_entry_id: id_u2.clone(),
            tokens_before: 90_001,
            usage: ModelUsage::default(),
            usage_complete: false,
        }
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    let prompt = &requests[0].messages[1].content;
    // UPDATE prompt 使用上次压缩的完整 summary 文本（含文件操作块）。
    let previous_summary =
        "## Goal\nsummary of history\n\n<read-files>\nsrc/main.rs\n</read-files>";
    assert!(prompt.contains(&format!(
        "<previous-summary>\n{previous_summary}\n</previous-summary>"
    )));
    assert!(prompt.contains("PRESERVE all existing information"));
    let content = std::fs::read_to_string(session.path()).unwrap();
    let last: Value = serde_json::from_str(content.lines().last().unwrap()).unwrap();
    assert_eq!(last["firstKeptEntryId"], id_u2);
    // 文件列表从历史 details 累积。
    assert_eq!(last["details"]["readFiles"], json!(["src/main.rs"]));
    // 二次压缩后的重开视角：写者自身即最新持久事实，直接以它投影。
    let ctx = session.build_context_entries().unwrap();
    let ctx_ids: Vec<&str> = ctx.iter().map(|entry| entry.id.as_str()).collect();
    assert_eq!(
        ctx_ids,
        vec![
            ctx[0].id.as_str(),
            id_u2.as_str(),
            id_a2.as_str(),
            id_t2.as_str()
        ]
    );
}

/// 5c. compact 在无可摘要内容时返回 NotNeeded（全部保留路径）。
#[test]
fn compact_nothing_to_summarize() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), dir.path()).unwrap();
    session.append_message(user("hi")).unwrap();
    let (mut engine, mock) = mock_engine(vec![]);
    // 触发条件满足但 keep 预算极大 → 切点在起点 → 无可摘要内容。
    let cfg = budget(100_000, 1_000_000);
    assert_eq!(
        engine
            .compact(&mut session, &cfg, 90_000, &CancellationToken::new())
            .unwrap(),
        CompactionOutcome::NotNeeded
    );
    assert!(mock.requests().is_empty(), "不应发起摘要调用");
    // 再次 compact：仍然没有新内容，不产生 compaction 条目。
    let _ = engine
        .compact(&mut session, &cfg, 90_000, &CancellationToken::new())
        .unwrap();
    let content = std::fs::read_to_string(session.path()).unwrap();
    let lines: Vec<&str> = content.lines().skip(1).collect();
    assert_eq!(lines.len(), 1, "只有一条消息，无 compaction 条目");
}

/// 5f. 有更早历史且当前轮尾部 ToolResult 跨过保留预算：切点回退到当前轮起点，
///     摘要仅含更早历史；当前轮完整保留且 ToolCall/ToolResult 成对。
#[test]
fn compact_falls_back_to_turn_start_when_tail_tool_result_crosses_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), dir.path()).unwrap();
    let id_u0 = session.append_message(user(&"u".repeat(400))).unwrap(); // 更早历史 turn
    session
        .append_message(file_call("read", "old.txt"))
        .unwrap();
    session
        .append_message(tool_result("c1", &"t".repeat(400)))
        .unwrap();
    let id_u1 = session.append_message(user(&"d".repeat(400))).unwrap(); // 当前轮起点
    let id_a1 = session
        .append_message(file_call("read", "new.txt"))
        .unwrap();
    let id_t1 = session
        .append_message(tool_result("c2", &"f".repeat(4000)))
        .unwrap(); // 尾部跨预算

    let (mut engine, mock) = mock_engine(vec!["## Goal\nearlier history".to_string()]);
    // keep=250：回走累积在当前轮尾部 ToolResult（1000 token）跨过预算，
    // 其后无合法切点 → 回退到当前轮起点 u1。
    let outcome = engine
        .compact(
            &mut session,
            &budget(100_000, 250),
            90_001,
            &CancellationToken::new(),
        )
        .unwrap();
    let CompactionOutcome::Compacted {
        first_kept_entry_id,
        ..
    } = &outcome
    else {
        panic!("expected Compacted, got {outcome:?}");
    };
    assert_eq!(first_kept_entry_id, &id_u1, "切点应为当前轮起点");

    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    let prompt = &requests[0].messages[1].content;
    assert!(prompt.contains("[User]: uuuu"), "摘要应包含更早历史");
    assert!(!prompt.contains("dddd"), "摘要不得包含当前轮内容");
    assert!(!prompt.contains("ffff"), "摘要不得包含当前轮工具结果");

    // 重开视角：上下文 = [compaction, 当前轮全部消息]，ToolCall 与 ToolResult
    // 成对保留。写者自身即最新持久事实，直接以它投影。
    let ctx = session.build_context_entries().unwrap();
    let ctx_ids: Vec<&str> = ctx.iter().map(|entry| entry.id.as_str()).collect();
    assert_eq!(ctx_ids.len(), 4);
    assert!(matches!(ctx[0].entry_type, SessionEntryType::Compaction(_)));
    assert_eq!(
        ctx_ids,
        vec![
            ctx[0].id.as_str(),
            id_u1.as_str(),
            id_a1.as_str(),
            id_t1.as_str()
        ]
    );
    assert!(!ctx_ids.contains(&id_u0.as_str()));
}

/// 6. 摘要 Prompt 常量结构完整性校验（段落与顺序检查）。
#[test]
fn summarization_prompts_match_expected_structure() {
    let sections = [
        "## Goal",
        "## Constraints & Preferences",
        "## Progress",
        "### Done",
        "### In Progress",
        "### Blocked",
        "## Key Decisions",
        "## Next Steps",
        "## Critical Context",
    ];
    for section in sections {
        assert!(SUMMARIZATION_PROMPT.contains(section), "缺少段落 {section}");
    }
    assert!(SUMMARIZATION_PROMPT.contains("Use this EXACT format:"));
    assert!(
        SUMMARIZATION_PROMPT
            .contains("Preserve exact file paths, function names, and error messages.")
    );
    // 校验段落顺序一致性。
    let positions: Vec<usize> = [
        "## Goal",
        "## Progress",
        "## Key Decisions",
        "## Next Steps",
        "## Critical Context",
    ]
    .iter()
    .map(|section| SUMMARIZATION_PROMPT.find(section).unwrap())
    .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));

    assert!(UPDATE_SUMMARIZATION_PROMPT.contains("PRESERVE all existing information"));
    assert!(UPDATE_SUMMARIZATION_PROMPT.contains("<previous-summary>"));
    assert!(UPDATE_SUMMARIZATION_PROMPT.contains("move items from \"In Progress\" to \"Done\""));
    assert!(UPDATE_SUMMARIZATION_PROMPT.contains("## Critical Context"));

    assert!(TURN_PREFIX_SUMMARIZATION_PROMPT.contains("## Original Request"));
    assert!(TURN_PREFIX_SUMMARIZATION_PROMPT.contains("## Early Progress"));
    assert!(TURN_PREFIX_SUMMARIZATION_PROMPT.contains("## Context for Suffix"));

    assert!(SUMMARIZATION_SYSTEM_PROMPT.contains("Do NOT continue the conversation."));
    assert!(SUMMARIZATION_SYSTEM_PROMPT.contains("ONLY output the structured summary."));
}
