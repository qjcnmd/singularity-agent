use super::*;
use singularity_model::{ModelRole, ProviderReasoningReplay};

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
        timestamp: Some(1_700_000_000_000),
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
        timestamp: Some(1_700_000_000_001),
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
        timestamp: Some(1_700_000_000_002),
    }
}

fn compaction(summary: &str, first_kept_entry_id: Option<String>) -> CompactionEntry {
    CompactionEntry {
        summary: summary.to_string(),
        first_kept_entry_id,
        tokens_before: Some(100),
        previous_summary: None,
        details: None,
    }
}

fn entry_ids(entries: &[SessionEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.id.clone()).collect()
}

/// 1. create → 追加 3 条消息 → 重开 open → 条目一致、leaf 一致。
#[test]
fn create_append_reopen_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    // cwd 无需存在（Pi resolvePath 不要求路径存在）。
    let cwd = dir.path().join("project");
    let mut manager = SessionManager::create(&cwd, &sessions).unwrap();
    assert!(manager.leaf_id().is_empty());

    let id1 = manager.append_message(user("hello")).unwrap();
    let id2 = manager.append_message(assistant("hi there")).unwrap();
    let id3 = manager
        .append_message(tool_result("call_1", "ls output"))
        .unwrap();
    let file = manager.path().to_path_buf();
    let leaf = manager.leaf_id().to_string();
    assert_eq!(leaf, id3);

    // header 已写入；文件名 <ts>_<uuid>.jsonl 且 uuid 段 = header id。
    let content = std::fs::read_to_string(&file).unwrap();
    let first_line: Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert_eq!(first_line["type"], "session");
    assert_eq!(first_line["version"], 4);
    assert_eq!(
        first_line["cwd"],
        normalize_cwd_string(&std::path::absolute(&cwd).unwrap())
    );
    let file_name = manager.path().file_name().unwrap().to_str().unwrap();
    assert!(file_name.ends_with(".jsonl"));
    let header_ts = first_line["timestamp"]
        .as_str()
        .unwrap()
        .replace([':', '.'], "-");
    assert_eq!(file_name.rsplit_once('_').unwrap().0, header_ts);
    let header_id = first_line["id"].as_str().unwrap();
    assert!(file_name.ends_with(&format!("_{header_id}.jsonl")));
    drop(manager);

    let opened = SessionManager::open(&file).unwrap();
    assert_eq!(opened.leaf_id(), leaf);
    let entries = opened.build_context_entries().unwrap();
    assert_eq!(entry_ids(&entries), vec![id1, id2, id3]);
    // Pi entry id 为 8 位十六进制。
    for entry in &entries {
        assert_eq!(entry.id.len(), 8);
        assert!(entry.id.chars().all(|c| c.is_ascii_hexdigit()));
    }
    assert!(matches!(&entries[0].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::User && m.content_text() == "hello"));
    assert!(matches!(&entries[1].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::Assistant && m.content_text() == "hi there"));
    assert!(matches!(&entries[2].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::ToolResult
                && m.tool_call_id.as_deref() == Some("call_1")
                && m.tool_name.as_deref() == Some("bash")));
}

/// 1b. create_with_name：确定性文件名，追加/重开语义与 create 一致。
#[test]
fn create_with_name_uses_deterministic_file_name() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let cwd = dir.path().join("project");
    let mut manager = SessionManager::create_with_name(&cwd, &sessions, "thread_abc").unwrap();
    assert_eq!(manager.path().file_name().unwrap(), "thread_abc.jsonl");
    assert!(manager.leaf_id().is_empty());

    let id1 = manager.append_message(user("hello")).unwrap();
    let file = manager.path().to_path_buf();
    drop(manager);

    let opened = SessionManager::open(&file).unwrap();
    assert_eq!(opened.leaf_id(), id1);
    let entries = opened.build_context_entries().unwrap();
    assert_eq!(entry_ids(&entries), vec![id1]);
    // header cwd 与 create 语义一致（绝对路径）。
    let content = std::fs::read_to_string(&file).unwrap();
    let first_line: Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert_eq!(first_line["type"], "session");
    assert_eq!(
        first_line["cwd"],
        normalize_cwd_string(&std::path::absolute(&cwd).unwrap())
    );
}

/// 1c. 已存在但为空的 session 文件 fail closed：不静默重写为新随机 UUID。
#[test]
fn empty_existing_session_file_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("empty.jsonl");
    std::fs::write(&file, b"").unwrap();

    let error = SessionManager::open(&file)
        .err()
        .expect("empty session file must fail closed");
    assert!(matches!(error, SessionError::InvalidSession(_)));
    assert!(error.to_string().contains("empty"));
    // 文件保持原样，未被重写为 header（身份不得静默丢失）。
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "");

    // 缺失文件仍按 Pi 语义创建新会话（不回归）。
    let missing = dir.path().join("missing.jsonl");
    let created = SessionManager::open(&missing).unwrap();
    assert!(created.path().is_file());
}

/// 2. 追加顺序：每条 parent = 前一条 id，首条为根（磁盘上 parentId 为 null）。
#[test]
fn append_chain_parent_ids() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
    let id1 = manager.append_message(user("a")).unwrap();
    let id2 = manager.append_message(user("b")).unwrap();
    let id3 = manager.append_message(assistant("c")).unwrap();

    let entries = manager.build_context_entries().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].parent_id, "");
    assert_eq!(entries[1].parent_id, id1);
    assert_eq!(entries[2].parent_id, id2);
    assert_eq!(manager.leaf_id(), id3);

    let content = std::fs::read_to_string(manager.path()).unwrap();
    let lines: Vec<Value> = content
        .lines()
        .skip(1)
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines[0]["parentId"], Value::Null);
    assert_eq!(lines[1]["parentId"], id1);
    assert_eq!(lines[2]["parentId"], id2);
}

