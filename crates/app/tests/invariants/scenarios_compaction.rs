use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use serde_json::{Value, json};

use crate::harness::{
    BackendClient, DEFAULT_TIMEOUT, MockModelServer, ScriptedResponse, TINY_PNG_DATA_URL, TestEnv,
    find_tool_result, messages_text_sse, messages_tools_sse, responses_sse, write_test_mcp_script,
};

const COMPACT_CONTEXT_WINDOW: u32 = 20_000;
const USER_TURN_TAG: &str = "[TEST_USER]";

fn is_compaction_request(body: &Value) -> bool {
    let Some(last_msg) = body.get("messages").and_then(Value::as_array).and_then(|msgs| msgs.last()) else {
        return false;
    };
    if last_msg.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(blocks) = last_msg.get("content").and_then(Value::as_array) else {
        return false;
    };
    let Some(last_block) = blocks.last() else {
        return false;
    };
    if last_block.get("type").and_then(Value::as_str) != Some("text") {
        return false;
    }
    last_block
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.contains(USER_TURN_TAG))
}

fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn backdate_session_timestamps(path: &Path, iso: &str) {
    let raw = std::fs::read_to_string(path).expect("read session jsonl");
    let mut lines = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut entry: Value = serde_json::from_str(line).expect("parse session jsonl line");
        if let Some(obj) = entry.as_object_mut()
            && obj.contains_key("timestamp")
        {
            obj.insert("timestamp".to_string(), Value::String(iso.to_string()));
        }
        lines.push(serde_json::to_string(&entry).expect("serialize backdated entry"));
    }
    let rewritten = format!("{}\n", lines.join("\n"));
    std::fs::write(path, rewritten).expect("write backdated session jsonl");
}

