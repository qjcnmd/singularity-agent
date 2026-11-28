use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use model::{ContentBlock, Message, ModelConfigurationSnapshot, ModelToolCall, ProviderMessageMetadata};
use proptest::prelude::*;
use protocol::{ModelSelection, ModelUsage, ProviderAttemptStatus, RequestObservation, RequestPurpose};
use serde_json::json;

use crate::request_execution::{min_compaction_output_tokens, output_budget_tokens};
use crate::session::compaction::{plan_compaction_step, select_compactable_split};
use crate::session::context::{
    cleared_tool_result_ids, frame_compacted_summary, input_tokens, message_token_estimate, project_messages,
    resolve_context_entries,
};
use crate::session::{
    CompactionEntry, LedgerRecord, SessionData, SessionEntry, SessionManager, SessionMetadata,
    session_file_name,
};
use crate::tools::{
    COMMAND_MAX_BYTES, COMMAND_MAX_LINES, CaptureState, PreparedTool, ToolRegistrySnapshot, WEB_MAX_BYTES,
    WEB_MAX_LINES, edit_execute, edit_tool, format_head_bounded,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("singularity-prop-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn test_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create property test tokio runtime")
    })
}

fn unicode_seed_strategy(min_len: usize, max_len: usize) -> impl Strategy<Value = String> {
    const CHARS: &[char] = &['a', 'Z', '0', ' ', '\n', '中', '文', '测', '试', '🚀', 'é'];
    prop::collection::vec(proptest::sample::select(CHARS), min_len..max_len)
        .prop_map(|chars| chars.into_iter().collect())
}

fn unicode_text_strategy(max_repeats: usize) -> impl Strategy<Value = String> {
    (unicode_seed_strategy(1, 48), 1usize..=max_repeats).prop_map(|(seed, times)| seed.repeat(times))
}

fn provider_meta(provider: &str, model: &str) -> ProviderMessageMetadata {
    ProviderMessageMetadata {
        protocol: protocol::ProviderApiProtocol::Messages,
        provider_name: provider.into(),
        model_name: model.into(),
        endpoint: "https://example.invalid/v1/messages".into(),
        reasoning: None,
    }
}

#[derive(Debug, Clone)]
enum HistoryStep {
    User {
        text: String,
        with_image: bool,
    },
    Notification(String),
    AssistantWithTools {
        text: String,
        thinking: Option<String>,
        tool_outputs: Vec<String>,
    },
}

fn build_history(steps: Vec<HistoryStep>) -> Vec<Message> {
    let (mut messages, mut seq) = (Vec::new(), 0usize);
    for step in steps {
        match step {
            HistoryStep::User { text, with_image } => {
                let mut content = vec![ContentBlock::text(text)];
                if with_image {
                    seq += 1;
                    content.push(ContentBlock::Image(protocol::ImageAttachment {
                        id: format!("img_{seq}.png"),
                        name: "img.png".into(),
                        mime_type: "image/png".into(),
                        width: 64,
                        height: 64,
                    }));
                }
                messages.push(Message::User { content });
            }
            HistoryStep::Notification(t) => {
                messages.push(Message::Notification { content: vec![ContentBlock::text(t)] });
            }
            HistoryStep::AssistantWithTools { text, thinking, tool_outputs } => {
                let mut content = Vec::new();
                if let Some(thought) = thinking {
                    content.push(ContentBlock::Thinking {
                        thinking: thought,
                        metadata: serde_json::from_value(json!({ "signature": "sig_test" })).ok(),
                    });
                }
                content.push(ContentBlock::text(text));
                let mut results = Vec::new();
                for output in tool_outputs {
                    seq += 1;
                    let id = format!("call_{seq}");
                    content.push(ContentBlock::ToolCall(ModelToolCall {
                        tool_call_id: id.clone(),
                        tool_name: "tool".into(),
                        arguments: json!({ "seq": seq }),
                        metadata: None,
                    }));
                    results.push(Message::ToolResult {
                        content: vec![ContentBlock::text(output)],
                        tool_call_id: id,
                        is_error: false,
                    });
                }
                messages.push(Message::Assistant {
                    content,
                    provider_metadata: Some(provider_meta("p", "m")),
                });
                messages.extend(results);
            }
        }
    }
    messages
}