/// 3. branch：leaf 变化；再追加挂在分支下；原路径不受影响；未知 id 报错。
#[test]
fn branch_moves_leaf_and_keeps_original_path() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
    let id1 = manager.append_message(user("first")).unwrap();
    let id2 = manager.append_message(user("second")).unwrap();
    let _id3 = manager.append_message(user("third")).unwrap();

    manager.branch(&id2).unwrap();
    assert_eq!(manager.leaf_id(), id2);
    let id4 = manager.append_message(user("branched")).unwrap();
    assert_eq!(manager.leaf_id(), id4);

    let entries = manager.build_context_entries().unwrap();
    assert_eq!(entry_ids(&entries), vec![id1, id2, id4]);

    assert!(matches!(
        manager.branch("deadbeef"),
        Err(SessionError::EntryNotFound(_))
    ));
}

/// 4. 迁移：手工构造 v1 与 v2 样例 → open → 内容与 v3 语义一致，文件被重写。
#[test]
fn open_migrates_v1_file_to_v4() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("v1.jsonl");
    let lines = [
        r#"{"type":"session","version":1,"id":"v1-session","timestamp":"2024-01-01T00:00:00.000Z","cwd":"C:/work"}"#,
        r#"{"type":"message","timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":"a"}}"#,
        r#"{"type":"message","timestamp":"2024-01-01T00:00:02.000Z","message":{"role":"assistant","content":"b"}}"#,
        r#"{"type":"compaction","timestamp":"2024-01-01T00:00:03.000Z","summary":"summary of a and b","tokensBefore":100,"firstKeptEntryIndex":1}"#,
        r#"{"type":"message","timestamp":"2024-01-01T00:00:04.000Z","message":{"role":"user","content":"c"}}"#,
        r#"{"type":"label","timestamp":"2024-01-01T00:00:05.000Z","targetId":"t1","label":"checkpoint"}"#,
    ];
    std::fs::write(&file, lines.join("\n")).unwrap();

    let manager = SessionManager::open(&file).unwrap();
    let entries = manager.build_context_entries().unwrap();
    assert_eq!(entries.len(), 5);
    for entry in &entries {
        assert_eq!(entry.id.len(), 8);
        assert!(entry.id.chars().all(|c| c.is_ascii_hexdigit()));
    }
    // 切片顺序 = [compaction] + (firstKeptEntryId 起) + (compaction 之后)：
    // 路径为 msg_a→msg_b→comp→msg_c→label，firstKeptEntryId=msg_a。
    assert!(matches!(
        entries[0].entry_type,
        SessionEntryType::Compaction(_)
    ));
    assert!(matches!(&entries[1].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::User && m.content_text() == "a"));
    assert!(matches!(&entries[2].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::Assistant && m.content_text() == "b"));
    assert!(matches!(&entries[3].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::User && m.content_text() == "c"));
    // parent 链（按原文件顺序）：msg_a 为根；msg_b/comp/msg_c/label 各自挂接。
    assert_eq!(entries[1].parent_id, "");
    assert_eq!(entries[2].parent_id, entries[1].id);
    assert_eq!(entries[0].parent_id, entries[2].id);
    assert_eq!(entries[3].parent_id, entries[0].id);
    assert_eq!(entries[4].parent_id, entries[3].id);
    // firstKeptEntryIndex=1 是含 header（0 位）的原始数组下标 → 第一条消息 "a"。
    let comp = match &entries[0].entry_type {
        SessionEntryType::Compaction(comp) => comp,
        _ => unreachable!(),
    };
    assert_eq!(
        comp.first_kept_entry_id.as_deref(),
        Some(entries[1].id.as_str())
    );
    assert_eq!(comp.tokens_before, Some(100));
    assert_eq!(comp.summary, "summary of a and b");
    // label 条目以 Other 原样往返。
    let label_json: Value = serde_json::to_value(&entries[4]).unwrap();
    assert_eq!(label_json["type"], "label");
    assert_eq!(label_json["label"], "checkpoint");
    assert_eq!(label_json["targetId"], "t1");
    // 切片边界：firstKeptEntryId=msg_a（含边界）→ context 恰好包含全部 5 条。
    let context_ids: Vec<String> = manager
        .build_context_entries()
        .unwrap()
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    assert_eq!(context_ids, entry_ids(&entries));
    // 文件已重写为 v4：header version 4，条目带 id/parentId，索引已转 id。
    let rewritten: Vec<Value> = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect();
    assert_eq!(rewritten.len(), 6);
    assert_eq!(rewritten[0]["version"], 4);
    for entry in &rewritten[1..] {
        assert!(entry.get("id").is_some());
        assert!(entry.get("parentId").is_some());
    }
    let comp_wire = rewritten
        .iter()
        .find(|entry| entry["type"] == "compaction")
        .unwrap();
    assert!(comp_wire.get("firstKeptEntryId").is_some());
    assert!(comp_wire.get("firstKeptEntryIndex").is_none());
    // 重写后重新打开语义一致（迁移幂等）。
    let reopened = SessionManager::open(&file).unwrap();
    assert_eq!(reopened.build_context_entries().unwrap().len(), 5);
}

#[test]
fn open_migrates_v2_hook_message_role_to_v4() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("v2.jsonl");
    let lines = [
        r#"{"type":"session","version":2,"id":"v2-session","timestamp":"2024-01-01T00:00:00.000Z","cwd":"C:/work"}"#,
        r#"{"type":"message","id":"aaaa1111","parentId":null,"timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":"hello"}}"#,
        r#"{"type":"message","id":"bbbb2222","parentId":"aaaa1111","timestamp":"2024-01-01T00:00:02.000Z","message":{"role":"hookMessage","customType":"ext","content":"injected"}}"#,
    ];
    std::fs::write(&file, lines.join("\n")).unwrap();

    let manager = SessionManager::open(&file).unwrap();
    let entries = manager.build_context_entries().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(matches!(&entries[1].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::Custom && m.content_text() == "injected"));
    // 文件已重写为 v4，hookMessage → custom。
    let rewritten: Vec<Value> = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect();
    assert_eq!(rewritten[0]["version"], 4);
    assert_eq!(rewritten[2]["message"]["role"], "custom");
}