#[tokio::test]
async fn context_pressure_and_overflow_invariants() {
    let env = TestEnv::new("context-invariants");
    let server = MockModelServer::start().await;
    let ws = env.workspace_str();
    let mcp_script = write_test_mcp_script(&env.home);

    let overflow_req_len = Arc::new(AtomicUsize::new(0));
    let overflow_len_recorder = Arc::clone(&overflow_req_len);
    let recovered_req_len = Arc::new(AtomicUsize::new(0));
    let recovered_len_recorder = Arc::clone(&recovered_req_len);
    let compaction_success_count = Arc::new(AtomicUsize::new(0));
    let compaction_ok_counter = Arc::clone(&compaction_success_count);
    let normal_step = Arc::new(AtomicUsize::new(0));
    let step_counter = Arc::clone(&normal_step);

    server.set_fallback(move |req, _| {
        let serialized = req.body.to_string();
        if is_compaction_request(&req.body) {
            compaction_ok_counter.fetch_add(1, Ordering::SeqCst);
            let summary = if serialized.contains("USER_ORIGIN_MARKER_ROUND_1")
                && !serialized.contains("FIRST_CHECKPOINT_MARKER")
            {
                "FIRST_CHECKPOINT_MARKER: initial work condensed."
            } else {
                "SECOND_CHECKPOINT_UNIQUE_MARKER: compacted prior heavy turns cleanly."
            };
            return ScriptedResponse::sse(messages_text_sse(None, summary, 1200, 0, 60, "end_turn"));
        }

        let step = step_counter.fetch_add(1, Ordering::SeqCst);
        if step == 0 {
            return ScriptedResponse::sse(messages_tools_sse(
                Some("gathering initial large context"),
                vec![(
                    "t0",
                    "mcp_call",
                    json!({ "server": "probe", "tool": "large_output", "arguments": { "kb": 20 } }),
                )],
                1500,
                100,
                "tool_use",
            ));
        }
        if step == 1 {
            return ScriptedResponse::sse(messages_text_sse(
                None,
                "initial multi-tool work complete",
                12_000,
                0,
                80,
                "end_turn",
            ));
        }

        if serialized.contains("LATEST_INTENT_MUST_SURVIVE")
            && !serialized.contains("FIRST_CHECKPOINT_MARKER")
            && serialized.len() > 20_000
        {
            overflow_len_recorder.store(serialized.len(), Ordering::SeqCst);
            return ScriptedResponse::json(
                400,
                json!({
                    "error": {
                        "code": "context_length_exceeded",
                        "message": "maximum context length exceeded"
                    }
                }),
            );
        }

        if serialized.contains("LATEST_INTENT_MUST_SURVIVE") {
            recovered_len_recorder.store(serialized.len(), Ordering::SeqCst);
        }

        ScriptedResponse::sse(messages_text_sse(
            None,
            "RECOVERED_UNDER_CONTEXT_PRESSURE",
            1500,
            0,
            40,
            "end_turn",
        ))
    });

    let mut client = BackendClient::spawn(&env.home).await;
    client
        .setup_workspace_and_provider(
            &ws,
            "local",
            "messages",
            &server.base_url,
            "compact-model",
            COMPACT_CONTEXT_WINDOW,
            2048,
        )
        .await;
    client.register_test_mcp(&ws, &mcp_script).await;

    let session_id = client
        .start_session(
            &ws,
            "local",
            "compact-model",
            None,
            &format!("{USER_TURN_TAG} USER_ORIGIN_MARKER_ROUND_1"),
            vec![json!({ "name": "img.png", "dataUrl": TINY_PNG_DATA_URL })],
            vec!["skills"],
        )
        .await;
    let s1 = client.wait_settled(&session_id).await;
    assert_eq!(s1.pointer("/summary/status").and_then(Value::as_str), Some("completed"));

    client.submit(&session_id, &format!("{USER_TURN_TAG} LATEST_INTENT_MUST_SURVIVE")).await;
    let s2 = client.wait_settled(&session_id).await;
    assert_eq!(
        s2.pointer("/summary/status").and_then(Value::as_str),
        Some("completed"),
        "context overflow recovery must converge without deadlock"
    );

    let reqs = server.model_requests();
    let final_req_text = reqs.last().expect("final request").body.to_string();
    assert!(
        final_req_text.contains("LATEST_INTENT_MUST_SURVIVE"),
        "overflow recovery must keep the last balanced unit verbatim"
    );
    assert!(
        final_req_text.contains("FIRST_CHECKPOINT_MARKER"),
        "overflow recovery must compact prefix before the last unit"
    );
    let overflow_len = overflow_req_len.load(Ordering::SeqCst);
    let recovered_len = recovered_req_len.load(Ordering::SeqCst);
    assert!(overflow_len > 0, "Turn 2 must genuinely trigger upstream context_length_exceeded");
    assert!(
        recovered_len > 0 && recovered_len < overflow_len,
        "recovered request must be strictly smaller than overflowed request (overflow={overflow_len}, recovered={recovered_len})"
    );

    for idx in 0..5 {
        let heavy =
            format!("{USER_TURN_TAG} HEAVY_POST_COMPACTION_TURN_{idx}: {}", "payload_segment_".repeat(320));
        client.submit(&session_id, &heavy).await;
        let settled = client.wait_settled(&session_id).await;
        assert_eq!(settled.pointer("/summary/status").and_then(Value::as_str), Some("completed"));
    }

    server.enqueue(ScriptedResponse::sse(messages_text_sse(None, "   ", 500, 0, 5, "end_turn")));
    client.rpc_ok("session.compact", json!({ "sessionId": session_id })).await;
    let compact_settled = client.wait_settled(&session_id).await;
    assert!(
        compact_settled.pointer("/payload/failure").is_some_and(|v| !v.is_null()),
        "manual compaction with empty output must fail explicitly"
    );

    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "STILL_USABLE_AFTER_FAILED_COMPACTION",
        600,
        0,
        20,
        "end_turn",
    )));
    client.submit(&session_id, &format!("{USER_TURN_TAG} after failed compaction")).await;
    let s3 = client.wait_settled(&session_id).await;
    assert_eq!(s3.pointer("/summary/status").and_then(Value::as_str), Some("completed"));

    let compact_before = compaction_success_count.load(Ordering::SeqCst);
    client.rpc_ok("session.compact", json!({ "sessionId": session_id })).await;
    let compact2_settled = client.wait_settled(&session_id).await;
    assert!(
        compact2_settled.pointer("/payload/failure").is_none_or(Value::is_null),
        "repeated manual compaction must succeed"
    );
    assert!(
        compaction_success_count.load(Ordering::SeqCst) > compact_before,
        "repeated compaction must invoke prefix-replay compaction"
    );
    client
        .submit(&session_id, &format!("{USER_TURN_TAG} after retained tail compaction"))
        .await;
    let s4 = client.wait_settled(&session_id).await;
    assert_eq!(s4.pointer("/summary/status").and_then(Value::as_str), Some("completed"));
    let post_compaction_req =
        server.model_requests().last().expect("post compaction request").body.to_string();
    assert!(
        post_compaction_req.contains("SECOND_CHECKPOINT_UNIQUE_MARKER"),
        "second compaction must replace previous checkpoint with unique marker"
    );
    assert!(
        !post_compaction_req.contains("FIRST_CHECKPOINT_MARKER"),
        "older checkpoint must be replaced after second compaction"
    );
    assert!(
        !post_compaction_req.contains("HEAVY_POST_COMPACTION_TURN_0"),
        "compacted old prefix turn must not reappear after compaction"
    );
    assert!(
        post_compaction_req.contains("STILL_USABLE_AFTER_FAILED_COMPACTION"),
        "retained 15% tail must survive verbatim after compaction"
    );
    server.assert_no_protocol_violations();

    client.shutdown().await;
    let mut restarted = BackendClient::spawn(&env.home).await;
    let history = restarted.rpc_ok("session.read", json!({ "sessionId": session_id })).await;
    assert!(
        history.get("context").is_some_and(Value::is_null),
        "freshly reopened idle session must lazily defer context inspection"
    );
    let history_text = history.to_string();
    assert!(history_text.contains("USER_ORIGIN_MARKER_ROUND_1"));
    assert!(history_text.contains("LATEST_INTENT_MUST_SURVIVE"));
    assert!(history_text.contains("STILL_USABLE_AFTER_FAILED_COMPACTION"));

    let _ = restarted.drain_events();
    restarted.rpc_ok("session.context", json!({ "sessionId": session_id })).await;
    restarted
        .wait_event(DEFAULT_TIMEOUT, |frame| {
            frame.get("sessionId").and_then(Value::as_str) == Some(session_id.as_str())
                && frame.pointer("/payload/method").and_then(Value::as_str) == Some("context/loaded")
        })
        .await;
    let after_context = restarted.rpc_ok("session.read", json!({ "sessionId": session_id })).await;
    let modules = after_context.get("context").and_then(Value::as_array).expect("loaded context modules");
    assert!(!modules.is_empty(), "session.context must populate context modules");
    assert!(
        modules.iter().any(|m| {
            m.get("group").and_then(Value::as_str) == Some("tools")
                && m.get("enabled").and_then(Value::as_bool) == Some(true)
                && m.get("tokens").and_then(Value::as_u64).unwrap_or(0) > 0
        }),
        "session.context must report enabled tools with non-zero token estimate: {modules:?}"
    );
    assert!(
        modules.iter().all(|m| {
            m.get("id").and_then(Value::as_str) != Some("skills")
                || m.get("enabled").and_then(Value::as_bool) == Some(false)
        }),
        "session.context must reflect disabledModules for skills: {modules:?}"
    );

    let baseline_input = u64::from(COMPACT_CONTEXT_WINDOW) * 85 / 100;
    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "RECOVERED_UNDER_CONTEXT_PRESSURE",
        baseline_input,
        0,
        40,
        "end_turn",
    )));
    restarted.submit(&session_id, &format!("{USER_TURN_TAG} POST_RESTART_CONTINUATION")).await;
    let s_restart = restarted.wait_settled(&session_id).await;
    assert_eq!(s_restart.pointer("/summary/status").and_then(Value::as_str), Some("completed"));
    let restart_req = server.model_requests().last().expect("post restart request").body.to_string();
    assert!(
        restart_req.contains("SECOND_CHECKPOINT_UNIQUE_MARKER")
            && restart_req.contains("STILL_USABLE_AFTER_FAILED_COMPACTION")
            && !restart_req.contains("HEAVY_POST_COMPACTION_TURN_0"),
        "restarted session generation must project persisted second checkpoint and retained tail"
    );

    server.enqueue(ScriptedResponse::sse(messages_text_sse(None, "   ", 500, 0, 5, "end_turn")));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "AUTO_COMPACTION_FAIL_STILL_COMPLETED",
        1000,
        0,
        20,
        "end_turn",
    )));
    let reqs_before_pressure = server.model_requests().len();
    let pressure_ascii_bytes = (COMPACT_CONTEXT_WINDOW as usize * 8 / 100) * 4;
    let trigger_turn = format!("{USER_TURN_TAG} AUTO_PRESSURE_TRIGGER: {}", "z".repeat(pressure_ascii_bytes));
    restarted.submit(&session_id, &trigger_turn).await;
    let s_auto_fail = restarted.wait_settled(&session_id).await;
    assert_eq!(
        s_auto_fail.pointer("/summary/status").and_then(Value::as_str),
        Some("completed"),
        "auto compaction failure under context pressure must warn and continue generation without aborting turn"
    );
    let post_pressure_reqs = server.model_requests();
    let post_pressure_slice = &post_pressure_reqs[reqs_before_pressure..];
    assert_eq!(
        post_pressure_slice.len(),
        2,
        "high-pressure auto-compaction failure must issue 1 compaction request and 1 main generation request"
    );
    assert!(
        is_compaction_request(&post_pressure_slice[0].body),
        "first request under high context pressure must be automatic compaction"
    );
    assert!(
        !is_compaction_request(&post_pressure_slice[1].body)
            && post_pressure_slice[1].body.to_string().contains("AUTO_PRESSURE_TRIGGER"),
        "main generation request must proceed immediately after automatic compaction failure"
    );

    server.assert_no_protocol_violations();
    restarted.shutdown().await;
}

