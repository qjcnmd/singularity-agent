use super::*;
use crate::session::SessionMetadata;
use singularity_model::{ModelTurnResponse, ProviderProtocolContract};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

fn user(text: &str) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::User,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        provider_reasoning_replay: None,
        tool_call_id: None,
        tool_name: None,
        is_error: None,
        timestamp: None,
    }
}

fn assistant(text: &str) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::Assistant,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        provider_reasoning_replay: None,
        tool_call_id: None,
        tool_name: None,
        is_error: None,
        timestamp: None,
    }
}

fn tool_result(call_id: &str, text: &str) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::ToolResult,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        provider_reasoning_replay: None,
        tool_call_id: Some(call_id.to_string()),
        tool_name: Some("bash".to_string()),
        is_error: None,
        timestamp: None,
    }
}

fn file_call(tool_name: &str, path: &str) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::Assistant,
        content: vec![ContentBlock::ToolCall {
            id: "call_file".to_string(),
            name: tool_name.to_string(),
            args: json!({"path": path}),
        }],
        provider_reasoning_replay: None,
        tool_call_id: None,
        tool_name: None,
        is_error: None,
        timestamp: None,
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
        parent_id: String::new(),
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
        reserve_tokens: DEFAULT_RESERVE_TOKENS,
        keep_recent_tokens: keep_recent,
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