/// hookMessage → custom；v3 → v4 内容块化：user/assistant 消息 content 字符串
/// 改写为 text 块数组，assistant 单工具调用字段迁移为 tool_call 块。
#[test]
fn open_migrates_v3_content_blocks_to_v4() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("v3.jsonl");
    let lines = [
        r#"{"type":"session","version":3,"id":"v3-session","timestamp":"2024-01-01T00:00:00.000Z","cwd":"C:/work"}"#,
        r#"{"type":"message","id":"aaaa1111","parentId":null,"timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":"fix it"}}"#,
        r#"{"type":"message","id":"bbbb2222","parentId":"aaaa1111","timestamp":"2024-01-01T00:00:02.000Z","message":{"role":"assistant","content":"calling tool","toolCallId":"c1","toolName":"write","args":{"path":"out.txt","content":"x"}}}"#,
        r#"{"type":"message","id":"cccc3333","parentId":"bbbb2222","timestamp":"2024-01-01T00:00:03.000Z","message":{"role":"toolResult","content":"wrote c1","toolCallId":"c1","toolName":"write"}}"#,
    ];
    std::fs::write(&file, lines.join("\n")).unwrap();

    let manager = SessionManager::open(&file).unwrap();
    let entries = manager.build_context_entries().unwrap();
    assert!(matches!(&entries[0].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::User
                && m.content_text() == "fix it"));
    assert!(matches!(&entries[1].entry_type,
    SessionEntryType::Message(m) if m.role == AgentMessageRole::Assistant
        && m.content_text() == "calling tool"
        && m.tool_calls().len() == 1
        && matches!(
            m.tool_calls()[0],
            ContentBlock::ToolCall { id, name, args }
                if id == "c1"
                    && name == "write"
                    && args == &serde_json::json!({"path": "out.txt", "content": "x"})
        )));
    assert!(matches!(&entries[2].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::ToolResult
                && m.tool_call_id.as_deref() == Some("c1")));
    // 文件已重写为 v4：header version 4，content 为数组，toolCallId 已移除。
    let rewritten: Vec<Value> = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect();
    assert_eq!(rewritten[0]["version"], 4);
    let assistant = &rewritten[2]["message"];
    assert!(assistant["content"].is_array());
    assert_eq!(assistant["content"][0]["type"], "text");
    assert_eq!(assistant["content"][1]["type"], "tool_call");
    assert_eq!(assistant["content"][1]["id"], "c1");
    assert_eq!(assistant["content"][1]["name"], "write");
    assert!(assistant.get("toolCallId").is_none());
    assert!(assistant.get("args").is_none());
}

#[test]
fn open_migrates_v3_multi_tool_batch_to_one_assistant_entry() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("v3-multi.jsonl");
    let lines = [
        r#"{"type":"session","version":3,"id":"v3-multi-session","timestamp":"2024-01-01T00:00:00.000Z","cwd":"C:/work"}"#,
        r#"{"type":"message","id":"user0001","parentId":null,"timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":"fix both"}}"#,
        r#"{"type":"message","id":"assist001","parentId":"user0001","timestamp":"2024-01-01T00:00:02.000Z","message":{"role":"assistant","content":"calling both","toolCallId":"call_1","toolName":"write","args":{"path":"a.txt","content":"a"}}}"#,
        r#"{"type":"message","id":"assist002","parentId":"assist001","timestamp":"2024-01-01T00:00:02.001Z","message":{"role":"assistant","content":"","toolCallId":"call_2","toolName":"write","args":{"path":"b.txt","content":"b"}}}"#,
        r#"{"type":"message","id":"result001","parentId":"assist002","timestamp":"2024-01-01T00:00:03.000Z","message":{"role":"toolResult","content":"wrote a","toolCallId":"call_1","toolName":"write"}}"#,
        r#"{"type":"message","id":"result002","parentId":"result001","timestamp":"2024-01-01T00:00:04.000Z","message":{"role":"toolResult","content":"wrote b","toolCallId":"call_2","toolName":"write"}}"#,
    ];
    std::fs::write(&file, lines.join("\n")).unwrap();

    let manager = SessionManager::open(&file).unwrap();
    let entries = manager.build_context_entries().unwrap();
    assert_eq!(entries.len(), 4);
    let assistant = match &entries[1].entry_type {
        SessionEntryType::Message(message) => message,
        other => panic!("expected merged assistant message, got {other:?}"),
    };
    assert_eq!(assistant.role, AgentMessageRole::Assistant);
    assert_eq!(assistant.content_text(), "calling both");
    let calls = assistant.tool_calls();
    assert_eq!(calls.len(), 2);
    assert!(matches!(
        calls[0],
        ContentBlock::ToolCall { id, name, args }
            if id == "call_1"
                && name == "write"
                && args == &serde_json::json!({"path":"a.txt","content":"a"})
    ));
    assert!(matches!(
        calls[1],
        ContentBlock::ToolCall { id, name, args }
            if id == "call_2"
                && name == "write"
                && args == &serde_json::json!({"path":"b.txt","content":"b"})
    ));
    let context = manager.build_session_context().unwrap();
    assert_eq!(context.messages[1].tool_calls.len(), 2);
    assert_eq!(context.messages[2].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(context.messages[3].tool_call_id.as_deref(), Some("call_2"));
    assert!(matches!(
        &entries[2].entry_type,
        SessionEntryType::Message(message)
            if message.role == AgentMessageRole::ToolResult
                && message.tool_call_id.as_deref() == Some("call_1")
    ));
    assert!(matches!(
        &entries[3].entry_type,
        SessionEntryType::Message(message)
            if message.role == AgentMessageRole::ToolResult
                && message.tool_call_id.as_deref() == Some("call_2")
    ));

    let rewritten: Vec<Value> = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect();
    assert_eq!(rewritten.len(), 5);
    assert_eq!(rewritten[0]["version"], 4);
    assert_eq!(
        rewritten[2]["message"]["content"].as_array().unwrap().len(),
        3
    );
    assert_eq!(rewritten[2]["message"]["content"][1]["id"], "call_1");
    assert_eq!(rewritten[2]["message"]["content"][2]["id"], "call_2");
    assert!(rewritten[2]["message"].get("toolCallId").is_none());
    assert_eq!(rewritten[3]["parentId"], "assist001");
}