#[tokio::test]
async fn idle_tool_result_clearing_app_server_invariants() {
    let env = TestEnv::new("idle-clear-e2e");
    let server = MockModelServer::start().await;
    let ws = env.workspace_str();
    let mcp_script = write_test_mcp_script(&env.home);
    std::fs::write(env.workspace.join("recent.txt"), "RECENT_READ_PRESERVED_MARKER\n")
        .expect("write recent.txt");

    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![
            (
                "call_old_large",
                "mcp_call",
                json!({ "server": "probe", "tool": "large_output", "arguments": { "kb": 32 } }),
            ),
            (
                "call_old_image",
                "mcp_call",
                json!({ "server": "probe", "tool": "echo_image", "arguments": {} }),
            ),
        ],
        200,
        40,
        "tool_use",
    )));
    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![
            ("call_keep_0", "read", json!({ "path": "recent.txt" })),
            ("call_keep_1", "read", json!({ "path": "recent.txt" })),
            ("call_keep_2", "read", json!({ "path": "recent.txt" })),
            ("call_keep_3", "read", json!({ "path": "recent.txt" })),
            ("call_keep_4", "read", json!({ "path": "recent.txt" })),
        ],
        600,
        50,
        "tool_use",
    )));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "IDLE_CLEAR_TURN_1_DONE",
        40_000,
        0,
        30,
        "end_turn",
    )));

    let mut client = BackendClient::spawn(&env.home).await;
    client
        .setup_workspace_and_provider(&ws, "local", "messages", &server.base_url, "demo", 64000, 4096)
        .await;
    client.register_test_mcp(&ws, &mcp_script).await;

    let session_id = client
        .start_session(&ws, "local", "demo", None, "gather tools before idle gap", vec![], vec![])
        .await;
    let s1 = client.wait_settled(&session_id).await;
    assert_eq!(s1.pointer("/summary/status").and_then(Value::as_str), Some("completed"));
    client.shutdown().await;

    let session_jsonl = env.home.join("sessions").join(format!("{session_id}.jsonl"));
    backdate_session_timestamps(&session_jsonl, "2020-01-01T00:00:00.000Z");

    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "IDLE_CLEAR_TURN_2_DONE",
        1_200,
        0,
        20,
        "end_turn",
    )));
    let reqs_before_resume = server.model_requests().len();
    let mut restarted = BackendClient::spawn(&env.home).await;
    restarted.submit(&session_id, "resume after idle gap").await;
    let s2 = restarted.wait_settled(&session_id).await;
    assert_eq!(s2.pointer("/summary/status").and_then(Value::as_str), Some("completed"));

    let reqs = server.model_requests();
    assert_eq!(reqs.len(), reqs_before_resume + 1);
    let resumed_body = &reqs.last().expect("resumed request").body;

    let cleared_large = find_tool_result(resumed_body, "call_old_large");
    let cleared_large_text = tool_result_text(cleared_large);
    assert!(
        !cleared_large_text.contains("LLLLLLLLLLLLLLLL"),
        "old large tool output must be replaced after idle gap: {cleared_large_text}"
    );
    let spill_path = cleared_large_text
        .rsplit("Full output: ")
        .next()
        .and_then(|rest| rest.trim().strip_suffix(']').or(Some(rest.trim())))
        .filter(|p| cleared_large_text.contains("Full output: ") && !p.is_empty())
        .unwrap_or_else(|| {
            panic!("cleared large tool result must preserve spill path: {cleared_large_text}")
        });
    assert!(
        Path::new(spill_path).is_file(),
        "preserved spill path must point to real output file on disk: {spill_path}"
    );

    let cleared_img = find_tool_result(resumed_body, "call_old_image");
    assert!(
        !cleared_img.to_string().contains("\"type\":\"image\""),
        "cleared image tool result must omit old image blocks from model request: {cleared_img}"
    );
    assert!(
        !tool_result_text(cleared_img).contains("MCP_IMAGE_TOOL_TEXT"),
        "cleared image tool result must replace original payload: {cleared_img}"
    );

    let recent_read = find_tool_result(resumed_body, "call_keep_4");
    assert!(
        tool_result_text(recent_read).contains("RECENT_READ_PRESERVED_MARKER"),
        "recent protected tool results must stay intact after idle gap"
    );

    let history = restarted.rpc_ok("session.read", json!({ "sessionId": session_id })).await;
    let turn1_items =
        history.pointer("/history/turns/0/items").and_then(Value::as_array).expect("turn 1 items");
    let ui_large = turn1_items
        .iter()
        .find(|item| {
            item.get("type").and_then(Value::as_str) == Some("tool")
                && item.pointer("/args/tool").and_then(Value::as_str) == Some("large_output")
        })
        .expect("large_output tool in frontend history");
    assert!(
        ui_large
            .pointer("/result/output")
            .and_then(Value::as_str)
            .is_some_and(|out| out.contains("LLLLLLLLLLLLLLLL")),
        "session.read must preserve full original tool output for UI display"
    );
    let ui_image = turn1_items
        .iter()
        .find(|item| {
            item.get("type").and_then(Value::as_str) == Some("tool")
                && item.pointer("/args/tool").and_then(Value::as_str) == Some("echo_image")
        })
        .expect("echo_image tool in frontend history");
    assert!(
        ui_image
            .pointer("/result/images")
            .and_then(Value::as_array)
            .is_some_and(|imgs| !imgs.is_empty()),
        "session.read must preserve original image attachment for UI display: {ui_image}"
    );

    server.assert_no_protocol_violations();
    restarted.shutdown().await;
}