/// 1. should_compact 阈值边界：刚好低于/等于/超过。
#[test]
fn should_compact_threshold_boundaries() {
    let (engine, _) = mock_engine(vec![]);
    let budget = budget(100_000, 20_000);
    // 阈值 = 100_000 - 16_384 = 83_616。
    assert!(!engine.should_compact(0, &budget));
    assert!(!engine.should_compact(83_615, &budget));
    assert!(!engine.should_compact(83_616, &budget), "等于阈值不触发");
    assert!(engine.should_compact(83_617, &budget), "超过阈值触发");
    assert!(engine.should_compact(100_000, &budget));
    // context_window < reserve_tokens：阈值饱和为 0。
    let tiny = CompactionBudget {
        context_window: 100,
        reserve_tokens: 16_384,
        keep_recent_tokens: 20_000,
    };
    assert!(engine.should_compact(1, &tiny));
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

#[test]
fn find_cut_point_split_turn_and_keep_boundary() {
    let (engine, _) = mock_engine(vec![]);
    // 单个超大 turn：u0/a0/t0/a1/t1 各 400 字符（100 token）。
    let messages = vec![
        user(&"u".repeat(400)),
        assistant(&"a".repeat(400)),
        tool_result("c1", &"t".repeat(400)),
        assistant(&"b".repeat(400)),
        tool_result("c2", &"q".repeat(400)),
    ];
    let entries = entries_of(messages);

    // keep=250：跨过点落在 t0（index 2）→ 切在 a1（index 3），切点位于 assistant →
    // split turn，turn 起始为 u0（index 0）。
    let cut = engine.find_cut_point_in_range(&entries, 0, entries.len(), 250);
    assert_eq!(cut.first_kept_entry_index, 3);
    assert_eq!(cut.turn_start_index, Some(0));
    assert!(cut.is_split_turn);

    // keep=100：跨过点在最新 toolResult → 无合法切点可跳 → 全部保留（cutPoints[0]）。
    let cut = engine.find_cut_point_in_range(&entries, 0, entries.len(), 100);
    assert_eq!(cut.first_kept_entry_index, 0);
    assert!(!cut.is_split_turn);

    // keep 边界：恰好等于累积值（>= 语义）与多 1 token 的差异。
    let messages = vec![
        user(&"u".repeat(400)),
        assistant(&"a".repeat(400)),
        tool_result("c1", &"t".repeat(400)),
        user(&"d".repeat(400)),
        assistant(&"e".repeat(400)),
        tool_result("c2", &"f".repeat(400)),
    ];
    let entries = entries_of(messages);
    // 回走：t1=100,a1=200,u1=300 ≥ 300 → 切在 u1（index 3）。
    assert_eq!(
        engine
            .find_cut_point_in_range(&entries, 0, entries.len(), 300)
            .first_kept_entry_index,
        3
    );
    // 400：跨过点恰好等于累积值（>= 语义）→ 切在 u1（index 3）；
    // 401：多 1 token 时跨过点落在 t0 → 切在 a0（index 1）。
    assert_eq!(
        engine
            .find_cut_point_in_range(&entries, 0, entries.len(), 400)
            .first_kept_entry_index,
        3
    );
    assert_eq!(
        engine
            .find_cut_point_in_range(&entries, 0, entries.len(), 401)
            .first_kept_entry_index,
        1
    );
}

#[test]
fn find_cut_point_metadata_scan() {
    let (engine, _) = mock_engine(vec![]);
    // thread_settings 元数据无上下文消息：切点从 u0（index 1）回扫吸收该元数据条目。
    let thread_settings = SessionEntry {
        id: "m0000001".to_string(),
        parent_id: String::new(),
        timestamp: None,
        entry_type: SessionEntryType::Metadata(
            SessionMetadata::thread_settings("openai", "gpt-4o", None).unwrap(),
        ),
    };
    let mut entries = vec![thread_settings];
    entries.extend(entries_of(vec![
        user("aaaaaaaaaa"),
        assistant("bbbbbbbbbb"),
        tool_result("c1", "cccccccccc"),
    ]));
    assert_eq!(
        engine.find_cut_point(&entries, &budget(100_000, 100_000)),
        Some(0),
        "切点应回扫到元数据条目（firstKeptEntryId 指向它）"
    );
}

/// 3. estimate_tokens：空串/英文/中文/长文本边界。
#[test]
fn estimate_tokens_boundaries() {
    let (engine, _) = mock_engine(vec![]);
    assert_eq!(engine.estimate_tokens(""), 0);
    assert_eq!(engine.estimate_tokens("a"), 1);
    assert_eq!(engine.estimate_tokens("abcd"), 1);
    assert_eq!(engine.estimate_tokens("abcde"), 2);
    assert_eq!(engine.estimate_tokens("中文测试"), 1); // 4 字符 → ceil(4/4)
    assert_eq!(engine.estimate_tokens("中文测试一"), 2); // 5 字符 → ceil(5/4)
    assert_eq!(engine.estimate_tokens(&"x".repeat(8000)), 2000);
}

/// 4. serialize_conversation：role 标注、tool result 截断、tool call 序列化。
#[test]
fn serialize_conversation_roles_and_truncation() {
    let (engine, _) = mock_engine(vec![]);
    let long_output = "x".repeat(2500);
    let messages = vec![
        user("hello"),
        assistant("hi"),
        file_call("read", "src/main.rs"),
        tool_result("c1", &long_output),
        AgentMessage {
            role: AgentMessageRole::User,
            content: vec![ContentBlock::Text {
                text: "ran a command".to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        },
        AgentMessage {
            role: AgentMessageRole::User,
            content: vec![ContentBlock::Text {
                text: "earlier summary".to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        },
        user(""),
    ];
    let text = engine.serialize_conversation(&messages);
    assert!(text.contains("[User]: hello"));
    assert!(text.contains("[Assistant]: hi"));
    assert!(text.contains(r#"[Assistant tool calls]: read(path="src/main.rs")"#));
    assert!(text.contains("[Tool result]: "));
    assert!(text.contains("[User]: ran a command"));
    assert!(text.contains("[User]: earlier summary"));
    assert!(!text.contains("[User]: \n"), "空 user 内容应跳过");
    // 截断：保留开头 2000 字符并附精确截断标记（2500 - 2000 = 500）。
    assert!(text.contains(&"x".repeat(2000)));
    assert!(text.contains("[... 500 more characters truncated]"));
    assert!(!text.contains("xxx[..."));
}

/// 4b. 文件操作提取与格式化（read/write/edit、read-only 排除已修改文件、排序）。
#[test]
fn file_ops_extraction_and_formatting() {
    let mut ops = FileOps::default();
    extract_file_ops_from_message(&file_call("read", "z.txt"), &mut ops);
    extract_file_ops_from_message(&file_call("read", "a.txt"), &mut ops);
    extract_file_ops_from_message(&file_call("edit", "a.txt"), &mut ops);
    extract_file_ops_from_message(&file_call("write", "b.txt"), &mut ops);
    // 非 assistant / 无 path 的调用不提取。
    extract_file_ops_from_message(&user("hello"), &mut ops);
    let (read_files, modified_files) = compute_file_lists(&ops);
    // a.txt 被 edit → 属于 modified，不出现在 readFiles。
    assert_eq!(read_files, vec!["z.txt".to_string()]);
    assert_eq!(
        modified_files,
        vec!["a.txt".to_string(), "b.txt".to_string()]
    );
    let formatted = format_file_operations(&read_files, &modified_files);
    assert_eq!(
        formatted,
        "\n\n<read-files>\nz.txt\n</read-files>\n\n<modified-files>\na.txt\nb.txt\n</modified-files>"
    );
    // 历史 details 累积：readFiles/modifiedFiles 数组并入集合。
    let mut ops = FileOps::default();
    for file in ["old.txt", "z.txt"] {
        ops.read.insert(file.to_string());
    }
    ops.edited.insert("a.txt".to_string());
    let (read_files, modified_files) = compute_file_lists(&ops);
    let _ = (read_files, modified_files);
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
        .compact(&mut session, &budget, 90_000, &CancellationToken::new())
        .unwrap();
    assert_eq!(
        outcome,
        CompactionOutcome::Compacted {
            first_kept_entry_id: id_u1.clone(),
            tokens_before: 90_000,
        }
    );

    // 未触发 → NotNeeded。
    assert_eq!(
        engine
            .compact(&mut session, &budget, 10_000, &CancellationToken::new())
            .unwrap(),
        CompactionOutcome::NotNeeded
    );

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
    assert_eq!(last["tokensBefore"], 90_000);
    assert!(last.get("previousSummary").is_none());
    let summary = last["summary"].as_str().unwrap();
    assert!(summary.starts_with("## Goal"));
    assert!(summary.ends_with("\n\n<read-files>\nsrc/main.rs\n</read-files>"));
    assert_eq!(last["details"]["readFiles"], json!(["src/main.rs"]));
    assert_eq!(last["details"]["modifiedFiles"], json!([]));

    // 重开：上下文 = [compaction, 从 firstKeptEntryId 起的保留条目]，旧消息被摘要取代。
    let reopened = SessionManager::open(session.path()).unwrap();
    let ctx = reopened.build_context_entries().unwrap();
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

    // 二次压缩：起点 = 上次 first_kept_entry_id，previousSummary 传入 UPDATE 合并。
    let id_u2 = session.append_message(user(&"g".repeat(7000))).unwrap();
    let id_a2 = session
        .append_message(assistant(&"h".repeat(7000)))
        .unwrap();
    let id_t2 = session
        .append_message(tool_result("c3", &"i".repeat(7000)))
        .unwrap();
    let (mut engine, mock) = mock_engine(vec!["## Goal\nupdated summary".to_string()]);
    let outcome = engine
        .compact(&mut session, &budget, 90_000, &CancellationToken::new())
        .unwrap();
    assert_eq!(
        outcome,
        CompactionOutcome::Compacted {
            first_kept_entry_id: id_u2.clone(),
            tokens_before: 90_000,
        }
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    let prompt = &requests[0].messages[1].content;
    // previousSummary 为上次压缩的完整 summary 文本（含文件操作块）。
    let previous_summary =
        "## Goal\nsummary of history\n\n<read-files>\nsrc/main.rs\n</read-files>";
    assert!(prompt.contains(&format!(
        "<previous-summary>\n{previous_summary}\n</previous-summary>"
    )));
    assert!(prompt.contains("PRESERVE all existing information"));
    let content = std::fs::read_to_string(session.path()).unwrap();
    let last: Value = serde_json::from_str(content.lines().last().unwrap()).unwrap();
    assert_eq!(last["firstKeptEntryId"], id_u2);
    assert_eq!(last["previousSummary"], previous_summary);
    // 文件列表从历史 details 累积。
    assert_eq!(last["details"]["readFiles"], json!(["src/main.rs"]));
    let reopened = SessionManager::open(session.path()).unwrap();
    let ctx = reopened.build_context_entries().unwrap();
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

/// 5b. split turn：历史摘要 + turn 前缀摘要两次调用，合并为完整结构。
#[test]
fn compact_split_turn_merges_two_summaries() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), dir.path()).unwrap();
    // u0/a0/t0 为完整历史 turn；u1/a1/t1 为超大 turn。
    session.append_message(user(&"u".repeat(7000))).unwrap();
    session
        .append_message(file_call("read", "split.txt"))
        .unwrap();
    session
        .append_message(tool_result("c1", &"t".repeat(7000)))
        .unwrap();
    let id_u1 = session.append_message(user(&"d".repeat(10_000))).unwrap();
    let id_a1 = session
        .append_message(assistant(&"e".repeat(10_000)))
        .unwrap();
    let id_t1 = session
        .append_message(tool_result("c2", &"f".repeat(10_000)))
        .unwrap();

    // keep=2600：跨过点在 a1 → 切在 a1（index 4）→ split；历史 = u0/a0/t0。
    let (mut engine, mock) = mock_engine(vec![
        "## Goal\nhistory".to_string(),
        "## Original Request\nprefix".to_string(),
    ]);
    let budget = budget(100_000, 2_600);
    let outcome = engine
        .compact(&mut session, &budget, 90_000, &CancellationToken::new())
        .unwrap();
    assert_eq!(
        outcome,
        CompactionOutcome::Compacted {
            first_kept_entry_id: id_a1.clone(),
            tokens_before: 90_000,
        }
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "历史与 turn 前缀各一次摘要调用");
    assert!(
        requests[0].messages[1]
            .content
            .contains("Use this EXACT format:")
    );
    assert!(
        requests[1].messages[1]
            .content
            .contains("## Original Request")
    );
    let content = std::fs::read_to_string(session.path()).unwrap();
    let last: Value = serde_json::from_str(content.lines().last().unwrap()).unwrap();
    let summary = last["summary"].as_str().unwrap();
    assert!(summary.starts_with("## Goal\nhistory"));
    assert!(
        summary
            .contains("\n\n---\n\n**Turn Context (split turn):**\n\n## Original Request\nprefix")
    );
    assert!(summary.ends_with("\n\n<read-files>\nsplit.txt\n</read-files>"));
    assert_eq!(last["details"]["readFiles"], json!(["split.txt"]));

    let reopened = SessionManager::open(session.path()).unwrap();
    let ctx = reopened.build_context_entries().unwrap();
    let ctx_ids: Vec<&str> = ctx.iter().map(|entry| entry.id.as_str()).collect();
    assert_eq!(ctx_ids.len(), 3);
    assert_eq!(
        ctx_ids,
        vec![ctx[0].id.as_str(), id_a1.as_str(), id_t1.as_str()]
    );
    assert!(!ctx_ids.contains(&id_u1.as_str()));
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