#[test]
fn open_rejects_ambiguous_v3_multi_tool_batch_without_rewriting() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("v3-ambiguous.jsonl");
    let lines = [
        r#"{"type":"session","version":3,"id":"v3-ambiguous-session","timestamp":"2024-01-01T00:00:00.000Z","cwd":"C:/work"}"#,
        r#"{"type":"message","id":"user0001","parentId":null,"timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":"fix both"}}"#,
        r#"{"type":"message","id":"assist001","parentId":"user0001","timestamp":"2024-01-01T00:00:02.000Z","message":{"role":"assistant","content":"calling both","toolCallId":"call_1","toolName":"write","args":{"path":"a.txt","content":"a"}}}"#,
        r#"{"type":"message","id":"assist002","parentId":"assist001","timestamp":"2024-01-01T00:00:02.001Z","message":{"role":"assistant","content":"","toolCallId":"call_2","toolName":"write","args":{"path":"b.txt","content":"b"}}}"#,
        r#"{"type":"message","id":"result001","parentId":"assist002","timestamp":"2024-01-01T00:00:03.000Z","message":{"role":"toolResult","content":"wrote unknown","toolCallId":"call_unknown","toolName":"write"}}"#,
    ];
    std::fs::write(&file, lines.join("\n")).unwrap();
    let original = std::fs::read(&file).unwrap();

    let error = match SessionManager::open(&file) {
        Ok(_) => panic!("ambiguous v3 batch must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(error, SessionError::InvalidStructure(_)));
    assert_eq!(std::fs::read(&file).unwrap(), original);
}

/// 7. assistant 带 tool call 的消息投影为带 tool_calls 的 LLM 消息（Phase 2d 扩展，
///    对齐 Pi convertToLlm：assistant 消息携带 tool call 结构进入上下文）。
#[test]
fn build_session_context_replays_assistant_tool_calls() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
    manager
        .append_message(AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: "call_1".to_string(),
                name: "write".to_string(),
                args: serde_json::json!({
                    "path": "hello.txt",
                    "content": "hello",
                }),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        })
        .unwrap();
    manager
        .append_message(tool_result(
            "call_1",
            "Successfully wrote 5 bytes to hello.txt",
        ))
        .unwrap();

    // 落盘重开：字段持久化后投影语义一致。
    let file = manager.path().to_path_buf();
    drop(manager);
    let manager = SessionManager::open(&file).unwrap();
    let ctx = manager.build_session_context().unwrap();
    assert_eq!(ctx.messages.len(), 2);
    assert_eq!(ctx.messages[0].role, ModelRole::Assistant);
    assert_eq!(ctx.messages[0].content, "");
    assert_eq!(ctx.messages[0].tool_calls.len(), 1);
    let call = &ctx.messages[0].tool_calls[0];
    assert_eq!(call.tool_call_id, "call_1");
    assert_eq!(call.tool_name, "write");
    assert_eq!(call.parse_status, ModelToolParseStatus::Valid);
    assert!(call.validation_errors.is_empty());
    // raw_arguments 由 args 重序列化，语义等价。
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&call.raw_arguments).unwrap(),
        serde_json::json!({ "path": "hello.txt", "content": "hello" })
    );
    assert_eq!(ctx.messages[1].role, ModelRole::Tool);
    assert_eq!(ctx.messages[1].tool_call_id.as_deref(), Some("call_1"));

    // 无 tool call 的 assistant 消息保持纯文本投影；缺 call id 时退化纯文本。
    let mut plain = SessionManager::create(dir.path(), dir.path()).unwrap();
    plain.append_message(assistant("hi")).unwrap();
    let ctx = plain.build_session_context().unwrap();
    assert_eq!(ctx.messages[0].role, ModelRole::Assistant);
    assert!(ctx.messages[0].tool_calls.is_empty());
    let mut missing_id = SessionManager::create(dir.path(), dir.path()).unwrap();
    missing_id
        .append_message(AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: "text".to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        })
        .unwrap();
    let ctx = missing_id.build_session_context().unwrap();
    assert_eq!(ctx.messages[0].role, ModelRole::Assistant);
    assert!(ctx.messages[0].tool_calls.is_empty());
    assert_eq!(ctx.messages[0].content, "text");
}