fn assert_balanced_tool_calls(messages: &[Message]) {
    let mut open_calls: HashSet<String> = HashSet::new();
    for msg in messages {
        match msg {
            Message::Assistant { .. } => {
                assert!(open_calls.is_empty(), "unanswered tool_call before next assistant");
                for call in msg.tool_calls() {
                    assert!(open_calls.insert(call.tool_call_id.clone()), "duplicate tool_call id");
                }
            }
            Message::ToolResult { tool_call_id, .. } => {
                assert!(open_calls.remove(tool_call_id), "orphaned tool_result {tool_call_id}");
            }
            _ => assert!(open_calls.is_empty(), "non-tool message interrupted tool_call/tool_result group"),
        }
    }
    assert!(open_calls.is_empty(), "dangling tool_call at end: {open_calls:?}");
}

#[derive(Debug, Clone)]
struct CompactionRoundInput {
    steps: Vec<HistoryStep>,
    drop_last_tool_result: bool,
    retain_tokens: u64,
}

fn nonempty_or(text: String, fallback: &str) -> String {
    if text.trim().is_empty() {
        fallback.into()
    } else {
        text
    }
}

fn compaction_round_strategy() -> impl Strategy<Value = CompactionRoundInput> {
    let step = prop_oneof![
        (unicode_text_strategy(10), any::<bool>())
            .prop_map(|(s, with_image)| HistoryStep::User { text: nonempty_or(s, "u"), with_image }),
        unicode_text_strategy(6).prop_map(|s| HistoryStep::Notification(nonempty_or(s, "n"))),
        (
            unicode_text_strategy(15),
            prop::option::of(unicode_text_strategy(8)),
            prop::collection::vec(unicode_text_strategy(120), 0..=2),
        )
            .prop_map(|(text, thinking, tool_outputs)| HistoryStep::AssistantWithTools {
                text: nonempty_or(text, "reply"),
                thinking: thinking.map(|t| nonempty_or(t, "thought")),
                tool_outputs,
            }),
    ];
    (prop::collection::vec(step, 1..=6), any::<bool>(), prop_oneof![Just(0u64), 1u64..=800]).prop_map(
        |(steps, drop_last_tool_result, retain_tokens)| CompactionRoundInput {
            steps,
            drop_last_tool_result,
            retain_tokens,
        },
    )
}

fn compaction_case_strategy() -> impl Strategy<Value = (String, Vec<CompactionRoundInput>)> {
    (unicode_seed_strategy(24, 48), prop::collection::vec(compaction_round_strategy(), 1..=4)).prop_map(
        |(seed, mut rounds)| {
            if !rounds.iter().any(|r| r.retain_tokens == 0) {
                rounds.last_mut().expect("nonempty rounds").retain_tokens = 0;
            }
            (seed, rounds)
        },
    )
}

