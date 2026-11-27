use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

use model::{ContentBlock, Message, ModelConfigurationSnapshot, ModelToolCall, ProviderMessageMetadata};
use proptest::prelude::*;
use protocol::{ModelSelection, ModelUsage, ProviderAttemptStatus, RequestObservation, RequestPurpose};
use serde_json::json;

use crate::agent::modules::{ModuleInputs, assemble as assemble_modules};
use crate::agent::precheck_manual_compaction;
use crate::request_execution::{min_compaction_output_tokens, output_budget_tokens};
use crate::session::compaction::{plan_compaction_step, select_compactable_split};
use crate::session::context::{
    PreparedContext, cleared_tool_result_ids, frame_compacted_summary, input_tokens, load_images,
    message_token_estimate, project_messages, resolve_context_entries, select_idle_clearable_entry_ids,
};
use crate::session::{
    CompactionEntry, LedgerRecord, SessionData, SessionEntry, SessionManager, SessionMetadata,
    session_file_name,
};
use crate::tools::{
    ABORTED_MESSAGE, COMMAND_MAX_BYTES, COMMAND_MAX_LINES, CaptureState, GlobArgs, GrepArgs, PreparedTool,
    WEB_MAX_BYTES, WEB_MAX_LINES, apply_edits_for_test, execute_glob_for_test, execute_grep_for_test,
    execute_todo_for_test, format_diff_result_for_test, format_head_bounded,
    search_reader_with_cancel_for_test,
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

fn unicode_seed_strategy(min_len: usize, max_len: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            Just('a'),
            Just('Z'),
            Just('0'),
            Just(' '),
            Just('\n'),
            Just('中'),
            Just('文'),
            Just('测'),
            Just('试'),
            Just('🚀'),
            Just('é'),
        ],
        min_len..max_len,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

fn unicode_text_strategy(max_repeats: usize) -> impl Strategy<Value = String> {
    (unicode_seed_strategy(1, 48), 1usize..=max_repeats).prop_map(|(seed, times)| seed.repeat(times))
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
    let mut messages = Vec::new();
    let mut seq = 0usize;
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
                    provider_metadata: Some(ProviderMessageMetadata {
                        protocol: protocol::ProviderApiProtocol::Messages,
                        provider_name: "p".into(),
                        model_name: "m".into(),
                        endpoint: "https://example.invalid/v1/messages".into(),
                        prefix_digest: None,
                        reasoning: None,
                    }),
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
            _ => {
                assert!(open_calls.is_empty(), "non-tool message interrupted tool_call/tool_result group");
            }
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

fn compaction_round_strategy() -> impl Strategy<Value = CompactionRoundInput> {
    (
        prop::collection::vec(
            prop_oneof![
                (unicode_text_strategy(10), any::<bool>()).prop_map(|(s, with_image)| {
                    HistoryStep::User {
                        text: if s.trim().is_empty() { "u".into() } else { s },
                        with_image,
                    }
                }),
                unicode_text_strategy(6).prop_map(|s| HistoryStep::Notification(if s.trim().is_empty() {
                    "n".into()
                } else {
                    s
                })),
                (
                    unicode_text_strategy(15),
                    prop::option::of(unicode_text_strategy(8)),
                    prop::collection::vec(unicode_text_strategy(120), 0..=3),
                )
                    .prop_map(|(text, thinking, tool_outputs)| {
                        HistoryStep::AssistantWithTools {
                            text: if text.trim().is_empty() {
                                "reply".into()
                            } else {
                                text
                            },
                            thinking: thinking
                                .map(|t| if t.trim().is_empty() { "thought".into() } else { t }),
                            tool_outputs,
                        }
                    }),
            ],
            1..=8,
        ),
        any::<bool>(),
        prop_oneof![Just(0u64), 1u64..=800],
    )
        .prop_map(|(steps, drop_last_tool_result, retain_tokens)| CompactionRoundInput {
            steps,
            drop_last_tool_result,
            retain_tokens,
        })
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
        rounds in prop::collection::vec(compaction_round_strategy(), 1..=4),
    ) {
        let (_tmp, _sessions, _sid, mut manager) = new_test_session("compaction-split", "p", "m");
        let mut seq = 0usize;

        for (round_idx, round) in rounds.into_iter().enumerate() {
            let mut round_msgs = build_history(round.steps);
            if round.drop_last_tool_result
                && matches!(round_msgs.last(), Some(Message::ToolResult { .. }))
            {
                round_msgs.pop();
            }
            for msg in round_msgs {
                seq += 1;
                let id = format!("msg_{seq}");
                manager
                    .append_message_with_id(&id, msg, None, None)
                    .expect("append round message");
            }

            let data = manager.data();
            let active_before = resolve_context_entries(data);
            let cleared_before = cleared_tool_result_ids(data);
            let Some((split_idx, first_kept_id)) =
                select_compactable_split(&active_before, data, round.retain_tokens, ("p", "m"), &cleared_before)
            else {
                continue;
            };
            prop_assert!(split_idx >= 1 && split_idx < active_before.len());
            let prefix = &active_before[..split_idx];
            let kept = &active_before[split_idx..];
            prop_assert!(
                prefix.iter().any(|e| matches!(e, SessionEntry::Message { .. })),
                "compacted prefix must contain at least one raw message"
            );

            assert_balanced_tool_calls(&project_messages(prefix, &cleared_before));

            prop_assert!(!kept.is_empty());
            prop_assert_eq!(first_kept_id.as_str(), kept[0].id());
            prop_assert!(
                !matches!(
                    kept[0],
                    SessionEntry::Message { message: Message::ToolResult { .. }, .. }
                ),
                "kept tail must not start with an orphaned ToolResult"
            );
            if round.retain_tokens == 0 {
                let tail_after_first_is_only_tool_results = kept[1..].iter().all(|entry| {
                    matches!(entry, SessionEntry::Message { message: Message::ToolResult { .. }, .. })
                });
                prop_assert!(
                    tail_after_first_is_only_tool_results,
                    "retain_tokens == 0 must keep exactly the last balanced atomic unit"
                );
            }
            assert_balanced_tool_calls(&project_messages(kept, &cleared_before));

            let model = ModelConfigurationSnapshot { max_context_tokens: 4_000, max_output_tokens: 512 };
            let overflow =
                (round.retain_tokens == 0).then(|| input_tokens(data, &active_before, ("p", "m"), 400));
            if let Ok(Some(step)) =
                plan_compaction_step(data, round.retain_tokens, ("p", "m"), 400, 300, model, overflow)
            {
                let budget = u64::from(output_budget_tokens(model, step.estimated_request_input));
                prop_assert!(budget >= min_compaction_output_tokens(model) && budget > 0);
                if let Some(over) = overflow {
                    prop_assert!(step.estimated_request_input < over);
                }
                assert_balanced_tool_calls(&step.messages);
            }

            let expected_kept_ids: Vec<String> = kept.iter().map(|e| e.id().to_owned()).collect();
            let compact_id = format!("compact_{round_idx}");
            let summary = format!("summary_{round_idx}");
            manager
                .append_compaction_with_id(
                    &compact_id,
                    CompactionEntry {
                        summary: summary.clone(),
                        first_kept_entry_id: Some(first_kept_id),
                    },
                )
                .expect("append compaction entry");

            let active_after = resolve_context_entries(manager.data());
            prop_assert_eq!(active_after[0].id(), compact_id.as_str());
            prop_assert!(
                active_after[1..]
                    .iter()
                    .all(|e| matches!(e, SessionEntry::Message { .. })),
                "older compaction entries must never reappear in active tail"
            );
            let actual_kept_ids: Vec<String> =
                active_after[1..].iter().map(|e| e.id().to_owned()).collect();
            prop_assert_eq!(actual_kept_ids, expected_kept_ids);
            let cleared_after = cleared_tool_result_ids(manager.data());
            assert_balanced_tool_calls(&project_messages(&active_after, &cleared_after));
        }
    }

    #[test]
    fn turn_calibration_and_foreign_thinking_invariants(
        reply_text in unicode_seed_strategy(1, 32),
        thinking_extra in unicode_text_strategy(60),
        turn2_user in unicode_seed_strategy(1, 32),
        reported_total in 500u64..=50_000,
    ) {
        let (_tmp, _sessions, _sid, mut mgr) = new_test_session("turn-calibration", "pa", "ma");
        let usage = ModelUsage {
            input_tokens: reported_total - 50,
            output_tokens: 50,
            total_tokens: reported_total,
            cached_input_tokens: None,
        };
        let obs = RequestObservation {
            purpose: RequestPurpose::Generation,
            provider: "pa".into(),
            model: "ma".into(),
            status: ProviderAttemptStatus::Ok,
            ttft_ms: None,
            decode_ms: None,
            usage: Some(usage),
        };
        let redacted = serde_json::from_value(json!({ "type": "redacted_thinking", "data": "x" })).ok();
        let meta = ProviderMessageMetadata {
            protocol: protocol::ProviderApiProtocol::Messages,
            provider_name: "pa".into(),
            model_name: "ma".into(),
            endpoint: "https://example.invalid/v1/messages".into(),
            prefix_digest: None,
            reasoning: None,
        };
        let assistant = Message::Assistant {
            content: vec![
                ContentBlock::Thinking { thinking: thinking_extra, metadata: None },
                ContentBlock::Thinking { thinking: String::new(), metadata: redacted },
                ContentBlock::text(reply_text.clone()),
            ],
            provider_metadata: Some(meta),
        };
        let full_a1 = message_token_estimate(&assistant);
        mgr.append_message_with_id("a1", assistant, Some(obs), None).expect("append a1");
        mgr.append_record(LedgerRecord::OperationStarted)
            .expect("append turn boundary");
        mgr.append_message_with_id("u2", Message::user(turn2_user.as_str()), None, None).expect("append u2");
        let tail = message_token_estimate(&Message::user(turn2_user.as_str()));
        let active = resolve_context_entries(mgr.data());
        let plain = message_token_estimate(&Message::Assistant {
            content: vec![ContentBlock::text(reply_text)],
            provider_metadata: None,
        });
        prop_assert_eq!(input_tokens(mgr.data(), &active, ("pa", "ma"), 300), reported_total + tail);
        prop_assert_eq!(
            input_tokens(mgr.data(), &active, ("pa", "ma"), reported_total - 50),
            full_a1 + tail + reported_total - 50
        );
        let grown = input_tokens(mgr.data(), &active, ("pa", "ma"), reported_total);
        prop_assert!(grown == full_a1 + tail + reported_total && grown > reported_total + tail);
        prop_assert_eq!(input_tokens(mgr.data(), &active, ("pb", "mb"), 300), plain + tail + 300);
        let cp = CompactionEntry { summary: "s".into(), first_kept_entry_id: Some("u2".into()) };
        mgr.append_compaction_with_id("cp", cp).expect("append compaction");
        let after = resolve_context_entries(mgr.data());
        let expected = message_token_estimate(&frame_compacted_summary("s")) + tail + 300;
        prop_assert_eq!(input_tokens(mgr.data(), &after, ("pa", "ma"), 300), expected);
    }

    #[test]
    fn session_file_tail_repair_invariance(
        user_texts in prop::collection::vec(unicode_seed_strategy(1, 48), 0..=5),
        trailing_bytes in prop::collection::vec(any::<u8>().prop_filter("no newline", |b| *b != b'\n'), 1..=128),
    ) {
        let (_tmp, sessions, session_id, mut manager) = new_test_session("tail-repair", "p", "m");
        for (idx, text) in user_texts.iter().enumerate() {
            manager
                .append_message_with_id(&format!("msg_{idx}"), Message::user(text.clone()), None, None)
                .expect("append user message");
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

        let readonly =
            SessionData::open(&path, Some(&session_id)).expect("readonly open ignores incomplete tail");
        prop_assert_eq!(readonly.entries().len(), expected_entries);
        prop_assert_eq!(
            std::fs::read(&path).expect("read after readonly open"),
            tampered_bytes,
            "readonly open must never modify session file on disk"
        );

        let repaired = SessionManager::open_existing(&path, &session_id).expect("writer repairs tail");
        prop_assert_eq!(repaired.data().entries().len(), expected_entries);
        drop(repaired);

        prop_assert_eq!(
            std::fs::read(&path).expect("read after repair"),
            valid_bytes,
            "repair must restore exact valid prefix bytes"
        );
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
        let needs_truncation = content.lines().count() > WEB_MAX_LINES || content.len() > WEB_MAX_BYTES;
        if !needs_truncation {
            prop_assert_eq!(out, content);
        } else {
            prop_assert!(out.contains("Full output: "), "missing spill note");
            let path_str = out
                .rsplit("Full output: ")
                .next()
                .expect("spill path")
                .trim();
            let spilled = std::fs::read_to_string(path_str).expect("read spilled output");
            prop_assert_eq!(spilled, content);
            let head_body = out.split("\n\n[Showing ").next().expect("head body");
            prop_assert!(head_body.len() <= WEB_MAX_BYTES, "head body exceeded byte cap");
            prop_assert!(head_body.lines().count() <= WEB_MAX_LINES, "head body exceeded line cap");
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
            let body = seed.replace('\n', "x");
            format!("HEAD_MARK_{}_TAIL_MARK", body.repeat(repeat))
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
        let (whole_head_buf, whole_tail_buf) = whole.buffer_bytes();
        let (whole_out, whole_note) = whole.finish();

        let tmp_chunked = TempDir::new("cap-chunked");
        let mut chunked = CaptureState::new(tmp_chunked.0.clone());
        let mut pos = 0usize;
        let mut idx = 0usize;
        while pos < content.len() {
            let step = chunk_sizes[idx % chunk_sizes.len()];
            idx += 1;
            let mut end = content.floor_char_boundary((pos + step).min(content.len()));
            if end == pos {
                end = content.ceil_char_boundary((pos + 1).min(content.len()));
            }
            chunked.ingest(&content[pos..end]);
            let (hb, tb) = chunked.buffer_bytes();
            prop_assert!(hb <= COMMAND_MAX_BYTES / 2, "head buffer exceeded bound: {hb}");
            prop_assert!(tb <= COMMAND_MAX_BYTES * 2, "tail buffer exceeded bound: {tb}");
            pos = end;
        }
        let (chunked_out, chunked_note) = chunked.finish();

        prop_assert!(whole_head_buf <= COMMAND_MAX_BYTES / 2);
        prop_assert!(whole_tail_buf <= COMMAND_MAX_BYTES * 2);
        prop_assert_eq!(&whole_out, &chunked_out, "chunk boundaries must not alter output");

        let total_lines = content.bytes().filter(|b| *b == b'\n').count()
            + usize::from(!content.is_empty() && !content.ends_with('\n'));
        let truncated = total_lines > COMMAND_MAX_LINES || content.len() > COMMAND_MAX_BYTES;
        if !truncated {
            prop_assert_eq!(whole_out, content);
            prop_assert!(whole_note.is_none());
            prop_assert!(chunked_note.is_none());
        } else {
            prop_assert!(whole_out.contains("HEAD_MARK"), "truncated output must preserve head");
            prop_assert!(whole_out.contains("TAIL_MARK"), "truncated output must preserve tail");
            prop_assert!(whole_out.contains("[... "), "truncated output must contain omission marker");
            let note = chunked_note.expect("truncated output must spill to disk");
            let path = note.strip_prefix("Full output: ").expect("spill path prefix");
            let spilled = std::fs::read_to_string(path).expect("read spilled file");
            prop_assert_eq!(spilled, content, "spilled file must match full original content");
        }
    }

    #[test]
    fn edit_tolerant_matching_preserves_uniqueness_bom_and_crlf(
        bom in any::<bool>(),
        crlf in any::<bool>(),
        indent_spaces in 1usize..=8,
        mode in 0u8..=4,
        duplicate in any::<bool>(),
    ) {
        let eol = if crlf { "\r\n" } else { "\n" };
        let pad = " ".repeat(indent_spaces);
        let (block_in_file, old_query, new_text, expected_replaced) = match mode {
            // Exact match
            0 => (
                format!("{pad}let alpha = 1;{eol}{pad}let beta = 2;"),
                format!("{pad}let alpha = 1;\n{pad}let beta = 2;"),
                format!("{pad}let alpha = 1;\n{pad}let beta = 99;"),
                format!("{pad}let alpha = 1;{eol}{pad}let beta = 99;"),
            ),
            // Trailing whitespace tolerance
            1 => (
                format!("{pad}let alpha = 1;   {eol}{pad}let beta = 2;  "),
                format!("{pad}let alpha = 1;\n{pad}let beta = 2;"),
                format!("{pad}let alpha = 1;\n{pad}let beta = 99;"),
                format!("{pad}let alpha = 1;{eol}{pad}let beta = 99;"),
            ),
            // Unicode smart quotes / en-dash / NBSP tolerance
            2 => (
                format!("{pad}// don\u{2019}t \u{2013} \u{201C}touch\u{201D}\u{00A0}here{eol}{pad}let v = 1;"),
                format!("{pad}// don't - \"touch\" here\n{pad}let v = 1;"),
                format!("{pad}// updated\n{pad}let v = 2;"),
                format!("{pad}// updated{eol}{pad}let v = 2;"),
            ),
            // Outer indentation shift (IndentationFlexible + automatic re-indentation)
            3 => (
                format!("{pad}fn demo() {{{eol}{pad}    let v = 1;{eol}{pad}}}"),
                "fn demo() {\n    let v = 1;\n}".to_string(),
                "fn demo() {\n    let v = 2;\n}".to_string(),
                format!("{pad}fn demo() {{{eol}{pad}    let v = 2;{eol}{pad}}}"),
            ),
            // Per-line indentation difference (LineTrimmed preserves mapped file indentation)
            _ => (
                format!("{pad}if ok {{{eol}{pad}    step_a();{eol}{pad}}}"),
                "if ok {\n  step_a();\n}".to_string(),
                "if ok {\n  step_b();\n}".to_string(),
                format!("{pad}if ok {{{eol}{pad}    step_b();{eol}{pad}}}"),
            ),
        };
        let prefix = if bom { "\u{feff}" } else { "" };
        let body = if duplicate {
            format!("header{eol}{block_in_file}{eol}mid{eol}{block_in_file}{eol}footer{eol}")
        } else {
            format!("header{eol}{block_in_file}{eol}footer{eol}")
        };
        let file = format!("{prefix}{body}");
        let res = apply_edits_for_test(&file, "demo.rs", &[(&old_query, &new_text)]);
        if duplicate {
            let err = res.expect_err("duplicate match must fail across all tolerance tiers");
            prop_assert!(err.contains("matches more than once"), "unexpected error: {err}");
        } else {
            let updated = res.expect("unique tolerant match must succeed");
            let expected = format!("{prefix}header{eol}{expected_replaced}{eol}footer{eol}");
            prop_assert_eq!(updated, expected);
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
        let mut target_id = String::new();
        let mut target_turn_id = String::new();
        for t in 0..turns {
            let tid = format!("turn_{t}");
            mgr.append_operation_started(tid.clone()).expect("start turn");
            let uid = format!("u_{t}");
            mgr.append_message_with_id(&uid, Message::user(format!("msg {t}")), None, None)
                .expect("user msg");
            mgr.append_message_with_id(
                &format!("a_{t}"),
                Message::assistant(format!("reply {t}"), String::new(), Vec::new(), None),
                None,
                None,
            )
            .expect("assistant msg");
            let mid_uid = format!("u_mid_{t}");
            if mid_turn && t == rewind_idx {
                mgr.append_message_with_id(&mid_uid, Message::user("steer"), None, None)
                    .expect("mid-turn user msg");
                target_id = format!("{mid_uid}:text:0");
                target_turn_id = tid.clone();
            } else if t == rewind_idx {
                target_id = format!("{uid}:text:0");
                target_turn_id = tid.clone();
            }
            if t == turns - 1 && reselect_after {
                mgr.append_metadata(SessionMetadata::ThreadSettings {
                    selection: ModelSelection {
                        provider: "p_latest".into(),
                        model: "m_latest".into(),
                        reasoning: Some("high".into()),
                    },
                })
                .expect("append latest settings");
            }
            if t == turns - 1 && rename_after {
                mgr.append_metadata(SessionMetadata::ThreadName { name: "Renamed Title".into() })
                    .expect("append latest name");
            }
            mgr.append_record(LedgerRecord::OperationFinished {
                turn_id: tid,
                outcome: protocol::TurnStatus::Completed,
                error: None,
                user_stopped: false,
            })
            .expect("finish turn");
        }

        mgr.rewind_before_message(&target_id).expect("rewind succeeds");
        let reloaded = SessionData::open(&sessions.join(session_file_name(&sid)), None)
            .expect("reopen after rewind");

        let raw_target = target_id.strip_suffix(":text:0").unwrap_or(&target_id);
        prop_assert!(!reloaded.entries().iter().any(|e| e.id() == raw_target), "rewound message must be removed");
        if mid_turn {
            prop_assert!(
                reloaded.entries().iter().any(|e| matches!(
                    e,
                    SessionEntry::Record {
                        record: LedgerRecord::OperationFinished {
                            turn_id,
                            outcome: protocol::TurnStatus::Completed,
                            ..
                        },
                        ..
                    } if turn_id == &target_turn_id
                )),
                "mid-turn rewind must cleanly close the preceding turn"
            );
        } else {
            prop_assert!(
                !reloaded.entries().iter().any(|e| e.id() == target_turn_id),
                "turn-start rewind must remove OperationStarted"
            );
        }

        let latest_sel = reloaded.entries().iter().rev().find_map(|e| match e {
            SessionEntry::Metadata { metadata: SessionMetadata::ThreadSettings { selection }, .. } => {
                Some(selection)
            }
            _ => None,
        });
        let expected_provider = if reselect_after { "p_latest" } else { "p0" };
        prop_assert_eq!(latest_sel.map(|s| s.provider.as_str()), Some(expected_provider));

        let latest_name = reloaded.entries().iter().rev().find_map(|e| match e {
            SessionEntry::Metadata { metadata: SessionMetadata::ThreadName { name }, .. } => {
                Some(name.as_str())
            }
            _ => None,
        });
        let expected_name = rename_after.then_some("Renamed Title");
        prop_assert_eq!(latest_name, expected_name);
    }
}

#[test]
fn edit_safety_invariants_and_long_diff_head_tail_spill() {
    // #1: trailing space in old_text must not prefix-match "key = 10" in BOM/CRLF file
    let bom_crlf = "\u{feff}key = 10\r\nother = 2\r\n";
    let err = apply_edits_for_test(bom_crlf, "cfg.ini", &[("key = 1 ", "key = 2")])
        .expect_err("trailing-space old_text must not corrupt key = 10");
    assert!(err.contains("was not found"), "unexpected error: {err}");
    let with_real_target = "\u{feff}key = 10\r\nkey = 1  \r\n";
    let updated = apply_edits_for_test(with_real_target, "cfg.ini", &[("key = 1   ", "key = 2")])
        .expect("must skip key = 10 and match actual key = 1 line");
    assert_eq!(updated, "\u{feff}key = 10\r\nkey = 2\r\n");

    // #2: Level 5 Trimmed preserves 4sp/8sp Python indentation when mapping is consistent,
    // and rejects contradictory or unknown indents explicitly.
    let py = "class C:\n    def f(self):\n        return 1\n";
    let py_ok =
        apply_edits_for_test(py, "app.py", &[("def f(self):\n  return 1", "def f(self):\n  return 2")])
            .expect("consistent Trimmed indent mapping must succeed");
    assert_eq!(py_ok, "class C:\n    def f(self):\n        return 2\n");

    let py_unknown =
        apply_edits_for_test(py, "app.py", &[("def f(self):\n  return 1", "def f(self):\n    return 2")])
            .expect_err("unknown new_text indent under Trimmed must fail explicitly");
    assert!(py_unknown.contains("indentation cannot be uniquely derived"), "got: {py_unknown}");

    let py_ambig = apply_edits_for_test("    a()\n        b()\n", "app.py", &[("a()\nb()", "a()\nc()")])
        .expect_err("contradictory indent mapping under Trimmed must fail explicitly");
    assert!(py_ambig.contains("indentation cannot be uniquely derived"), "got: {py_ambig}");

    let py_collapse = apply_edits_for_test(
        "        a = 1\n        b = 2\n",
        "app.py",
        &[("a = 1\n  b = 2", "if cond:\n  b = 2")],
    )
    .expect_err("collapsing distinct indents to same file indent must fail explicitly");
    assert!(py_collapse.contains("indentation cannot be uniquely derived"), "got: {py_collapse}");

    // Valid Python file with separate blocks using "    \t" and "        ": old_ind "    " -> file_ind "    \t"
    // would pass prefix-ordering alone and inject mixed tab+space before "        y = 2" (TabError); must reject.
    let py_tab_mix = apply_edits_for_test(
        "def f():\n    if a:\n    \tx = 1\n    if b:\n        y = 2\n",
        "app.py",
        &[("  if a:\n    x = 1\n  if b:", "  if a:\n    x = 1\n  if b:\n    z = 3")],
    )
    .expect_err("mixed space+tab indent on file side under Trimmed must fail explicitly");
    assert!(py_tab_mix.contains("indentation cannot be uniquely derived"), "got: {py_tab_mix}");

    // #3: 560-line diff produces Head+Tail + Full output spill, while display stays parseable
    let tmp = TempDir::new("diff-spill");
    let mut big_after = String::new();
    for i in 1..=560 {
        use std::fmt::Write as _;
        let _ = writeln!(big_after, "line {i}");
    }
    let exec = format_diff_result_for_test("Created", "big.txt", "", &big_after, &tmp.0);
    assert!(exec.content.starts_with("Created big.txt.\n+560 -0\n"));
    assert!(exec.content.contains("+line 1\n"), "missing diff head");
    assert!(exec.content.contains("[... "), "missing omission marker");
    assert!(exec.content.contains("+line 560"), "missing diff tail");
    let spill_path = exec.content.rsplit("Full output: ").next().expect("spill path").trim();
    let spilled = std::fs::read_to_string(spill_path).expect("read spilled diff");
    assert!(spilled.starts_with("+560 -0\n@@ -0,0 +1,560 @@\n+line 1\n"));
    assert!(spilled.contains("+line 560\n"));
    let display = exec.display.expect("display diff present").output;
    assert!(display.starts_with("+560 -0\n@@ -0,0 +1,560 @@\n"));
    assert!(display.ends_with("[Diff truncated]\n"));
}

#[test]
fn grep_and_glob_core_behavior_and_module_guidelines() {
    let tmp = TempDir::new("grep-glob");
    let root = &tmp.0;
    std::fs::create_dir_all(root.join(".git")).expect("create .git");
    std::fs::create_dir_all(root.join(".agents/skills")).expect("create .agents");
    std::fs::create_dir_all(root.join(".github/workflows")).expect("create .github");
    std::fs::create_dir_all(root.join(".secret")).expect("create .secret");
    std::fs::create_dir_all(root.join("src/nested")).expect("create src/nested");
    std::fs::create_dir_all(root.join("ignored_dir")).expect("create ignored_dir");

    std::fs::write(root.join(".gitignore"), "ignored_dir/\n*.log\n").expect("write .gitignore");
    std::fs::write(root.join(".git/config"), "needle_token in git\n").expect("write .git/config");
    std::fs::write(root.join(".agents/skills/demo.md"), "agent skill needle_token\n").expect("write skill");
    std::fs::write(root.join(".github/workflows/ci.yml"), "name: ci\n").expect("write ci.yml");
    std::fs::write(root.join(".secret/hidden.rs"), "fn hidden() {}\n").expect("write hidden.rs");
    std::fs::write(root.join("ignored_dir/skip.rs"), "needle_token ignored\n").expect("write ignored");
    std::fs::write(root.join("root.rs"), "fn root_fn() {}\n").expect("write root.rs");
    std::fs::write(
        root.join("src/alpha.rs"),
        "line1\nneedle_token alpha\nline3\nline4\nline5\nneedle_token beta\nline7\n",
    )
    .expect("write alpha.rs");
    let long_cjk = format!("needle_token {}", "汉".repeat(520));
    std::fs::write(root.join("src/nested/long.rs"), format!("{long_cjk}\nCASE_NEEDLE\n[literal](1)\n"))
        .expect("write long.rs");
    std::fs::write(root.join("src/bin.dat"), b"needle_token\0binary_payload").expect("write bin.dat");

    let signal = tokio_util::sync::CancellationToken::new();

    // glob: bare filename pattern matches across depths, respects .gitignore, skips hidden and .git
    let rs_files = execute_glob_for_test(
        &GlobArgs {
            pattern: "*.rs".into(),
            path: None,
            limit: None,
        },
        root,
        &signal,
    );
    assert!(!rs_files.is_error, "glob failed: {}", rs_files.content);
    assert!(rs_files.content.contains("root.rs"));
    assert!(rs_files.content.contains("src/alpha.rs"));
    assert!(rs_files.content.contains("src/nested/long.rs"));
    assert!(!rs_files.content.contains("ignored_dir"));
    assert!(!rs_files.content.contains(".secret"));
    assert!(!rs_files.content.contains(".git"));

    // glob: backslash normalization & limit cap
    let nested_rs = execute_glob_for_test(
        &GlobArgs {
            pattern: r"src\**\*.rs".into(),
            path: None,
            limit: Some(1),
        },
        root,
        &signal,
    );
    assert!(!nested_rs.is_error);
    assert!(nested_rs.content.contains("[Showing 1 of 2 matching files"));

    // glob: explicit hidden directory path & explicit dot-prefixed pattern, while still skipping .git
    let agents_md = execute_glob_for_test(
        &GlobArgs {
            pattern: "*.md".into(),
            path: Some(".agents".into()),
            limit: None,
        },
        root,
        &signal,
    );
    assert!(!agents_md.is_error);
    assert!(agents_md.content.contains(".agents/skills/demo.md"), "got: {}", agents_md.content);

    let github_files = execute_glob_for_test(
        &GlobArgs {
            pattern: ".github/**".into(),
            path: None,
            limit: None,
        },
        root,
        &signal,
    );
    assert!(!github_files.is_error);
    assert!(github_files.content.contains(".github/workflows/ci.yml"), "got: {}", github_files.content);

    let git_files = execute_glob_for_test(
        &GlobArgs {
            pattern: ".git/**".into(),
            path: None,
            limit: None,
        },
        root,
        &signal,
    );
    assert!(!git_files.is_error);
    assert_eq!(git_files.content, "No files found.");

    // #7: explicit .git path skipped in both glob and grep; non-Git dir still respects .gitignore
    let explicit_git_glob = execute_glob_for_test(
        &GlobArgs {
            pattern: "*".into(),
            path: Some(".git".into()),
            limit: None,
        },
        root,
        &signal,
    );
    assert!(!explicit_git_glob.is_error);
    assert_eq!(explicit_git_glob.content, "No files found.");
    let explicit_git_grep = execute_grep_for_test(
        &GrepArgs {
            pattern: "needle_token".into(),
            path: Some(".git/config".into()),
            glob: None,
            ignore_case: false,
            literal: false,
            context: None,
            limit: None,
        },
        root,
        &signal,
    );
    assert!(!explicit_git_grep.is_error);
    assert_eq!(explicit_git_grep.content, "No matches found.");

    let nongit = TempDir::new("nongit-ignore");
    std::fs::create_dir_all(nongit.0.join("ignored_sub")).expect("create ignored_sub");
    std::fs::write(nongit.0.join(".gitignore"), "ignored_sub/\n").expect("write .gitignore");
    std::fs::write(nongit.0.join("ignored_sub/a.rs"), "needle_token\n").expect("write ignored");
    std::fs::write(nongit.0.join("kept.rs"), "needle_token\n").expect("write kept");
    let nongit_glob = execute_glob_for_test(
        &GlobArgs {
            pattern: "*.rs".into(),
            path: None,
            limit: None,
        },
        &nongit.0,
        &signal,
    );
    assert!(nongit_glob.content.contains("kept.rs") && !nongit_glob.content.contains("ignored_sub"));
    let nongit_grep = execute_grep_for_test(
        &GrepArgs {
            pattern: "needle_token".into(),
            path: None,
            glob: None,
            ignore_case: false,
            literal: false,
            context: None,
            limit: None,
        },
        &nongit.0,
        &signal,
    );
    assert!(nongit_grep.content.contains("kept.rs") && !nongit_grep.content.contains("ignored_sub"));

    // #4: ^find-me$ matches CRLF, LF, and mixed line endings
    std::fs::write(root.join("src/crlf.txt"), "pre\r\nfind-me\r\npost\r\n").expect("crlf");
    std::fs::write(root.join("src/lf.txt"), "pre\nfind-me\npost\n").expect("lf");
    std::fs::write(root.join("src/mixed.txt"), "pre\r\nfind-me\npost\r\n").expect("mixed");
    let anchored = execute_grep_for_test(
        &GrepArgs {
            pattern: "^find-me$".into(),
            path: Some("src".into()),
            glob: Some("*.txt".into()),
            ignore_case: false,
            literal: false,
            context: None,
            limit: None,
        },
        root,
        &signal,
    );
    assert!(!anchored.is_error, "anchored grep failed: {}", anchored.content);
    assert!(anchored.content.contains("src/crlf.txt:2: find-me"), "got: {}", anchored.content);
    assert!(anchored.content.contains("src/lf.txt:2: find-me"), "got: {}", anchored.content);
    assert!(anchored.content.contains("src/mixed.txt:2: find-me"), "got: {}", anchored.content);

    // #5: CancellableReader aborts mid-read during searcher.search_reader
    struct CancelAfterFirstChunk {
        signal: tokio_util::sync::CancellationToken,
        reads: usize,
    }
    impl std::io::Read for CancelAfterFirstChunk {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            if self.reads == 1 {
                self.signal.cancel();
                let data = b"no match here\n";
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                return Ok(n);
            }
            panic!("CancellableReader must abort before calling inner reader again");
        }
    }
    let mid_cancel = tokio_util::sync::CancellationToken::new();
    let cancel_err = search_reader_with_cancel_for_test(
        CancelAfterFirstChunk { signal: mid_cancel.clone(), reads: 0 },
        "needle",
        &mid_cancel,
    )
    .expect_err("mid-read cancel must abort search_reader");
    assert_eq!(cancel_err.kind(), std::io::ErrorKind::Other);

    // Pre-first-read cancel must not spin in grep-searcher BOM detection
    let pre_cancel = tokio_util::sync::CancellationToken::new();
    pre_cancel.cancel();
    let pre_err =
        search_reader_with_cancel_for_test(std::io::Cursor::new(b"hello world\n"), "needle", &pre_cancel)
            .expect_err("pre-first-read cancel must abort immediately");
    assert_eq!(pre_err.kind(), std::io::ErrorKind::Other);

    // grep: directory search skips binary/ignored/.git, truncates >500 UTF-8 chars
    let grep_dir = execute_grep_for_test(
        &GrepArgs {
            pattern: "needle_token".into(),
            path: None,
            glob: None,
            ignore_case: false,
            literal: false,
            context: None,
            limit: None,
        },
        root,
        &signal,
    );
    assert!(!grep_dir.is_error, "grep failed: {}", grep_dir.content);
    assert!(grep_dir.content.contains("src/alpha.rs:2: needle_token alpha"));
    assert!(grep_dir.content.contains("src/nested/long.rs:1: "));
    assert!(grep_dir.content.contains("…[truncated]"), "long UTF-8 line must truncate");
    assert!(!grep_dir.content.contains("bin.dat"), "binary file must be skipped");
    assert!(!grep_dir.content.contains("ignored_dir"), "gitignored file must be skipped");
    assert!(!grep_dir.content.contains(".git/config"), ".git must be skipped");

    // grep: single file path bypasses .gitignore & hidden filtering
    let single_ignored = execute_grep_for_test(
        &GrepArgs {
            pattern: "needle_token".into(),
            path: Some("ignored_dir/skip.rs".into()),
            glob: None,
            ignore_case: false,
            literal: false,
            context: None,
            limit: None,
        },
        root,
        &signal,
    );
    assert!(!single_ignored.is_error);
    assert!(single_ignored.content.contains("ignored_dir/skip.rs:1: needle_token ignored"));

    // grep: context lines with '--', literal, ignore_case, glob filter, limit, no match, invalid regex
    let ctx_res = execute_grep_for_test(
        &GrepArgs {
            pattern: "needle_token".into(),
            path: Some("src/alpha.rs".into()),
            glob: None,
            ignore_case: false,
            literal: false,
            context: Some(1),
            limit: None,
        },
        root,
        &signal,
    );
    assert!(!ctx_res.is_error);
    assert!(ctx_res.content.contains("src/alpha.rs-1- line1"));
    assert!(ctx_res.content.contains("src/alpha.rs:2: needle_token alpha"));
    assert!(ctx_res.content.contains("\n--\n"), "context blocks must be separated by --");
    assert!(ctx_res.content.contains("src/alpha.rs:6: needle_token beta"));

    let lit_ci = execute_grep_for_test(
        &GrepArgs {
            pattern: "[LITERAL](1)".into(),
            path: None,
            glob: Some("src/**/*.rs".into()),
            ignore_case: true,
            literal: true,
            context: None,
            limit: Some(1),
        },
        root,
        &signal,
    );
    assert!(!lit_ci.is_error);
    assert!(lit_ci.content.contains("src/nested/long.rs:3: [literal](1)"));

    let no_match = execute_grep_for_test(
        &GrepArgs {
            pattern: "definitely_absent_xyz".into(),
            path: None,
            glob: None,
            ignore_case: false,
            literal: false,
            context: None,
            limit: None,
        },
        root,
        &signal,
    );
    assert!(!no_match.is_error);
    assert_eq!(no_match.content, "No matches found.");

    let bad_regex = execute_grep_for_test(
        &GrepArgs {
            pattern: "[unclosed".into(),
            path: None,
            glob: None,
            ignore_case: false,
            literal: false,
            context: None,
            limit: None,
        },
        root,
        &signal,
    );
    assert!(bad_regex.is_error);

    // cancellation token honored immediately
    let cancelled = tokio_util::sync::CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        execute_glob_for_test(
            &GlobArgs {
                pattern: "*.rs".into(),
                path: None,
                limit: None
            },
            root,
            &cancelled,
        )
        .content,
        ABORTED_MESSAGE
    );
    assert_eq!(
        execute_grep_for_test(
            &GrepArgs {
                pattern: "needle".into(),
                path: None,
                glob: None,
                ignore_case: false,
                literal: false,
                context: None,
                limit: None,
            },
            root,
            &cancelled,
        )
        .content,
        ABORTED_MESSAGE
    );

    // modules::assemble dynamic prompt guidelines for all 4 (grep, glob) combinations
    let cases: [(&[&str], &str, &str); 4] = [
        (&[], "Use grep for content searches and glob for file discovery", "Use bash with rg"),
        (
            &["glob"],
            "Use grep for content searches; use bash with rg --files",
            "Use glob for file discovery",
        ),
        (
            &["grep"],
            "Use glob for file discovery; use bash with rg for content searches",
            "Use grep for content searches",
        ),
        (
            &["grep", "glob"],
            "Use bash with rg for content searches and rg --files",
            "Use grep for content searches",
        ),
    ];
    for (disabled, must_contain, must_not_contain) in cases {
        let disabled_vec: Vec<String> = disabled.iter().map(|s| (*s).to_owned()).collect();
        let assembly = assemble_modules(ModuleInputs {
            cwd: "C:/workspace",
            instructions: Vec::new(),
            skills: app_core::skills::SkillCatalog::default(),
            mcp: mcp::McpDiscovery::default(),
            web: None,
            disabled: &disabled_vec,
        });
        let prompt = assembly.messages[0].content_text();
        assert!(
            prompt.contains(must_contain),
            "disabled={disabled:?} prompt missing expected guideline: {prompt}"
        );
        assert!(
            !prompt.contains(must_not_contain),
            "disabled={disabled:?} prompt had contradictory guideline: {prompt}"
        );
    }
}

#[test]
fn idle_tool_result_clearing_projection_usage_and_rewind_invariants() {
    assert_eq!(
        app_core::iso_elapsed_secs("2026-01-01T00:00:00.000Z", "2026-01-01T00:09:59.999Z"),
        Some(599)
    );
    assert_eq!(
        app_core::iso_elapsed_secs("2026-01-01T00:00:00.000Z", "2026-01-01T00:10:00.000Z"),
        Some(600)
    );
    assert_eq!(
        app_core::iso_elapsed_secs("2026-01-01T00:10:00.000Z", "2026-01-01T00:00:00.000Z"),
        Some(-600)
    );

    let (_tmp, sessions, sid, mut mgr) = new_test_session("idle-clear", "pa", "ma");
    let big_body = "x".repeat(2200);
    let spill_body = format!(
        "{}\nFull output: C:/tmp/out.log\n[Additional background hint after path]",
        "y".repeat(2200)
    );
    let make_obs = |input_tokens: u64| RequestObservation {
        purpose: RequestPurpose::Generation,
        provider: "pa".into(),
        model: "ma".into(),
        status: ProviderAttemptStatus::Ok,
        ttft_ms: None,
        decode_ms: None,
        usage: Some(ModelUsage {
            input_tokens,
            output_tokens: 100,
            total_tokens: input_tokens + 100,
            cached_input_tokens: None,
        }),
    };

    let append_tool_step = |mgr: &mut SessionManager,
                            step: &str,
                            tool_name: &str,
                            content: Vec<ContentBlock>,
                            obs: Option<RequestObservation>| {
        let call_id = format!("call_{step}");
        mgr.append_message_with_id(
            &format!("a_{step}"),
            Message::Assistant {
                content: vec![ContentBlock::ToolCall(ModelToolCall {
                    tool_call_id: call_id.clone(),
                    tool_name: tool_name.into(),
                    arguments: json!({}),
                    metadata: None,
                })],
                provider_metadata: None,
            },
            obs,
            None,
        )
        .expect("append assistant");
        mgr.append_message_with_id(
            &format!("r_{step}"),
            Message::ToolResult {
                content,
                tool_call_id: call_id,
                is_error: false,
            },
            None,
            None,
        )
        .expect("append tool result");
    };

    mgr.append_operation_started("t1".into()).expect("start t1");
    mgr.append_message_with_id("u1", Message::user("start"), None, None).expect("u1");

    // Older entries beyond the protected tail of 5 whitelist results:
    // 1. bash with spill path + trailing hint -> SHOULD clear & preserve spill path
    append_tool_step(&mut mgr, "1_bash", "bash", vec![ContentBlock::text(spill_body)], None);
    // 2. mcp_call with short text + image (image does NOT exist on disk!) -> SHOULD clear & drop image block
    append_tool_step(
        &mut mgr,
        "2_mcp_img",
        "mcp_call",
        vec![
            ContentBlock::text("ok"),
            ContentBlock::Image(protocol::ImageAttachment {
                id: "missing_cleared_img.png".into(),
                name: "shot.png".into(),
                mime_type: "image/png".into(),
                width: 1024,
                height: 768,
            }),
        ],
        None,
    );
    // 3. small read (< 2048B, no image) -> SHOULD NOT clear
    append_tool_step(&mut mgr, "3_read_small", "read", vec![ContentBlock::text("short")], None);
    // 4. large edit & view_image & skill & todo (never-clear tools) -> MUST NEVER clear
    append_tool_step(&mut mgr, "4_edit_big", "edit", vec![ContentBlock::text(big_body.clone())], None);
    append_tool_step(&mut mgr, "5_skill_big", "skill", vec![ContentBlock::text(big_body.clone())], None);
    append_tool_step(&mut mgr, "6_view_img", "view_image", vec![ContentBlock::text(big_body.clone())], None);
    append_tool_step(&mut mgr, "7_todo_big", "todo", vec![ContentBlock::text(big_body.clone())], None);

    // Recent 5 whitelist results (must all be protected)
    for (idx, tool) in ["read", "grep", "glob", "web_search", "web_fetch"].iter().enumerate() {
        append_tool_step(
            &mut mgr,
            &format!("recent_{idx}"),
            tool,
            vec![ContentBlock::text(big_body.clone())],
            None,
        );
    }
    // Trailing 6 edits must NOT push out any of the 5 protected whitelist results!
    for idx in 0..6 {
        append_tool_step(
            &mut mgr,
            &format!("tail_edit_{idx}"),
            "edit",
            vec![ContentBlock::text(big_body.clone())],
            (idx == 5).then(|| make_obs(50_000)),
        );
    }

    let last_ts = mgr.data().entries().last().expect("last entry").timestamp().to_owned();
    // < 600s idle and clock rollback must NOT trigger
    assert!(select_idle_clearable_entry_ids(mgr.data(), &last_ts).is_empty());
    assert!(select_idle_clearable_entry_ids(mgr.data(), "2000-01-01T00:00:00.000Z").is_empty());

    // >= 600s idle selects ONLY r_1_bash and r_2_mcp_img
    let to_clear = select_idle_clearable_entry_ids(mgr.data(), "2099-01-01T00:00:00.000Z");
    assert_eq!(to_clear, vec!["r_1_bash".to_string(), "r_2_mcp_img".to_string()]);

    let active_before = resolve_context_entries(mgr.data());
    let tokens_before = input_tokens(mgr.data(), &active_before, ("pa", "ma"), 300);
    assert!(tokens_before >= 50_000, "pre-clear uses reported usage: {tokens_before}");

    mgr.append_record(LedgerRecord::ToolResultsCleared { entry_ids: to_clear })
        .expect("append ToolResultsCleared");

    // Re-running after clearing must be idempotent (empty -> no duplicate record)
    assert!(select_idle_clearable_entry_ids(mgr.data(), "2099-01-01T00:00:00.000Z").is_empty());

    // Usage truncation: stale 50k usage must be dropped and replaced by smaller estimate
    let active_after = resolve_context_entries(mgr.data());
    let tokens_after = input_tokens(mgr.data(), &active_after, ("pa", "ma"), 300);
    assert!(tokens_after < 20_000, "post-clear tokens must drop stale usage: {tokens_after}");

    // Projection replaces cleared tool results, preserves Full output path, drops Image blocks so load_images succeeds
    let mut prepared = PreparedContext::new(mgr.data(), &active_after, tokens_after);
    assert_balanced_tool_calls(&prepared.messages);
    let bash_msg = prepared
        .messages
        .iter()
        .find(|m| m.tool_call_id() == Some("call_1_bash"))
        .expect("bash tool result");
    assert_eq!(
        bash_msg.content_text(),
        "[Old tool result cleared after idle gap. Full output: C:/tmp/out.log]"
    );
    let mcp_msg = prepared
        .messages
        .iter()
        .find(|m| m.tool_call_id() == Some("call_2_mcp_img"))
        .expect("mcp tool result");
    assert_eq!(mcp_msg.images().count(), 0, "cleared tool result must not keep image blocks");
    assert!(mcp_msg.content_text().contains("Old tool result cleared after idle gap"));
    let loaded = load_images(&mut prepared.messages, &prepared.image_directory)
        .expect("load_images must not read disk for cleared images");
    assert!(loaded.is_empty());

    // Cleared IDs survive across Compaction when first_kept_entry_id retains pre-compaction entries
    mgr.append_compaction_with_id(
        "cp_1",
        CompactionEntry {
            summary: "summary".into(),
            first_kept_entry_id: Some("a_1_bash".into()),
        },
    )
    .expect("append compaction");
    let post_cp = resolve_context_entries(mgr.data());
    let post_cp_msgs = project_messages(&post_cp, &cleared_tool_result_ids(mgr.data()));
    let post_cp_bash = post_cp_msgs
        .iter()
        .find(|m| m.tool_call_id() == Some("call_1_bash"))
        .expect("kept bash result");
    assert!(post_cp_bash.content_text().contains("Full output: C:/tmp/out.log"));

    // Raw log entry stays intact & rewind before a later turn drops post-cut clearing records safely
    mgr.append_operation_started("t2".into()).expect("start t2");
    mgr.append_message_with_id("u2", Message::user("second turn"), None, None).expect("u2");
    mgr.append_record(LedgerRecord::ToolResultsCleared { entry_ids: vec!["r_recent_0".into()] })
        .expect("append post-u2 clear");
    mgr.rewind_before_message("u2:text:0").expect("rewind before u2");
    let reopened =
        SessionData::open(&sessions.join(session_file_name(&sid)), Some(&sid)).expect("reopen after rewind");
    let cleared_after_rewind = cleared_tool_result_ids(&reopened);
    assert!(cleared_after_rewind.contains("r_1_bash"));
    assert!(!cleared_after_rewind.contains("r_recent_0"));
    assert!(reopened.entries().iter().any(|entry| {
        matches!(
            entry,
            SessionEntry::Message { id, message, .. }
                if id == "r_1_bash" && message.content_text().len() >= 2200
        )
    }));

    // #8: manual Compact precheck runs idle cold-cache clearing before checking compactable prefix
    let (_tmp2, sessions2, sid2, mut mgr2) = new_test_session("manual-compact-idle-clear", "pa", "ma");
    mgr2.append_operation_started("t1".into()).expect("start t1");
    mgr2.append_message_with_id("u1", Message::user("start"), None, None).expect("u1");
    append_tool_step(&mut mgr2, "old_bash", "bash", vec![ContentBlock::text(big_body.clone())], None);
    for idx in 0..5 {
        append_tool_step(
            &mut mgr2,
            &format!("keep_{idx}"),
            "read",
            vec![ContentBlock::text(big_body.clone())],
            (idx == 4).then(|| make_obs(40_000)),
        );
    }
    let session_path2 = sessions2.join(session_file_name(&sid2));
    drop(mgr2);
    let raw = std::fs::read_to_string(&session_path2).expect("read session");
    let year_prefix = &app_core::now_iso()[..5];
    let backdated = raw.replace(year_prefix, "2020-");
    std::fs::write(&session_path2, backdated).expect("write backdated session");
    let mut mgr2 = SessionManager::open_existing(&session_path2, &sid2).expect("reopen backdated");
    let _ = precheck_manual_compaction(&mut mgr2, 100_000, ("pa", "ma"))
        .expect("manual compact precheck succeeds");
    assert!(
        cleared_tool_result_ids(mgr2.data()).contains("r_old_bash"),
        "manual compact precheck must clear eligible idle tool results"
    );
}

#[test]
fn todo_validation_execution_and_module_gating() {
    let enabled_assembly = assemble_modules(ModuleInputs {
        cwd: "C:/workspace",
        instructions: Vec::new(),
        skills: app_core::skills::SkillCatalog::default(),
        mcp: mcp::McpDiscovery::default(),
        web: None,
        disabled: &[],
    });
    assert!(
        enabled_assembly.modules.iter().any(|m| m.id == "todo" && m.enabled),
        "todo module must be enabled by default"
    );
    assert!(
        enabled_assembly.registry.provider_schemas().iter().any(|s| s.name == "todo"),
        "todo tool schema must be exposed when enabled"
    );
    assert!(
        enabled_assembly.messages[0].content_text().contains("Use todo to track progress"),
        "todo guideline must be injected when enabled"
    );

    let Ok(PreparedTool::Todo(valid_args)) = enabled_assembly.registry.preflight(
        "todo",
        &json!({
            "todos": [
                { "content": "step 1", "status": "completed" },
                { "content": "step 2", "status": "completed" },
                { "content": "step 3", "status": "in_progress" },
                { "content": "step 4", "status": "pending" }
            ]
        }),
    ) else {
        panic!("valid todo args must preflight");
    };
    let updated = execute_todo_for_test(&valid_args);
    assert!(!updated.is_error);
    assert_eq!(updated.content, "Updated todo list (2/4 completed, 1 in progress).");

    let Ok(PreparedTool::Todo(empty_args)) =
        enabled_assembly.registry.preflight("todo", &json!({ "todos": [] }))
    else {
        panic!("empty todo list must preflight");
    };
    let cleared = execute_todo_for_test(&empty_args);
    assert!(!cleared.is_error);
    assert_eq!(cleared.content, "Cleared todo list.");

    for invalid in [
        json!({ "todos": [{ "content": "   ", "status": "pending" }] }),
        json!({
            "todos": [
                { "content": "a", "status": "in_progress" },
                { "content": "b", "status": "in_progress" }
            ]
        }),
        json!({ "todos": [{ "content": "a", "status": "pending", "extra": 1 }] }),
    ] {
        let err = enabled_assembly
            .registry
            .preflight("todo", &invalid)
            .err()
            .expect("invalid todo args must be rejected");
        assert!(err.is_error);
    }

    let disabled = vec!["todo".to_string()];
    let disabled_assembly = assemble_modules(ModuleInputs {
        cwd: "C:/workspace",
        instructions: Vec::new(),
        skills: app_core::skills::SkillCatalog::default(),
        mcp: mcp::McpDiscovery::default(),
        web: None,
        disabled: &disabled,
    });
    assert!(
        disabled_assembly.modules.iter().any(|m| m.id == "todo" && !m.enabled),
        "todo module must respect disabled_modules"
    );
    assert!(
        !disabled_assembly.registry.provider_schemas().iter().any(|s| s.name == "todo"),
        "disabled todo tool must not be exposed"
    );
    assert!(
        !disabled_assembly.messages[0].content_text().contains("Use todo to track progress"),
        "disabled todo module must not inject guideline"
    );
    assert!(disabled_assembly.registry.preflight("todo", &json!({ "todos": [] })).is_err());
}