/// 5. build_context_entries：无 compaction = 全量；有 compaction = 正确切片。
#[test]
fn build_context_entries_compaction_slicing() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();

    let id_a = manager.append_message(user("a")).unwrap();
    let id_b = manager.append_message(user("b")).unwrap();
    let id_c = manager.append_message(user("c")).unwrap();
    let all = manager.build_context_entries().unwrap();
    assert_eq!(
        entry_ids(&all),
        vec![id_a.clone(), id_b.clone(), id_c.clone()]
    );

    let comp = manager
        .append_compaction(compaction("sum", Some(id_b.clone())))
        .unwrap();
    let id_d = manager.append_message(user("d")).unwrap();
    let ctx = manager.build_context_entries().unwrap();
    assert_eq!(
        entry_ids(&ctx),
        vec![comp.clone(), id_b.clone(), id_c.clone(), id_d.clone()]
    );

    // firstKeptEntryId 边界：= a（含边界本身）。
    let mut other = SessionManager::create(dir.path(), dir.path()).unwrap();
    let a2 = other.append_message(user("a")).unwrap();
    let b2 = other.append_message(user("b")).unwrap();
    let c2 = other.append_message(user("c")).unwrap();
    let comp2 = other
        .append_compaction(compaction("s", Some(a2.clone())))
        .unwrap();
    let d2 = other.append_message(user("d")).unwrap();
    let ctx2 = other.build_context_entries().unwrap();
    assert_eq!(entry_ids(&ctx2), vec![comp2, a2, b2, c2, d2]);
}

/// 6. build_session_context：消息顺序/role 转换正确，compaction 摘要包裹，
///    model/thinking 从条目提取。
#[test]
fn build_session_context_messages_and_settings() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
    manager.append_message(user("hello")).unwrap();
    manager.append_message(assistant("hi")).unwrap();
    manager
        .append_message(tool_result("call_1", "out"))
        .unwrap();

    // 默认：无 model/thinking 条目 → None。
    let ctx = manager.build_session_context().unwrap();
    assert_eq!(ctx.model, None);
    assert_eq!(ctx.thinking_level, None);
    let roles: Vec<ModelRole> = ctx.messages.iter().map(|m| m.role.clone()).collect();
    assert_eq!(
        roles,
        vec![ModelRole::User, ModelRole::Assistant, ModelRole::Tool]
    );
    assert_eq!(ctx.messages[0].content, "hello");
    assert_eq!(ctx.messages[1].content, "hi");
    assert_eq!(ctx.messages[2].content, "out");
    assert_eq!(ctx.messages[2].tool_call_id.as_deref(), Some("call_1"));

    // compaction 条目 → user 文本 + Pi 摘要包裹（firstKept 为 None 时旧条目被摘要取代）。
    manager
        .append_compaction(compaction("earlier stuff", None))
        .unwrap();
    let ctx = manager.build_session_context().unwrap();
    assert_eq!(ctx.messages.len(), 1);
    assert_eq!(ctx.messages[0].role, ModelRole::User);
    assert_eq!(
        ctx.messages[0].content,
        format!("{COMPACTION_SUMMARY_PREFIX}earlier stuff{COMPACTION_SUMMARY_SUFFIX}")
    );
}

/// session/read 仓储入口：只返回摘要 + 最近片段，filter/range 有界。
#[test]
fn repository_read_is_bounded_and_filtered() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let cwd = dir.path().join("project");
    let session_id = "64e9177f-ef7e-42af-910d-bd0b94b99230";
    let mut manager = SessionManager::create_with_id(&cwd, &sessions, session_id).unwrap();
    let id1 = manager.append_message(user("one")).unwrap();
    let id2 = manager.append_message(assistant("two")).unwrap();
    manager
        .append_compaction(compaction("summary", Some(id1.clone())))
        .unwrap();
    let id4 = manager.append_message(user("three")).unwrap();
    drop(manager);

    let repository = SessionRepository::new(&sessions);
    let read = repository
        .read(
            session_id,
            &SessionReadOptions {
                recent_limit: 1,
                ..SessionReadOptions::default()
            },
        )
        .unwrap();
    assert_eq!(read.summary.as_deref(), Some("summary"));
    assert_eq!(read.total_entries, 4);
    assert_eq!(entry_ids(&read.entries), vec![id4.clone()]);

    let messages = repository
        .read(
            session_id,
            &SessionReadOptions {
                filter: SessionEntryFilter::Messages,
                range: Some((1, 3)),
                recent_limit: 10,
            },
        )
        .unwrap();
    assert_eq!(entry_ids(&messages.entries), vec![id2.clone(), id4.clone()]);
}