fn new_test_session(tag: &str, provider: &str, model: &str) -> (TempDir, PathBuf, String, SessionManager) {
    let tmp = TempDir::new(tag);
    let (workspace, sessions) = (tmp.0.join("ws"), tmp.0.join("sessions"));
    std::fs::create_dir_all(&workspace).expect("create workspace");
    let sid = uuid::Uuid::now_v7();
    let sel = ModelSelection {
        provider: provider.into(),
        model: model.into(),
        reasoning: None,
    };
    let mgr =
        SessionManager::create_with_id(&workspace, &sessions, sid, sel, vec![]).expect("create session");
    (tmp, sessions, sid.to_string(), mgr)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn compaction_split_and_multi_round_projection_invariants(
        (prefix_seed, rounds) in compaction_case_strategy(),
    ) {
        let (_tmp, _sessions, _sid, mut manager) = new_test_session("compaction-split", "p", "m");
        let (mut seq, mut split_hits, mut plan_hits) = (0usize, 0usize, 0usize);
        let empty_summary_tokens = message_token_estimate(&frame_compacted_summary(""));
        manager
            .append_message_with_id("seed_prefix", Message::user(format!("prefix:{}", prefix_seed.repeat(24))), None, None)
            .expect("append initial compactable prefix");

        for (round_idx, round) in rounds.into_iter().enumerate() {
            let mut round_msgs = build_history(round.steps);
            if round.drop_last_tool_result && matches!(round_msgs.last(), Some(Message::ToolResult { .. })) {
                round_msgs.pop();
            }
            for msg in round_msgs {
                seq += 1;
                manager.append_message_with_id(&format!("msg_{seq}"), msg, None, None).expect("append msg");
            }

            let data = manager.data();
            let active_before = resolve_context_entries(data);
            let cleared_before = cleared_tool_result_ids(data);
            let Some((split_idx, first_kept_id)) =
                select_compactable_split(&active_before, data, round.retain_tokens, ("p", "m"), &cleared_before)
            else {
                prop_assert!(round.retain_tokens > 0, "retain_tokens == 0 with >= 2 units must not skip split");
                continue;
            };
            split_hits += 1;
            prop_assert!(split_idx >= 1 && split_idx < active_before.len());
            let (prefix, kept) = (&active_before[..split_idx], &active_before[split_idx..]);
            prop_assert!(
                prefix.iter().any(|e| matches!(e, SessionEntry::Message { .. })),
                "compacted prefix must contain at least one raw message"
            );
            assert_balanced_tool_calls(&project_messages(prefix, &cleared_before));

            prop_assert!(!kept.is_empty());
            prop_assert_eq!(first_kept_id.as_str(), kept[0].id());
            prop_assert!(
                !matches!(kept[0], SessionEntry::Message { message: Message::ToolResult { .. }, .. }),
                "kept tail must not start with an orphaned ToolResult"
            );
            if round.retain_tokens == 0 {
                prop_assert!(
                    kept[1..].iter().all(|e| matches!(e, SessionEntry::Message { message: Message::ToolResult { .. }, .. })),
                    "retain_tokens == 0 must keep only the last balanced unit"
                );
            }
            assert_balanced_tool_calls(&project_messages(kept, &cleared_before));

            let model = ModelConfigurationSnapshot { max_context_tokens: 4_000, max_output_tokens: 512 };
            let total_input = input_tokens(data, &active_before, ("p", "m"), 400, &cleared_before);
            let overflow = (round.retain_tokens == 0 && total_input >= 2_400).then_some(total_input);
            match plan_compaction_step(data, round.retain_tokens, ("p", "m"), 400, 300, model, overflow) {
                Ok(Some(step)) => {
                    plan_hits += 1;
                    let budget = u64::from(output_budget_tokens(model, step.estimated_request_input));
                    prop_assert!(budget >= min_compaction_output_tokens(model) && budget > 0);
                    if let Some(over) = overflow {
                        prop_assert!(step.estimated_request_input < over);
                    }
                    assert_balanced_tool_calls(&step.messages);
                }
                Ok(None) => {
                    let prefix_est: u64 = project_messages(prefix, &cleared_before).iter().map(message_token_estimate).sum();
                    prop_assert!(prefix_est <= empty_summary_tokens, "unexpected None with compactable prefix");
                }
                Err(err) => prop_assert!(overflow.is_some(), "unexpected plan_compaction_step error: {err}"),
            }

            let expected_kept_ids: Vec<String> = kept.iter().map(|e| e.id().to_owned()).collect();
            let compact_id = format!("compact_{round_idx}");
            manager
                .append_compaction_with_id(
                    &compact_id,
                    CompactionEntry {
                        summary: format!("summary_round_{round_idx}_condensed_context_payload"),
                        first_kept_entry_id: Some(first_kept_id),
                    },
                )
                .expect("append compaction entry");

            let active_after = resolve_context_entries(manager.data());
            prop_assert_eq!(active_after[0].id(), compact_id.as_str());
            prop_assert!(
                active_after[1..].iter().all(|e| matches!(e, SessionEntry::Message { .. })),
                "older compaction entries must not reappear in active tail"
            );
            let actual_kept_ids: Vec<String> = active_after[1..].iter().map(|e| e.id().to_owned()).collect();
            prop_assert_eq!(actual_kept_ids, expected_kept_ids);
            assert_balanced_tool_calls(&project_messages(&active_after, &cleared_tool_result_ids(manager.data())));
        }

        prop_assert!(split_hits > 0, "compaction case must exercise select_compactable_split");
        prop_assert!(plan_hits > 0, "compaction case must exercise plan_compaction_step");
    }

    #[test]
    fn turn_calibration_and_foreign_thinking_invariants(
        reply_text in unicode_seed_strategy(1, 32),
        thinking_extra in unicode_text_strategy(60),
        turn2_user in unicode_seed_strategy(1, 32),
        reported_total in 10_000u64..=50_000,
    ) {
        let (_tmp, _s1, _id1, mut mgr) = new_test_session("turn-calibration", "pa", "ma");
        let (_tmp2, _s2, _id2, mut plain_mgr) = new_test_session("turn-calibration-plain", "pa", "ma");
        let obs = RequestObservation {
            purpose: RequestPurpose::Generation,
            provider: "pa".into(),
            model: "ma".into(),
            status: ProviderAttemptStatus::Ok,
            ttft_ms: None,
            decode_ms: None,
            usage: Some(ModelUsage {
                input_tokens: reported_total - 50,
                output_tokens: 50,
                total_tokens: reported_total,
                cached_input_tokens: None,
            }),
        };
        let redacted = serde_json::from_value(json!({ "type": "redacted_thinking", "data": "x" })).ok();
        let meta = provider_meta("pa", "ma");
        let with_thinking = Message::Assistant {
            content: vec![
                ContentBlock::Thinking { thinking: thinking_extra, metadata: None },
                ContentBlock::Thinking { thinking: String::new(), metadata: redacted },
                ContentBlock::text(reply_text.clone()),
            ],
            provider_metadata: Some(meta.clone()),
        };
        let without_thinking = Message::Assistant {
            content: vec![ContentBlock::text(reply_text)],
            provider_metadata: Some(meta),
        };
        mgr.append_message_with_id("a1", with_thinking, Some(obs), None).expect("append a1");
        plain_mgr.append_message_with_id("a1", without_thinking, None, None).expect("append plain a1");

        let empty_cleared = HashSet::new();
        let before_u2 =
            input_tokens(mgr.data(), &resolve_context_entries(mgr.data()), ("pa", "ma"), 300, &empty_cleared);
        prop_assert!(before_u2 >= reported_total, "calibrated estimate must adopt real usage");

        mgr.append_record(LedgerRecord::OperationStarted).expect("append turn boundary");
        mgr.append_message_with_id("u2", Message::user(turn2_user.as_str()), None, None).expect("u2");
        plain_mgr.append_message_with_id("u2", Message::user(turn2_user.as_str()), None, None).expect("u2");

        let active = resolve_context_entries(mgr.data());
        let plain_active = resolve_context_entries(plain_mgr.data());
        let calibrated = input_tokens(mgr.data(), &active, ("pa", "ma"), 300, &empty_cleared);
        prop_assert!(calibrated > before_u2, "new user tail must increase calibrated estimate");
        prop_assert!(input_tokens(mgr.data(), &active, ("pa", "ma"), reported_total, &empty_cleared) > calibrated);
        prop_assert_eq!(
            input_tokens(mgr.data(), &active, ("pb", "mb"), 300, &empty_cleared),
            input_tokens(plain_mgr.data(), &plain_active, ("pb", "mb"), 300, &empty_cleared),
            "foreign provider/model must not count source thinking blocks"
        );

        let cp = CompactionEntry { summary: "s".into(), first_kept_entry_id: Some("u2".into()) };
        mgr.append_compaction_with_id("cp", cp).expect("append compaction");
        let after_cp =
            input_tokens(mgr.data(), &resolve_context_entries(mgr.data()), ("pa", "ma"), 300, &empty_cleared);
        prop_assert!(after_cp < reported_total && after_cp > 300, "checkpoint must truncate stale usage");
    }

    #[test]
    fn session_file_tail_repair_invariance(
        user_texts in prop::collection::vec(unicode_seed_strategy(1, 48), 0..=5),
        trailing_bytes in prop::collection::vec(any::<u8>().prop_filter("no newline", |b| *b != b'\n'), 1..=128),
    ) {
        let (_tmp, sessions, session_id, mut manager) = new_test_session("tail-repair", "p", "m");
        for (idx, text) in user_texts.iter().enumerate() {
            manager.append_message_with_id(&format!("msg_{idx}"), Message::user(text.clone()), None, None).expect("msg");
        }
        let expected_entries = manager.data().entries().len();
        let path = sessions.join(session_file_name(&session_id));
        let valid_bytes = std::fs::read(&path).expect("read valid log");
        drop(manager);

        {
            let mut file = std::fs::OpenOptions::new().append(true).open(&path).expect("open for append");
            file.write_all(&trailing_bytes).expect("append truncated tail");
        }
        let tampered_bytes = std::fs::read(&path).expect("read tampered log");

        let readonly = SessionData::open(&path, Some(&session_id)).expect("readonly ignores incomplete tail");
        prop_assert_eq!(readonly.entries().len(), expected_entries);
        prop_assert_eq!(std::fs::read(&path).expect("read after readonly"), tampered_bytes);

        let repaired = SessionManager::open_existing(&path, &session_id).expect("writer repairs tail");
        prop_assert_eq!(repaired.data().entries().len(), expected_entries);
        drop(repaired);
        prop_assert_eq!(std::fs::read(&path).expect("read after repair"), valid_bytes);
    }

    #[test]
    fn format_head_bounded_preserves_utf8_and_spills_exact_content(
        seed in unicode_seed_strategy(4, 32),
        repeat in 1usize..=1_500,
        single_line in any::<bool>(),
    ) {
        let tmp = TempDir::new("head-bounded");
        let content = if single_line {
            seed.replace('\n', "x").repeat(repeat)
        } else {
            let line = seed.replace('\n', " ");
            (0..repeat).map(|i| format!("{i}:{line}")).collect::<Vec<_>>().join("\n")
        };
        let out = format_head_bounded(&content, &tmp.0);
        if content.lines().count() <= WEB_MAX_LINES && content.len() <= WEB_MAX_BYTES {
            prop_assert_eq!(out, content);
        } else {
            prop_assert!(out.contains("Full output: "), "missing spill note");
            let path_str = out.rsplit("Full output: ").next().expect("spill path").trim();
            prop_assert_eq!(std::fs::read_to_string(path_str).expect("read spill"), content);
            let head_body = out.split("\n\n[Showing ").next().expect("head body");
            prop_assert!(head_body.len() <= WEB_MAX_BYTES && head_body.lines().count() <= WEB_MAX_LINES);
        }
    }

    #[test]
    fn capture_state_head_tail_chunk_invariance_and_bounds(
        seed in unicode_seed_strategy(4, 32),
        repeat in 1usize..=800,
        single_line in any::<bool>(),
        chunk_sizes in prop::collection::vec(1usize..=2048, 1..=16),
    ) {
        let content = if single_line {
            format!("HEAD_MARK_{}_TAIL_MARK", seed.replace('\n', "x").repeat(repeat))
        } else {
            let line = seed.replace('\n', " ");
            let mut lines: Vec<String> = (0..repeat).map(|i| format!("{i}:{line}")).collect();
            lines.insert(0, "HEAD_MARK".into());
            lines.push("TAIL_MARK".into());
            lines.join("\n")
        };

        let tmp_whole = TempDir::new("cap-whole");
        let mut whole = CaptureState::new(tmp_whole.0.clone());
        whole.ingest(&content);
        let (whole_out, whole_note) = whole.finish();

        let tmp_chunked = TempDir::new("cap-chunked");
        let mut chunked = CaptureState::new(tmp_chunked.0.clone());
        let (mut pos, mut idx) = (0usize, 0usize);
        while pos < content.len() {
            let step = chunk_sizes[idx % chunk_sizes.len()];
            idx += 1;
            let mut end = content.floor_char_boundary((pos + step).min(content.len()));
            if end == pos {
                end = content.ceil_char_boundary((pos + 1).min(content.len()));
            }
            chunked.ingest(&content[pos..end]);
            pos = end;
        }
        let (chunked_out, chunked_note) = chunked.finish();
        prop_assert_eq!(&whole_out, &chunked_out, "chunk boundaries must not alter output");

        let total_lines = content.bytes().filter(|b| *b == b'\n').count()
            + usize::from(!content.is_empty() && !content.ends_with('\n'));
        if total_lines <= COMMAND_MAX_LINES && content.len() <= COMMAND_MAX_BYTES {
            prop_assert_eq!(whole_out, content);
            prop_assert!(whole_note.is_none() && chunked_note.is_none());
        } else {
            prop_assert!(whole_out.len() <= COMMAND_MAX_BYTES + 256);
            prop_assert!(whole_out.contains("HEAD_MARK") && whole_out.contains("TAIL_MARK"));
            for note in [whole_note, chunked_note] {
                let path = note.expect("spill note").strip_prefix("Full output: ").expect("prefix").to_owned();
                prop_assert_eq!(std::fs::read_to_string(path).expect("read spill"), content.as_str());
            }
        }
    }

    #[test]
    fn edit_tolerant_matching_preserves_uniqueness_bom_and_crlf(
        bom in any::<bool>(),
        crlf in any::<bool>(),
        indent_spaces in 1usize..=8,
        mode in 0u8..=3,
        duplicate in any::<bool>(),
    ) {
        let (eol, pad) = (if crlf { "\r\n" } else { "\n" }, " ".repeat(indent_spaces));
        let (block, old_query, new_text, expected_replaced) = match mode {
            0 => (
                format!("{pad}let alpha = 1;{eol}{pad}let beta = 2;"),
                format!("{pad}let alpha = 1;\n{pad}let beta = 2;"),
                format!("{pad}let alpha = 1;\n{pad}let beta = 99;"),
                format!("{pad}let alpha = 1;{eol}{pad}let beta = 99;"),
            ),
            1 => (
                format!("{pad}let alpha = 1;   {eol}{pad}let beta = 2;  "),
                format!("{pad}let alpha = 1;\n{pad}let beta = 2;"),
                format!("{pad}let alpha = 1;\n{pad}let beta = 99;"),
                format!("{pad}let alpha = 1;{eol}{pad}let beta = 99;"),
            ),
            2 => (
                format!("{pad}// don\u{2019}t \u{2013} \u{201C}touch\u{201D}\u{00A0}here{eol}{pad}let v = 1;"),
                format!("{pad}// don't - \"touch\" here\n{pad}let v = 1;"),
                format!("{pad}// updated\n{pad}let v = 2;"),
                format!("{pad}// updated{eol}{pad}let v = 2;"),
            ),
            _ => (
                format!("{pad}fn demo() {{{eol}{pad}    let v = 1;{eol}{pad}}}"),
                "fn demo() {\n    let v = 1;\n}".into(),
                "fn demo() {\n    let v = 2;\n}".into(),
                format!("{pad}fn demo() {{{eol}{pad}    let v = 2;{eol}{pad}}}"),
            ),
        };
        let prefix = if bom { "\u{feff}" } else { "" };
        let body = if duplicate {
            format!("header{eol}{block}{eol}mid{eol}{block}{eol}footer{eol}")
        } else {
            format!("header{eol}{block}{eol}footer{eol}")
        };
        let file = format!("{prefix}{body}");
        let tmp = TempDir::new("edit-tolerant");
        let file_path = tmp.0.join("demo.rs");
        std::fs::write(&file_path, &file).expect("write demo.rs");

        let mut registry = ToolRegistrySnapshot::default();
        registry.add_builtin([edit_tool()]);
        let Ok(PreparedTool::Edit(args)) = registry.preflight(
            "edit",
            &json!({ "path": "demo.rs", "edits": [{ "old_text": old_query, "new_text": new_text }] }),
        ) else {
            panic!("valid edit args must preflight");
        };
        let file_reads = std::sync::Arc::new(crate::tools::FileReadTracker::default());
        let canonical = file_path.canonicalize().expect("canonical demo.rs");
        let meta = std::fs::metadata(&canonical).expect("demo.rs metadata");
        file_reads.record_metadata(app_core::workspace_key(&canonical), &meta);
        let exec = test_runtime().block_on(edit_execute(
            args,
            tmp.0.clone(),
            tmp.0.join("outputs"),
            file_reads,
            tokio_util::sync::CancellationToken::new(),
        ));
        let disk = std::fs::read_to_string(&file_path).expect("read demo.rs");
        if duplicate {
            prop_assert!(exec.is_error, "duplicate match must fail across all tolerance tiers");
            prop_assert_eq!(disk, file, "failed edit must leave original file untouched");
        } else {
            prop_assert!(!exec.is_error, "unique tolerant match must succeed: {}", exec.content);
            prop_assert_eq!(disk, format!("{prefix}header{eol}{expected_replaced}{eol}footer{eol}"));
        }
    }

    #[test]
    fn rewind_before_message_preserves_latest_settings_and_name(
        turns in 2usize..=5,
        rewind_to in 0usize..5,
        mid_turn in any::<bool>(),
        rename_after in any::<bool>(),
        reselect_after in any::<bool>(),
    ) {
        let rewind_idx = rewind_to % turns;
        let (_tmp, sessions, sid, mut mgr) = new_test_session("rewind-meta", "p0", "m0");
        let (mut target_id, mut target_turn_id) = (String::new(), String::new());
        for t in 0..turns {
            let (tid, uid) = (format!("turn_{t}"), format!("u_{t}"));
            mgr.append_operation_started(tid.clone()).expect("start turn");
            mgr.append_message_with_id(&uid, Message::user(format!("msg {t}")), None, None).expect("user");
            mgr.append_message_with_id(&format!("a_{t}"), Message::assistant(format!("reply {t}"), String::new(), Vec::new(), None), None, None).expect("assistant");
            if mid_turn && t == rewind_idx {
                let mid_uid = format!("u_mid_{t}");
                mgr.append_message_with_id(&mid_uid, Message::user("steer"), None, None).expect("mid");
                (target_id, target_turn_id) = (format!("{mid_uid}:text:0"), tid.clone());
            } else if t == rewind_idx {
                (target_id, target_turn_id) = (format!("{uid}:text:0"), tid.clone());
            }
            if t == turns - 1 && reselect_after {
                mgr.append_metadata(SessionMetadata::ThreadSettings {
                    selection: ModelSelection { provider: "p_latest".into(), model: "m_latest".into(), reasoning: Some("high".into()) },
                }).expect("settings");
            }
            if t == turns - 1 && rename_after {
                mgr.append_metadata(SessionMetadata::ThreadName { name: "Renamed Title".into() }).expect("name");
            }
            mgr.append_record(LedgerRecord::OperationFinished {
                turn_id: tid,
                outcome: protocol::TurnStatus::Completed,
                error: None,
                user_stopped: false,
            }).expect("finish turn");
        }

        mgr.rewind_before_message(&target_id).expect("rewind succeeds");
        let reloaded = SessionData::open(&sessions.join(session_file_name(&sid)), None).expect("reopen");
        let raw_target = target_id.strip_suffix(":text:0").unwrap_or(&target_id);
        prop_assert!(!reloaded.entries().iter().any(|e| e.id() == raw_target));
        if mid_turn {
            prop_assert!(
                reloaded.entries().iter().any(|e| matches!(
                    e,
                    SessionEntry::Record { record: LedgerRecord::OperationFinished { turn_id, outcome: protocol::TurnStatus::Completed, .. }, .. }
                        if turn_id == &target_turn_id
                )),
                "mid-turn rewind must cleanly close the preceding turn"
            );
        } else {
            prop_assert!(!reloaded.entries().iter().any(|e| e.id() == target_turn_id));
        }

        let latest_sel = reloaded.entries().iter().rev().find_map(|e| match e {
            SessionEntry::Metadata { metadata: SessionMetadata::ThreadSettings { selection }, .. } => Some(selection),
            _ => None,
        });
        prop_assert_eq!(latest_sel.map(|s| s.provider.as_str()), Some(if reselect_after { "p_latest" } else { "p0" }));

        let latest_name = reloaded.entries().iter().rev().find_map(|e| match e {
            SessionEntry::Metadata { metadata: SessionMetadata::ThreadName { name }, .. } => Some(name.as_str()),
            _ => None,
        });
        prop_assert_eq!(latest_name, rename_after.then_some("Renamed Title"));
    }
}