fn is_responses_compaction_request(body: &Value) -> bool {
    let Some(last) = body.get("input").and_then(Value::as_array).and_then(|items| items.last()) else {
        return false;
    };
    if last.get("type").and_then(Value::as_str) != Some("message")
        || last.get("role").and_then(Value::as_str) != Some("user")
    {
        return false;
    }
    match last.get("content") {
        Some(Value::String(text)) => !text.contains(USER_TURN_TAG),
        Some(Value::Array(parts)) => parts
            .last()
            .and_then(|part| part.get("text").and_then(Value::as_str))
            .is_some_and(|text| !text.contains(USER_TURN_TAG)),
        _ => false,
    }
}

#[tokio::test]
async fn responses_protocol_compaction_invariants() {
    let env = TestEnv::new("responses-compaction");
    let server = MockModelServer::start().await;
    let ws = env.workspace_str();

    let large_notes =
        format!("RESP_TOOL_OUTPUT_MARKER\n{}", "responses_note_payload_0123456789abcdef\n".repeat(420));
    std::fs::write(env.workspace.join("notes.txt"), large_notes).expect("write notes.txt");

    server.enqueue(ScriptedResponse::sse(responses_sse(
        Some(("round 1 tool plan", "enc_r1_tool")),
        None,
        vec![("resp_call_1", "read", json!({ "path": "notes.txt" }))],
        1200,
        0,
        80,
        false,
    )));
    server.enqueue(ScriptedResponse::sse(responses_sse(
        Some(("round 1 synthesis", "enc_r1_done")),
        Some("RESP_ROUND_1_ANSWER"),
        vec![],
        6500,
        400,
        60,
        false,
    )));
    server.enqueue(ScriptedResponse::sse(responses_sse(
        Some(("round 2 reasoning", "enc_r2")),
        Some("RESP_TAIL_ANSWER_2"),
        vec![],
        9800,
        800,
        50,
        false,
    )));
    server.enqueue(ScriptedResponse::sse(responses_sse(
        Some(("compacting prefix", "enc_compact")),
        Some("RESP_CHECKPOINT_SUMMARY_MARKER: condensed round 1 tool investigation."),
        vec![],
        7000,
        400,
        40,
        false,
    )));
    server.enqueue(ScriptedResponse::sse(responses_sse(
        None,
        Some("RESP_POST_COMPACT_ANSWER"),
        vec![],
        3800,
        200,
        30,
        false,
    )));
    server.enqueue(ScriptedResponse::sse(responses_sse(
        None,
        Some("RESP_POST_RESTART_ANSWER"),
        vec![],
        4100,
        200,
        30,
        false,
    )));

    let mut client = BackendClient::spawn(&env.home).await;
    client
        .setup_workspace_and_provider(
            &ws,
            "resp-local",
            "responses",
            &server.base_url,
            "resp-model",
            COMPACT_CONTEXT_WINDOW,
            2048,
        )
        .await;

    let session_id = client
        .start_session(
            &ws,
            "resp-local",
            "resp-model",
            Some("high"),
            &format!("{USER_TURN_TAG} RESP_ORIGIN_ROUND_1"),
            vec![],
            vec![],
        )
        .await;
    let s1 = client.wait_settled(&session_id).await;
    assert_eq!(s1.pointer("/summary/status").and_then(Value::as_str), Some("completed"));

    let tail_prompt = format!("{USER_TURN_TAG} RESP_TAIL_TURN_2: {}", "tail_segment_".repeat(950));
    client.submit(&session_id, &tail_prompt).await;
    let s2 = client.wait_settled(&session_id).await;
    assert_eq!(s2.pointer("/summary/status").and_then(Value::as_str), Some("completed"));

    client.rpc_ok("session.compact", json!({ "sessionId": session_id })).await;
    let compact_settled = client.wait_settled(&session_id).await;
    assert!(
        compact_settled.pointer("/payload/failure").is_none_or(Value::is_null),
        "manual Responses compaction must succeed without failure: {compact_settled}"
    );

    client.submit(&session_id, &format!("{USER_TURN_TAG} RESP_POST_COMPACT_TURN_3")).await;
    let s3 = client.wait_settled(&session_id).await;
    assert_eq!(s3.pointer("/summary/status").and_then(Value::as_str), Some("completed"));

    let reqs = server.model_requests();
    assert_eq!(reqs.len(), 5, "expected 2 tool-turn + 1 tail + 1 compaction + 1 post-compact requests");
    for (idx, req) in reqs.iter().enumerate() {
        assert!(req.path.ends_with("/responses"), "req #{idx} must target /responses: {}", req.path);
        assert_eq!(
            req.body.get("prompt_cache_key").and_then(Value::as_str),
            Some(session_id.as_str()),
            "req #{idx} must set prompt_cache_key to session_id"
        );
    }

    let compact_req = &reqs[3].body;
    assert!(
        is_responses_compaction_request(compact_req),
        "req #3 must be a Responses compaction request"
    );
    let compact_text = compact_req.to_string();
    assert!(
        compact_text.contains("RESP_ORIGIN_ROUND_1")
            && compact_text.contains("RESP_TOOL_OUTPUT_MARKER")
            && compact_text.contains("enc_r1_tool"),
        "Responses compaction request must replay encrypted reasoning and tool results from compacted prefix"
    );

    let post_compact_req = &reqs[4].body;
    assert!(
        !is_responses_compaction_request(post_compact_req),
        "req #4 must be normal post-compaction generation"
    );
    let post_compact_text = post_compact_req.to_string();
    assert!(
        post_compact_text.contains("RESP_CHECKPOINT_SUMMARY_MARKER")
            && post_compact_text.contains("RESP_TAIL_TURN_2")
            && post_compact_text.contains("RESP_TAIL_ANSWER_2")
            && post_compact_text.contains("enc_r2"),
        "post-compaction Responses request must project checkpoint and retained tail verbatim"
    );
    assert!(
        !post_compact_text.contains("RESP_ORIGIN_ROUND_1")
            && !post_compact_text.contains("RESP_TOOL_OUTPUT_MARKER"),
        "compacted prefix must not reappear after Responses compaction"
    );

    server.assert_no_protocol_violations();
    client.shutdown().await;

    let mut restarted = BackendClient::spawn(&env.home).await;
    let history = restarted.rpc_ok("session.read", json!({ "sessionId": session_id })).await;
    let history_text = history.to_string();
    assert!(
        history_text.contains("RESP_ORIGIN_ROUND_1")
            && history_text.contains("RESP_CHECKPOINT_SUMMARY_MARKER")
            && history_text.contains("RESP_POST_COMPACT_ANSWER"),
        "restarted session.read must preserve full history and compaction checkpoint"
    );

    restarted.submit(&session_id, &format!("{USER_TURN_TAG} RESP_POST_RESTART_TURN_4")).await;
    let s4 = restarted.wait_settled(&session_id).await;
    assert_eq!(s4.pointer("/summary/status").and_then(Value::as_str), Some("completed"));

    let all_reqs = server.model_requests();
    assert_eq!(all_reqs.len(), 6, "post-restart turn must send one Responses request");
    let restart_req = &all_reqs[5];
    assert!(restart_req.path.ends_with("/responses"));
    assert_eq!(
        restart_req.body.get("prompt_cache_key").and_then(Value::as_str),
        Some(session_id.as_str()),
        "post-restart Responses request must keep prompt_cache_key == session_id"
    );
    let restart_text = restart_req.body.to_string();
    assert!(
        restart_text.contains("RESP_CHECKPOINT_SUMMARY_MARKER")
            && restart_text.contains("RESP_TAIL_ANSWER_2")
            && !restart_text.contains("RESP_ORIGIN_ROUND_1")
            && !restart_text.contains("RESP_TOOL_OUTPUT_MARKER"),
        "post-restart Responses generation must keep projecting persisted checkpoint and retained tail"
    );

    server.assert_no_protocol_violations();
    restarted.shutdown().await;
}