#[test]
fn repair_orphaned_tool_calls_appends_synthetic_failed_result_once() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let cwd = dir.path().join("project");
    let session_id = "2e2c9e30-3f70-4b6c-a1de-46d55e4b9119";
    let mut manager = SessionManager::create_with_id(&cwd, &sessions, session_id).unwrap();
    manager
        .append_message(AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "calling tool".to_string(),
                },
                ContentBlock::ToolCall {
                    id: "orphan_call_1".to_string(),
                    name: "bash".to_string(),
                    args: json!({"command": "echo should-not-run"}),
                },
            ],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        })
        .unwrap();
    let file = manager.path().to_path_buf();
    drop(manager);

    let mut reopened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(reopened.repair_orphaned_tool_calls().unwrap(), 1);
    drop(reopened);

    let reopened = SessionManager::open_existing(&file).unwrap();
    let entries = reopened.build_context_entries().unwrap();
    let tool_results = entries
        .iter()
        .filter_map(|entry| match &entry.entry_type {
            SessionEntryType::Message(message) if message.role == AgentMessageRole::ToolResult => {
                Some(message)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(
        tool_results[0].tool_call_id.as_deref(),
        Some("orphan_call_1")
    );
    assert!(
        tool_results[0]
            .content_text()
            .contains("previous execution outcome unknown")
    );
    assert!(tool_results[0].content_text().contains("do not retry"));

    // 第二次打开不再追加：幂等。
    let mut reopened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(reopened.repair_orphaned_tool_calls().unwrap(), 0);
}

#[test]
fn build_session_context_model_and_thinking_from_entries() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("settings.jsonl");
    let lines = [
        r#"{"type":"session","version":3,"id":"s1","timestamp":"2024-01-01T00:00:00.000Z","cwd":"C:/work"}"#,
        r#"{"type":"message","id":"aaaa1111","parentId":null,"timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":"hello"}}"#,
        r#"{"type":"model_change","id":"bbbb2222","parentId":"aaaa1111","timestamp":"2024-01-01T00:00:02.000Z","provider":"openai","modelId":"gpt-4o"}"#,
        r#"{"type":"thinking_level_change","id":"cccc3333","parentId":"bbbb2222","timestamp":"2024-01-01T00:00:03.000Z","thinkingLevel":"high"}"#,
        r#"{"type":"message","id":"dddd4444","parentId":"cccc3333","timestamp":"2024-01-01T00:00:04.000Z","message":{"role":"assistant","content":"reply"}}"#,
    ];
    std::fs::write(&file, lines.join("\n")).unwrap();

    let manager = SessionManager::open(&file).unwrap();
    let ctx = manager.build_session_context().unwrap();
    assert_eq!(ctx.model.as_deref(), Some("openai/gpt-4o"));
    assert_eq!(ctx.thinking_level.as_deref(), Some("high"));
    let roles: Vec<ModelRole> = ctx.messages.iter().map(|m| m.role.clone()).collect();
    assert_eq!(roles, vec![ModelRole::User, ModelRole::Assistant]);
    assert_eq!(ctx.messages[0].content, "hello");
    assert_eq!(ctx.messages[1].content, "reply");
}

#[test]
fn responses_private_replay_round_trips_exactly_through_jsonl() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let cwd = dir.path().join("project");
    let replay = ProviderReasoningReplay::Responses {
        provider_name: "provider".to_string(),
        model_name: "model".to_string(),
        reasoning_effort: "high".to_string(),
        tool_call_ids: vec!["call_1".to_string()],
        items: vec![
            json!({
                "type": "reasoning",
                "id": "rs_1",
                "encrypted_content": "opaque-secret"
            }),
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "write",
                "arguments": "{\"path\":\"out.txt\"}"
            }),
        ],
    };
    let mut manager =
        SessionManager::create_with_id(&cwd, &sessions, "f7be8e2a-6f7f-4b55-b1cf-ef6d00c4d8f8")
            .unwrap();
    manager
        .append_message(AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "summary projection".to_string(),
                    signature: None,
                },
                ContentBlock::ToolCall {
                    id: "call_1".to_string(),
                    name: "write".to_string(),
                    args: json!({"path": "out.txt"}),
                },
            ],
            provider_reasoning_replay: Some(replay.clone()),
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        })
        .unwrap();
    let path = manager.path().to_path_buf();
    drop(manager);

    let reopened = SessionManager::open_existing(&path).unwrap();
    let entries = reopened.build_context_entries().unwrap();
    let message = match &entries[0].entry_type {
        SessionEntryType::Message(message) => message,
        other => panic!("expected assistant message, got {other:?}"),
    };
    assert_eq!(message.provider_reasoning_replay.as_ref(), Some(&replay));
    assert_eq!(message.thinking_blocks().len(), 1);
    let debug = format!("{message:?}");
    assert!(!debug.contains("opaque-secret"));
    assert!(debug.contains("output_item_count"));
    let wire = serde_json::to_value(message).unwrap();
    assert_eq!(
        wire["providerReasoningReplay"]["items"][0]["encrypted_content"],
        "opaque-secret"
    );
    assert_eq!(
        reopened.build_session_context().unwrap().messages[0].content,
        ""
    );
}

fn session_header(id: &str) -> String {
    format!(
        r#"{{"type":"session","version":4,"id":"{id}","timestamp":"2024-01-01T00:00:00.000Z","cwd":"C:/work"}}"#
    )
}

fn session_message(id: &str, parent: Option<&str>, text: &str) -> String {
    let parent = parent.map_or_else(|| "null".to_string(), |value| format!("\"{value}\""));
    format!(
        r#"{{"type":"message","id":"{id}","parentId":{parent},"timestamp":"2024-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
    )
}

#[test]
fn strict_open_rejects_intermediate_malformed_json() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("broken-middle.jsonl");
    let content = format!(
        "{}\n{}\nnot-json\n{}\n",
        session_header("strict-middle"),
        session_message("entry-1", None, "one"),
        session_message("entry-2", Some("entry-1"), "two"),
    );
    std::fs::write(&file, content).unwrap();
    let error = SessionManager::open_existing(&file)
        .err()
        .expect("malformed middle line must be rejected");
    assert!(matches!(error, SessionError::MalformedLine { line: 3, .. }));
}

#[test]
fn strict_open_repairs_torn_tail_and_missing_final_newline() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("torn-tail.jsonl");
    let prefix = format!(
        "{}\n{}\n",
        session_header("strict-tail"),
        session_message("entry-1", None, "one")
    );
    std::fs::write(&file, format!("{prefix}{{\"type\":\"message\",\"id\":\"")).unwrap();
    let opened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(opened.build_context_entries().unwrap().len(), 1);
    assert!(std::fs::read(&file).unwrap().ends_with(b"\n"));
    drop(opened);
    let mut reopened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(reopened.build_context_entries().unwrap().len(), 1);
    reopened.append_message(user("after repair")).unwrap();
    let reopened_again = SessionManager::open_existing(&file).unwrap();
    assert_eq!(reopened_again.build_context_entries().unwrap().len(), 2);

    let missing_newline = dir.path().join("missing-newline.jsonl");
    std::fs::write(
        &missing_newline,
        format!(
            "{}\n{}",
            session_header("strict-newline"),
            session_message("entry-1", None, "one")
        ),
    )
    .unwrap();
    let opened = SessionManager::open_existing(&missing_newline).unwrap();
    assert_eq!(opened.build_context_entries().unwrap().len(), 1);
    assert!(std::fs::read(&missing_newline).unwrap().ends_with(b"\n"));
}

#[test]
fn strict_open_rejects_duplicate_missing_parent_and_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (
            "duplicate",
            format!(
                "{}\n{}\n{}\n",
                session_header("duplicate"),
                session_message("same", None, "one"),
                session_message("same", Some("same"), "two")
            ),
            "duplicate",
        ),
        (
            "missing-parent",
            format!(
                "{}\n{}\n",
                session_header("missing"),
                session_message("entry-1", Some("no-parent"), "one")
            ),
            "missing parent",
        ),
        (
            "cycle",
            format!(
                "{}\n{}\n{}\n",
                session_header("cycle"),
                session_message("a", Some("b"), "one"),
                session_message("b", Some("a"), "two")
            ),
            "cycle",
        ),
    ];
    for (name, content, expected) in cases {
        let file = dir.path().join(format!("{name}.jsonl"));
        std::fs::write(&file, content).unwrap();
        let error = SessionManager::open_existing(&file)
            .err()
            .expect("invalid session structure must be rejected");
        assert!(error.to_string().contains(expected), "{name}: {error}");
    }
}

#[test]
fn strict_open_rejects_complete_invalid_final_json() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("invalid-final.jsonl");
    std::fs::write(&file, format!("{}\n[]", session_header("invalid-final"))).unwrap();
    let error = SessionManager::open_existing(&file)
        .err()
        .expect("complete invalid final entry must be rejected");
    assert!(matches!(error, SessionError::InvalidEntry { line: 2, .. }));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        format!("{}\n[]", session_header("invalid-final"))
    );
}

#[test]
fn strict_open_rejects_invalid_header_and_known_schema() {
    let dir = tempfile::tempdir().unwrap();
    let missing_version = dir.path().join("missing-version.jsonl");
    std::fs::write(
        &missing_version,
        format!("{}\n", r#"{"type":"session","id":"missing-version"}"#),
    )
    .unwrap();
    let error = SessionManager::open_existing(&missing_version)
        .err()
        .expect("missing header version must be rejected");
    assert!(matches!(error, SessionError::InvalidHeader(_)));

    let malformed_message = dir.path().join("malformed-message.jsonl");
    let content = format!(
        "{}\n{{\"type\":\"message\",\"id\":\"bad\",\"parentId\":null,\"message\":{{\"role\":\"user\",\"content\":\"not-blocks\"}}}}\n",
        session_header("malformed-message")
    );
    std::fs::write(&malformed_message, content).unwrap();
    let error = SessionManager::open_existing(&malformed_message)
        .err()
        .expect("known message schema errors must be rejected");
    assert!(matches!(error, SessionError::InvalidEntry { line: 2, .. }));
}

#[test]
fn bounded_session_read_checks_file_metadata_before_parsing_body() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("oversized.jsonl");
    std::fs::write(&file, "not-json").unwrap();
    let error = match parse_session_lines_with_limits(&file, 1, 1024, 10) {
        Ok(_) => panic!("metadata limit must reject before body parsing"),
        Err(error) => error,
    };
    assert!(matches!(error, SessionError::InvalidSession(message) if message.contains("1 bytes")));
}

#[test]
fn bounded_session_read_rejects_an_oversized_line_without_unbounded_growth() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("oversized-line.jsonl");
    let header = session_header("oversized-line");
    let line_limit = header.len();
    std::fs::write(&file, format!("{header}\n{}\n", "x".repeat(line_limit + 1))).unwrap();
    let error = match parse_session_lines_with_limits(&file, 1024, line_limit, 10) {
        Ok(_) => panic!("line limit must fail closed"),
        Err(error) => error,
    };
    assert!(matches!(error, SessionError::InvalidSession(message) if message.contains("line 2")));
}

#[test]
fn bounded_session_read_counts_content_entries_without_counting_header() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("entry-boundary.jsonl");
    let content = format!(
        "{}\n{}\n{}\n",
        session_header("entry-boundary"),
        session_message("entry-1", None, "one"),
        session_message("entry-2", Some("entry-1"), "two")
    );
    std::fs::write(&file, content).unwrap();
    assert!(parse_session_lines_with_limits(&file, 1024, 1024, 2).is_ok());
    let error = match parse_session_lines_with_limits(&file, 1024, 1024, 1) {
        Ok(_) => panic!("second content entry must exceed a one-entry content limit"),
        Err(error) => error,
    };
    assert!(
        matches!(error, SessionError::InvalidSession(message) if message.contains("1 entries"))
    );
}

#[test]
fn append_io_failure_does_not_advance_memory() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let before = manager.build_context_entries().unwrap();
    manager.file = dir.path().to_path_buf();
    assert!(manager.append_message(user("must fail")).is_err());
    assert_eq!(manager.build_context_entries().unwrap(), before);
    assert!(manager.leaf_id().is_empty());
}

#[test]
fn append_limits_reject_without_writing_or_advancing_memory() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let mut manager = SessionManager::create(dir.path(), &sessions).unwrap();
    let before_bytes = std::fs::read(manager.path()).unwrap();
    let error = manager
        .append_entry_with_limits(
            SessionEntryType::Message(user("oversized")),
            AppendLimits {
                line_bytes: 1,
                file_bytes: u64::MAX,
                entries: usize::MAX,
            },
        )
        .unwrap_err();
    assert!(matches!(
        error,
        SessionError::AppendLimitExceeded {
            kind: "line bytes",
            ..
        }
    ));
    assert_eq!(std::fs::read(manager.path()).unwrap(), before_bytes);
    assert_eq!(manager.total_entries(), 0);
    assert!(manager.leaf_id().is_empty());

    validate_append_limits(
        50,
        1,
        49,
        AppendLimits {
            line_bytes: 49,
            file_bytes: 100,
            entries: 2,
        },
    )
    .unwrap();
    let error = validate_append_limits(
        50,
        1,
        50,
        AppendLimits {
            line_bytes: 50,
            file_bytes: 100,
            entries: 2,
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        SessionError::AppendLimitExceeded {
            kind: "file bytes",
            actual: 101,
            ..
        }
    ));
    validate_append_limits(
        50,
        1,
        49,
        AppendLimits {
            line_bytes: 49,
            file_bytes: 101,
            entries: 2,
        },
    )
    .unwrap();
    let error = validate_append_limits(
        50,
        2,
        1,
        AppendLimits {
            line_bytes: 1,
            file_bytes: 100,
            entries: 2,
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        SessionError::AppendLimitExceeded {
            kind: "entry count",
            actual: 3,
            ..
        }
    ));

    let file = manager.path().to_path_buf();
    manager.append_message(user("durable")).unwrap();
    let before_failed_append = std::fs::read(&file).unwrap();
    drop(manager);
    let mut reopened = SessionManager::open_existing(&file).unwrap();
    let error = reopened
        .append_entry_with_limits(
            SessionEntryType::Message(user("second")),
            AppendLimits {
                line_bytes: MAX_SESSION_LINE_BYTES,
                file_bytes: MAX_SESSION_FILE_BYTES as u64,
                entries: 1,
            },
        )
        .unwrap_err();
    assert!(matches!(
        error,
        SessionError::AppendLimitExceeded {
            kind: "entry count",
            ..
        }
    ));
    assert_eq!(std::fs::read(&file).unwrap(), before_failed_append);
    let reopened_again = SessionManager::open_existing(&file).unwrap();
    assert_eq!(reopened_again.total_entries(), 1);
}

#[test]
fn atomic_publish_failure_preserves_original_target() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("session.jsonl");
    std::fs::write(&original, b"original\n").unwrap();
    let missing_temporary = dir.path().join("missing.tmp");
    let error = atomic_replace_file(&missing_temporary, &original).unwrap_err();
    assert!(!error.to_string().is_empty());
    assert_eq!(std::fs::read(&original).unwrap(), b"original\n");
}

#[test]
fn metadata_round_trip_is_durable_but_never_enters_model_context() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let mut manager =
        SessionManager::create_with_id(dir.path(), &sessions, &Uuid::now_v7().to_string()).unwrap();
    manager
        .append_metadata(SessionMetadata::turn_started("turn-1"))
        .unwrap();
    manager
        .append_metadata(
            SessionMetadata::thread_settings(
                "openai_compatible",
                "gpt-test",
                Some("medium".to_string()),
            )
            .unwrap(),
        )
        .unwrap();
    manager.append_message(user("visible")).unwrap();
    let reopened = SessionManager::open_existing(manager.path()).unwrap();
    let metadata = reopened.metadata_entries();
    assert_eq!(metadata.len(), 2);
    assert_eq!(metadata[0].kind(), SessionMetadataKind::TurnStarted);
    assert_eq!(metadata[1].field_string("model"), Some("gpt-test"));
    assert!(
        metadata
            .iter()
            .all(|entry| entry.field("parentId").is_none())
    );
    let context = reopened.build_session_context().unwrap();
    assert_eq!(context.messages.len(), 1);
    assert_eq!(context.messages[0].content, "visible");
}

#[test]
fn separate_session_managers_follow_the_latest_durable_leaf() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let session_id = Uuid::now_v7().to_string();
    let file = sessions.join(format!("{session_id}.jsonl"));
    let mut first = SessionManager::create_with_id(dir.path(), &sessions, &session_id).unwrap();
    first.append_message(user("first")).unwrap();
    let mut second = SessionManager::open_existing(&file).unwrap();
    second
        .append_metadata(SessionMetadata::turn_started("turn-1"))
        .unwrap();
    first.append_message(assistant("second")).unwrap();

    let reopened = SessionManager::open_existing(&file).unwrap();
    let context = reopened.build_session_context().unwrap();
    assert_eq!(context.messages.len(), 2);
    assert_eq!(context.messages[0].content, "first");
    assert_eq!(context.messages[1].content, "second");
    assert_eq!(reopened.metadata_entries().len(), 1);
}

#[test]
fn reopen_interrupted_repair_is_idempotent_and_synthetic() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let session_id = Uuid::now_v7().to_string();
    let file = sessions.join(format!("{session_id}.jsonl"));
    let mut manager = SessionManager::create_with_id(dir.path(), &sessions, &session_id).unwrap();
    manager
        .append_metadata(SessionMetadata::turn_started("turn-1"))
        .unwrap();
    drop(manager);

    let mut reopened = SessionManager::open_existing(&file).unwrap();
    assert_eq!(reopened.repair_interrupted_turns().unwrap(), 1);
    drop(reopened);
    let mut reopened_again = SessionManager::open_existing(&file).unwrap();
    assert_eq!(reopened_again.repair_interrupted_turns().unwrap(), 0);
    let metadata = reopened_again.metadata_entries();
    assert_eq!(metadata.len(), 2);
    assert_eq!(metadata[1].kind(), SessionMetadataKind::TurnInterrupted);
    assert!(metadata[1].synthetic());
}

#[test]
fn thread_settings_reject_sensitive_fields() {
    let mut fields = Map::new();
    fields.insert("provider".to_string(), json!("openai_compatible"));
    fields.insert("model".to_string(), json!("gpt-test"));
    fields.insert("apiKey".to_string(), json!("do-not-persist"));
    let error = SessionMetadata::new(SessionMetadataKind::ThreadSettings, fields).unwrap_err();
    assert!(error.to_string().contains("sensitive"));
}
