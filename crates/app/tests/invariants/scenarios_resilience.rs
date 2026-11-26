use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::harness::{
    BackendClient, DEFAULT_TIMEOUT, MockModelServer, ScriptedResponse, TINY_PNG_DATA_URL, TestEnv,
    messages_text_sse, messages_tools_sse, write_test_mcp_script,
};

#[tokio::test]
async fn stream_fault_recovery_and_truncation_invariants() {
    let env = TestEnv::new("fault-invariants");
    let server = MockModelServer::start().await;
    let ws = env.workspace_str();

    server.enqueue(ScriptedResponse::json_with_headers(
        429,
        vec![("retry-after-ms", "10")],
        json!({ "error": { "code": "rate_limit_exceeded", "message": "slow down" } }),
    ));
    server.enqueue(ScriptedResponse::sse_truncated_drop(vec![
        format!(
            "data: {}\n\n",
            json!({ "type": "message_start", "message": { "usage": { "input_tokens": 50, "output_tokens": 1 } } })
        ),
        format!(
            "data: {}\n\n",
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "TRANSIENT_GARBAGE" } })
        ),
    ]));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "CLEAN_RECOVERED_ANSWER",
        60,
        0,
        20,
        "end_turn",
    )));

    server.enqueue(ScriptedResponse::sse(vec![
        format!(
            "data: {}\n\n",
            json!({ "type": "message_start", "message": { "usage": { "input_tokens": 80, "output_tokens": 1 } } })
        ),
        format!(
            "data: {}\n\n",
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "FAILED_PARTIAL_LEAK" } })
        ),
        format!(
            "data: {}\n\n",
            json!({ "type": "error", "error": { "code": "insufficient_quota", "message": "quota exhausted" } })
        ),
    ]));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(None, "POST_FAILURE_OK", 90, 0, 15, "end_turn")));

    server.enqueue(ScriptedResponse::json_with_headers(
        429,
        vec![("retry-after", "120")],
        json!({ "error": { "code": "rate_limit_exceeded", "message": "too long wait" } }),
    ));

    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        Some("cut off"),
        vec![("trunc_call", "write", json!({ "path": "should_not_exist.txt", "content": "bad" }))],
        100,
        40,
        "max_tokens",
    )));

    let mut client = BackendClient::spawn(&env.home).await;
    client
        .setup_workspace_and_provider(&ws, "local", "messages", &server.base_url, "demo", 64000, 4096)
        .await;

    let session_id = client
        .start_session(&ws, "local", "demo", None, "trigger transient retry", vec![], vec![])
        .await;
    let settled1 = client.wait_settled(&session_id).await;
    assert_eq!(settled1.pointer("/summary/status").and_then(Value::as_str), Some("completed"));
    let events1 = client.drain_events();
    assert!(
        events1
            .iter()
            .any(|e| e.pointer("/payload/method").and_then(Value::as_str) == Some("message/discarded")),
        "retry after partial output must emit message/discarded"
    );
    let read1 = client.rpc_ok("session.read", json!({ "sessionId": session_id })).await;
    let read1_text = read1.to_string();
    assert!(read1_text.contains("CLEAN_RECOVERED_ANSWER"));
    assert!(
        !read1_text.contains("TRANSIENT_GARBAGE"),
        "discarded partial output must not persist in history"
    );

    client.submit(&session_id, "trigger quota fail").await;
    let settled2 = client.wait_settled(&session_id).await;
    assert_eq!(settled2.pointer("/summary/status").and_then(Value::as_str), Some("failed"));

    client.submit(&session_id, "continue after failure").await;
    let settled3 = client.wait_settled(&session_id).await;
    assert_eq!(settled3.pointer("/summary/status").and_then(Value::as_str), Some("completed"));

    let reqs_after_3 = server.model_requests();
    let req_continue = reqs_after_3.last().expect("continue request").body.to_string();
    assert!(
        !req_continue.contains("FAILED_PARTIAL_LEAK"),
        "failed partial assistant message must never enter subsequent model context"
    );

    let start_fast_fail = std::time::Instant::now();
    client.submit(&session_id, "trigger long retry-after").await;
    let settled4 = client.wait_settled(&session_id).await;
    assert_eq!(settled4.pointer("/summary/status").and_then(Value::as_str), Some("failed"));
    assert!(
        start_fast_fail.elapsed().as_secs() < 10,
        "Retry-After exceeding cap must fail promptly without sleeping"
    );

    client.submit(&session_id, "trigger truncated tool").await;
    let settled5 = client.wait_settled(&session_id).await;
    assert_eq!(settled5.pointer("/summary/status").and_then(Value::as_str), Some("failed"));
    assert!(
        !env.workspace.join("should_not_exist.txt").exists(),
        "truncated tool call must never execute side effects"
    );

    client.shutdown().await;
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

    let is_compaction_request = |body: &Value| -> bool {
        let Some(last_blocks) = body
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|msgs| msgs.last())
            .and_then(|msg| msg.get("content"))
            .and_then(Value::as_array)
        else {
            return false;
        };
        let last_text = last_blocks
            .iter()
            .rev()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .find_map(|b| b.get("text").and_then(Value::as_str));
        match last_text {
            Some(text) => text.contains("You are now acting as a compaction engine"),
            None => false,
        }
    };

    server.set_fallback(move |req, _| {
        let serialized = req.body.to_string();
        let is_compact_prompt = is_compaction_request(&req.body);

        if is_compact_prompt {
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
            20_000,
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
            "USER_ORIGIN_MARKER_ROUND_1",
            vec![json!({ "name": "img.png", "dataUrl": TINY_PNG_DATA_URL })],
            vec!["skills"],
        )
        .await;
    let s1 = client.wait_settled(&session_id).await;
    assert_eq!(s1.pointer("/summary/status").and_then(Value::as_str), Some("completed"));

    client.submit(&session_id, "LATEST_INTENT_MUST_SURVIVE").await;
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
        let heavy = format!("HEAVY_POST_COMPACTION_TURN_{idx}: {}", "payload_segment_".repeat(320));
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
    client.submit(&session_id, "after failed compaction").await;
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
    client.submit(&session_id, "after retained tail compaction").await;
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
    let history_text = history.to_string();
    assert!(history_text.contains("USER_ORIGIN_MARKER_ROUND_1"));
    assert!(history_text.contains("LATEST_INTENT_MUST_SURVIVE"));
    assert!(history_text.contains("STILL_USABLE_AFTER_FAILED_COMPACTION"));

    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "RECOVERED_UNDER_CONTEXT_PRESSURE",
        6_500,
        0,
        40,
        "end_turn",
    )));
    restarted.submit(&session_id, "POST_RESTART_CONTINUATION").await;
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
    let reqs_before_90 = server.model_requests().len();
    let trigger_90_turn = format!("AUTO_90_TRIGGER_TURN: {}", "z".repeat(48_500));
    restarted.submit(&session_id, &trigger_90_turn).await;
    let s_auto_fail = restarted.wait_settled(&session_id).await;
    assert_eq!(
        s_auto_fail.pointer("/summary/status").and_then(Value::as_str),
        Some("completed"),
        "auto compaction failure at 90% must warn and continue generation without aborting turn"
    );
    let post_90_reqs = server.model_requests();
    let post_90_slice = &post_90_reqs[reqs_before_90..];
    assert_eq!(
        post_90_slice.len(),
        2,
        "90% auto-compaction failure must issue exactly 1 compaction request and 1 main generation request"
    );
    assert!(
        is_compaction_request(&post_90_slice[0].body),
        "first request at >= 90% must be automatic compaction"
    );
    assert!(
        !is_compaction_request(&post_90_slice[1].body)
            && post_90_slice[1].body.to_string().contains("AUTO_90_TRIGGER_TURN"),
        "main generation request must proceed immediately after automatic compaction failure"
    );

    restarted
        .rpc_ok(
            "model.saveProvider",
            json!({
                "provider": {
                    "apiProtocol": "messages",
                    "providerId": "local",
                    "displayName": "local",
                    "baseUrl": server.base_url,
                    "models": [
                        {
                            "modelId": "compact-model",
                            "maxContextTokens": 20_000,
                            "maxOutputTokens": 2048,
                            "reasoningVariants": ["off", "low", "high"]
                        },
                        {
                            "modelId": "small-model",
                            "maxContextTokens": 6_000,
                            "maxOutputTokens": 1024,
                            "reasoningVariants": ["off", "low", "high"]
                        }
                    ]
                },
                "apiKey": "test-secret-key"
            }),
        )
        .await;
    restarted
        .rpc_ok(
            "session.updateSettings",
            json!({
                "sessionId": session_id,
                "selection": { "provider": "local", "model": "small-model", "reasoning": null }
            }),
        )
        .await;

    let tail_pad = "p".repeat(3_200);
    let reqs_before_switch_fail = server.model_requests().len();
    server.enqueue(ScriptedResponse::sse(messages_text_sse(None, "   ", 500, 0, 5, "end_turn")));
    restarted
        .submit(&session_id, &format!("SWITCH_FAIL_SHOULD_NOT_HIT_SMALL_MODEL {tail_pad}"))
        .await;
    let s_switch_fail = restarted.wait_settled(&session_id).await;
    assert_eq!(
        s_switch_fail.pointer("/summary/status").and_then(Value::as_str),
        Some("failed"),
        "model-switch pre-compaction failure must abort turn explicitly"
    );
    let switch_fail_reqs = server.model_requests();
    let fail_slice = &switch_fail_reqs[reqs_before_switch_fail..];
    assert!(
        fail_slice.iter().any(|r| {
            is_compaction_request(&r.body)
                && r.body.get("model").and_then(Value::as_str) == Some("compact-model")
        }),
        "model-switch pre-compaction must run on the previous model"
    );
    assert!(
        fail_slice
            .iter()
            .all(|r| r.body.get("model").and_then(Value::as_str) != Some("small-model")),
        "failed model-switch pre-compaction must not send oversized request to the target model"
    );

    let reqs_before_switch_ok = server.model_requests().len();
    restarted.submit(&session_id, &format!("SWITCH_PRECOMPACT_OK_TURN {tail_pad}")).await;
    let s_switch_ok = restarted.wait_settled(&session_id).await;
    assert_eq!(
        s_switch_ok.pointer("/summary/status").and_then(Value::as_str),
        Some("completed"),
        "model-switch pre-compaction success must continue generation on the target model"
    );
    let switch_ok_reqs = server.model_requests();
    let ok_slice = &switch_ok_reqs[reqs_before_switch_ok..];
    assert!(
        ok_slice.iter().any(|r| {
            is_compaction_request(&r.body)
                && r.body.get("model").and_then(Value::as_str) == Some("compact-model")
        }),
        "successful model-switch pre-compaction must compact using the previous model"
    );
    let final_small_req = ok_slice.last().expect("final generation request on small-model");
    assert_eq!(
        final_small_req.body.get("model").and_then(Value::as_str),
        Some("small-model"),
        "post-compaction generation must run on the target model"
    );
    let final_small_text = final_small_req.body.to_string();
    assert!(
        final_small_text.contains("SECOND_CHECKPOINT_UNIQUE_MARKER")
            && final_small_text.contains("SWITCH_PRECOMPACT_OK_TURN")
            && !final_small_text.contains("AUTO_90_TRIGGER_TURN"),
        "target model must receive compacted checkpoint and small retained tail"
    );
    server.assert_no_protocol_violations();
    restarted.shutdown().await;
}

#[tokio::test]
async fn abort_delete_corruption_and_focus_invariants() {
    let env = TestEnv::new("lifecycle-invariants");
    let server = MockModelServer::start().await;
    let ws = env.workspace_str();

    let mut client = BackendClient::spawn(&env.home).await;
    client
        .setup_workspace_and_provider(&ws, "local", "messages", &server.base_url, "demo", 64000, 4096)
        .await;

    let abort_gate = Arc::new(Notify::new());
    server.enqueue(ScriptedResponse::sse_gated(
        messages_text_sse(None, "partial before abort", 50, 0, 10, "end_turn"),
        Arc::clone(&abort_gate),
    ));
    let s_abort = client.start_session(&ws, "local", "demo", None, "will abort", vec![], vec![]).await;
    server.wait_for_request_count(1).await;
    let busy_remove_err = client.rpc_err("workspace.remove", json!({ "workspaceRoot": ws })).await;
    assert!(!busy_remove_err.is_empty(), "workspace.remove must reject while a session is running");
    client.rpc_ok("session.abort", json!({ "sessionId": s_abort })).await;
    let aborted = client.wait_settled(&s_abort).await;
    assert_eq!(aborted.pointer("/summary/manuallyStopped").and_then(Value::as_bool), Some(true));
    abort_gate.notify_one();

    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![(
            "abort_slow_bash",
            "bash",
            json!({ "command": "sleep 30", "description": "slow command to abort" }),
        )],
        60,
        20,
        "tool_use",
    )));
    client.submit(&s_abort, "run slow bash then abort").await;
    client
        .wait_event(DEFAULT_TIMEOUT, |frame| {
            frame.get("sessionId").and_then(Value::as_str) == Some(s_abort.as_str())
                && frame.pointer("/payload/method").and_then(Value::as_str) == Some("item/updated")
                && frame.pointer("/payload/params/item/type").and_then(Value::as_str) == Some("tool")
                && frame.pointer("/payload/params/item/status").and_then(Value::as_str) == Some("running")
        })
        .await;
    let tool_abort_started = std::time::Instant::now();
    client.rpc_ok("session.abort", json!({ "sessionId": s_abort })).await;
    let tool_aborted = client.wait_settled(&s_abort).await;
    assert_eq!(tool_aborted.pointer("/summary/manuallyStopped").and_then(Value::as_bool), Some(true));
    assert!(
        tool_abort_started.elapsed().as_secs() < 10,
        "aborting a running bash tool must terminate promptly"
    );

    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![(
            "abort_pending_ask",
            "ask_user_question",
            json!({
                "questions": [{
                    "id": "q_abort",
                    "question": "Pending question to abort",
                    "options": [{ "label": "yes" }, { "label": "no" }]
                }]
            }),
        )],
        70,
        20,
        "tool_use",
    )));
    client.submit(&s_abort, "ask then abort").await;
    client
        .wait_event(DEFAULT_TIMEOUT, |frame| {
            frame.get("type").and_then(Value::as_str) == Some("session_changed")
                && frame.get("sessionId").and_then(Value::as_str) == Some(s_abort.as_str())
                && frame.pointer("/payload/pendingQuestion/itemId").and_then(Value::as_str).is_some()
        })
        .await;
    client.rpc_ok("session.abort", json!({ "sessionId": s_abort })).await;
    let ask_aborted = client.wait_settled(&s_abort).await;
    assert_eq!(ask_aborted.pointer("/summary/manuallyStopped").and_then(Value::as_bool), Some(true));

    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "CONTINUED_AFTER_TOOL_AND_QUESTION_ABORTS",
        90,
        0,
        20,
        "end_turn",
    )));
    client.submit(&s_abort, "continue after aborts").await;
    let continued = client.wait_settled(&s_abort).await;
    assert_eq!(
        continued.pointer("/summary/status").and_then(Value::as_str),
        Some("completed"),
        "session must recover cleanly after stream/tool/question aborts"
    );

    let delete_gate = Arc::new(Notify::new());
    server.enqueue(ScriptedResponse::sse_gated(
        messages_text_sse(None, "partial before delete", 50, 0, 10, "end_turn"),
        Arc::clone(&delete_gate),
    ));
    let expected_del_reqs = server.model_requests().len() + 1;
    let s_del = client
        .start_session(
            &ws,
            "local",
            "demo",
            None,
            "will delete while running",
            vec![json!({ "name": "del.png", "dataUrl": TINY_PNG_DATA_URL })],
            vec![],
        )
        .await;
    server.wait_for_request_count(expected_del_reqs).await;
    let del_jsonl = env.home.join("sessions").join(format!("{s_del}.jsonl"));
    let del_images = env.home.join("sessions").join("images").join(&s_del);
    assert!(del_jsonl.exists(), "session log must exist before delete");

    client.rpc_ok("session.delete", json!({ "sessionId": s_del })).await;
    delete_gate.notify_one();
    assert!(!del_jsonl.exists(), "deleted session JSONL must be removed");
    assert!(!del_images.exists(), "deleted session images directory must be removed");

    let focus_gate = Arc::new(Notify::new());
    server.enqueue(ScriptedResponse::sse_gated(
        messages_text_sse(None, "unfocused stream delta", 50, 0, 10, "end_turn"),
        Arc::clone(&focus_gate),
    ));
    let expected_focus_reqs = server.model_requests().len() + 1;
    let s_unfocused = client
        .start_session(&ws, "local", "demo", None, "check focus filter", vec![], vec![])
        .await;
    client.rpc_ok("session.focus", json!({ "sessionId": s_abort })).await;
    let _ = client.drain_events();
    server.wait_for_request_count(expected_focus_reqs).await;
    focus_gate.notify_one();
    client.wait_settled(&s_unfocused).await;

    let unfocused_events = client.drain_events();
    let leaked_delta = unfocused_events.iter().any(|e| {
        e.get("sessionId").and_then(Value::as_str) == Some(s_unfocused.as_str())
            && e.pointer("/payload/method").and_then(Value::as_str) == Some("item/textDelta")
    });
    assert!(!leaked_delta, "non-focused session must suppress high-frequency TextDelta frames");

    let mut batch_ids = Vec::new();
    for i in 0..18 {
        server.enqueue(ScriptedResponse::sse(messages_text_sse(
            None,
            &format!("reply_{i}"),
            30,
            0,
            10,
            "end_turn",
        )));
        let id = client
            .start_session(&ws, "local", "demo", None, &format!("batch_{i}"), vec![], vec![])
            .await;
        client.wait_settled(&id).await;
        batch_ids.push(id);
    }
    let first_reread = client.rpc_ok("session.read", json!({ "sessionId": batch_ids[0] })).await;
    assert!(first_reread.to_string().contains("reply_0"));

    client.rpc_ok("workspace.remove", json!({ "workspaceRoot": ws })).await;
    let boot_removed = client.rpc_ok("app.bootstrap", json!({})).await;
    assert!(
        boot_removed.get("workspaces").and_then(Value::as_array).is_some_and(Vec::is_empty),
        "removed workspace must disappear from bootstrap"
    );
    let preserved_log = env.home.join("sessions").join(format!("{}.jsonl", batch_ids[0]));
    assert!(preserved_log.exists(), "removing a workspace must preserve session logs on disk");

    client.rpc_ok("workspace.add", json!({ "root": ws })).await;
    let boot_readded = client.rpc_ok("app.bootstrap", json!({})).await;
    assert!(
        boot_readded.to_string().contains(&batch_ids[0]),
        "re-adding workspace must restore historical sessions in bootstrap"
    );

    server.assert_no_protocol_violations();
    client.shutdown().await;

    let target_log = env.home.join("sessions").join(format!("{}.jsonl", batch_ids[0]));
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&target_log).expect("open log");
        f.write_all(b"{\"type\":\"message\",\"truncated\":true").expect("append partial tail");
    }

    let mut repaired_client = BackendClient::spawn(&env.home).await;
    let repaired = repaired_client.rpc_ok("session.read", json!({ "sessionId": batch_ids[0] })).await;
    assert!(
        repaired.to_string().contains("reply_0"),
        "trailing partial line without newline must auto-repair"
    );
    repaired_client.shutdown().await;

    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&target_log).expect("open log");
        f.write_all(b"{corrupt json line}\n").expect("append corrupt complete line");
    }
    let mut corrupt_client = BackendClient::spawn(&env.home).await;
    let err = corrupt_client.rpc_err("app.bootstrap", json!({})).await;
    assert!(!err.is_empty(), "corrupt complete session line must surface explicit error");
    corrupt_client.shutdown().await;
}

#[tokio::test]
async fn git_diff_and_pure_rewind_invariants() {
    let env = TestEnv::new("rewind-invariants");
    let server = MockModelServer::start().await;
    let ws = env.workspace_str();
    let plain_dir = env.home.join("non-git-workspace");
    std::fs::create_dir_all(&plain_dir).expect("create non-git workspace");
    let plain_ws = app_core::display_path(&plain_dir);

    let tracked = env.workspace.join("tracked.txt");
    let extra = env.workspace.join("extra.txt");
    let mv_old = env.workspace.join("mv_old.txt");
    let mv_new = env.workspace.join("mv_new.txt");
    let bracket_file = env.workspace.join("src").join("[id]").join("page.ts");
    let plain_i_file = env.workspace.join("src").join("i").join("page.ts");
    std::fs::create_dir_all(bracket_file.parent().expect("bracket parent")).expect("mkdir [id]");
    std::fs::create_dir_all(plain_i_file.parent().expect("i parent")).expect("mkdir i");
    std::fs::write(&tracked, "v1\n").expect("write tracked baseline");
    std::fs::write(&mv_old, "old1\nold2\nold3\nold4\n").expect("write mv_old baseline");
    std::fs::write(&bracket_file, "export const id = 1\n").expect("write [id] baseline");
    std::fs::write(&plain_i_file, "export const i = 1\n").expect("write i baseline");
    let run_git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .current_dir(&env.workspace)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    };
    run_git(&["init", "-q"]);
    run_git(&["config", "user.email", "test@example.invalid"]);
    run_git(&["config", "user.name", "Test"]);
    run_git(&["add", "-A"]);
    run_git(&["commit", "-q", "-m", "baseline"]);
    // Hostile user git config must not break batch diff parsing or path matching.
    run_git(&["config", "diff.noprefix", "true"]);
    run_git(&["config", "diff.mnemonicPrefix", "true"]);
    run_git(&["config", "diff.relative", "true"]);

    server.enqueue(ScriptedResponse::sse(messages_text_sse(None, "NON_GIT_OK", 40, 0, 10, "end_turn")));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(None, "TURN_1_BASELINE", 60, 0, 15, "end_turn")));
    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![
            ("w_tracked", "write", json!({ "path": "tracked.txt", "content": "v2\n" })),
            ("w_extra", "write", json!({ "path": "extra.txt", "content": "extra\n" })),
            ("w_noop", "write", json!({ "path": "tracked.txt", "content": "v2\n" })),
        ],
        80,
        30,
        "tool_use",
    )));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(None, "TURN_2_MUTATED", 100, 0, 15, "end_turn")));

    let mut client = BackendClient::spawn(&env.home).await;
    client
        .setup_workspace_and_provider(&ws, "local", "messages", &server.base_url, "demo", 64000, 4096)
        .await;
    client.rpc_ok("workspace.add", json!({ "root": plain_ws })).await;

    // Non-git workspace returns empty changedFiles cleanly without creating any snapshot dir.
    let s_plain = client.start_session(&plain_ws, "local", "demo", None, "plain turn", vec![], vec![]).await;
    client.wait_settled(&s_plain).await;
    let plain_changed = client.rpc_ok("session.changedFiles", json!({ "sessionId": s_plain })).await;
    assert_eq!(plain_changed.get("files").and_then(Value::as_array).map(Vec::len), Some(0));
    assert!(
        !env.home.join("sessions").join("snapshots").exists(),
        "Shadow Git snapshots dir must not exist"
    );

    let s_git = client.start_session(&ws, "local", "demo", None, "baseline turn", vec![], vec![]).await;
    client.wait_settled(&s_git).await;

    client.submit(&s_git, "mutate tracked files").await;
    client.wait_settled(&s_git).await;
    assert_eq!(std::fs::read_to_string(&tracked).expect("read mutated file"), "v2\n");
    assert!(extra.exists(), "extra.txt must exist after turn 2");

    // Rename after turn 2 to verify pure rewind preserves ThreadName and ThreadSettings.
    client.rpc_ok("session.rename", json!({ "sessionId": s_git, "name": "Keep Title" })).await;

    let before_rewind = client.rpc_ok("session.read", json!({ "sessionId": s_git })).await;
    let turns = before_rewind.pointer("/history/turns").and_then(Value::as_array).expect("turns");
    assert_eq!(turns.len(), 2);
    let turn2_items = turns[1].get("items").and_then(Value::as_array).expect("turn 2 items");
    let created_flags: Vec<(&str, Option<bool>)> = turn2_items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool"))
        .map(|item| {
            (
                item.pointer("/args/path").and_then(Value::as_str).unwrap_or_default(),
                item.pointer("/result/fileCreated").and_then(Value::as_bool),
            )
        })
        .collect();
    assert_eq!(
        created_flags,
        vec![
            ("tracked.txt", Some(false)),
            ("extra.txt", Some(true)),
            ("tracked.txt", Some(false))
        ],
        "write tool must accurately report fileCreated for new vs overwritten/no-op files"
    );
    let turn2_user_msg_id = turn2_items
        .iter()
        .find(|item| {
            item.get("type").and_then(Value::as_str) == Some("message")
                && item.get("role").and_then(Value::as_str) == Some("user")
        })
        .and_then(|item| item.get("id").and_then(Value::as_str))
        .expect("turn 2 user message id")
        .to_string();

    // Exercise git mv + full rewrite and glob-like bracket filenames under hostile diff config.
    run_git(&["mv", "mv_old.txt", "mv_new.txt"]);
    std::fs::write(&mv_new, "completely\ndifferent\nrewritten\nbody\nnow\n").expect("rewrite mv_new");
    std::fs::write(&bracket_file, "export const id = 2\n").expect("modify [id]");
    std::fs::write(&plain_i_file, "export const i = 2\n").expect("modify i");

    let changed = client.rpc_ok("session.changedFiles", json!({ "workspaceRoot": ws })).await;
    let changed_items = changed.get("files").and_then(Value::as_array).expect("changed files");
    let changed_paths: Vec<&str> =
        changed_items.iter().filter_map(|f| f.get("path").and_then(Value::as_str)).collect();
    assert!(changed_paths.contains(&"tracked.txt"), "changedFiles: {changed_paths:?}");
    assert!(changed_paths.contains(&"extra.txt"), "changedFiles: {changed_paths:?}");
    assert!(changed_paths.contains(&"mv_old.txt"), "split mv must include deletion: {changed_paths:?}");
    assert!(changed_paths.contains(&"mv_new.txt"), "split mv must include addition: {changed_paths:?}");
    assert!(
        changed_items.iter().all(|f| f.get("diff").is_none()),
        "lightweight changedFiles must omit inline diffs: {changed_items:?}"
    );

    let changed_with_diffs = client
        .rpc_ok("session.changedFiles", json!({ "workspaceRoot": ws, "includeDiffs": true }))
        .await;
    let diff_items = changed_with_diffs
        .get("files")
        .and_then(Value::as_array)
        .expect("changed files with diffs");
    let tracked_batch = diff_items
        .iter()
        .find(|f| f.get("path").and_then(Value::as_str) == Some("tracked.txt"))
        .and_then(|f| f.get("diff").and_then(Value::as_str))
        .unwrap_or_default();
    assert!(
        tracked_batch.contains("-v1") && tracked_batch.contains("+v2"),
        "batch tracked diff mismatch under diff.noprefix/relative: {tracked_batch}"
    );
    let extra_batch = diff_items
        .iter()
        .find(|f| f.get("path").and_then(Value::as_str) == Some("extra.txt"))
        .and_then(|f| f.get("diff").and_then(Value::as_str))
        .unwrap_or_default();
    assert!(extra_batch.contains("+extra"), "batch untracked diff mismatch: {extra_batch}");
    let mv_old_entry = diff_items
        .iter()
        .find(|f| f.get("path").and_then(Value::as_str) == Some("mv_old.txt"))
        .expect("mv_old.txt batch entry");
    assert_eq!(mv_old_entry.get("status").and_then(Value::as_str), Some("deleted"));
    assert!(mv_old_entry.get("diff").and_then(Value::as_str).unwrap_or_default().contains("-old1"));

    let bracket_diff = client
        .rpc_ok("session.fileDiff", json!({ "workspaceRoot": ws, "path": "src/[id]/page.ts" }))
        .await;
    let bracket_out = bracket_diff.get("output").and_then(Value::as_str).unwrap_or_default();
    assert!(
        bracket_out.contains("+export const id = 2") && !bracket_out.contains("export const i = 2"),
        "literal pathspec must not glob src/i/page.ts: {bracket_out}"
    );

    // Unborn git repo (no HEAD yet): staged-then-edited file (AM) must diff working tree against empty tree.
    let unborn_dir = env.home.join("unborn_git");
    std::fs::create_dir_all(&unborn_dir).expect("mkdir unborn");
    let unborn_ws = app_core::display_path(&unborn_dir);
    let run_unborn_git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .current_dir(&unborn_dir)
            .args(args)
            .status()
            .expect("run unborn git");
        assert!(status.success(), "unborn git {args:?} failed");
    };
    run_unborn_git(&["init", "-q"]);
    std::fs::write(unborn_dir.join("am.txt"), "staged\n").expect("write staged am.txt");
    run_unborn_git(&["add", "am.txt"]);
    std::fs::write(unborn_dir.join("am.txt"), "staged\nworktree\n").expect("edit am.txt");
    client.rpc_ok("workspace.add", json!({ "root": unborn_ws })).await;
    let unborn_changed = client
        .rpc_ok("session.changedFiles", json!({ "workspaceRoot": unborn_ws, "includeDiffs": true }))
        .await;
    let am_diff = unborn_changed
        .get("files")
        .and_then(Value::as_array)
        .and_then(|arr| arr.iter().find(|f| f.get("path").and_then(Value::as_str) == Some("am.txt")))
        .and_then(|f| f.get("diff").and_then(Value::as_str))
        .unwrap_or_default();
    assert!(
        am_diff.contains("+staged") && am_diff.contains("+worktree"),
        "unborn AM file must reflect working tree content: {am_diff}"
    );

    let diff = client
        .rpc_ok("session.fileDiff", json!({ "sessionId": s_git, "path": "tracked.txt" }))
        .await;
    let diff_out = diff.get("output").and_then(Value::as_str).unwrap_or_default();
    assert!(diff_out.contains("-v1") && diff_out.contains("+v2"), "unexpected diff: {diff_out}");

    let untracked_diff = client
        .rpc_ok("session.fileDiff", json!({ "sessionId": s_git, "path": "extra.txt" }))
        .await;
    let untracked_out = untracked_diff.get("output").and_then(Value::as_str).unwrap_or_default();
    assert!(untracked_out.contains("+extra"), "unexpected untracked diff: {untracked_out}");

    // Failed turn with a queued steering input clears unconsumed input so rewind & workspace.remove work.
    let fail_gate = Arc::new(Notify::new());
    server.enqueue(ScriptedResponse::sse_gated(
        vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fail\",\"usage\":{\"input_tokens\":20}}}\n\n".into(),
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"bad prompt\"}}\n\n".into(),
        ],
        Arc::clone(&fail_gate),
    ));
    let reqs_before_fail = server.model_requests().len();
    client.submit(&s_git, "will fail").await;
    server.wait_for_request_count(reqs_before_fail + 1).await;
    client.submit(&s_git, "stale steering input").await;
    fail_gate.notify_one();
    client.wait_settled(&s_git).await;
    let after_fail = client.rpc_ok("session.read", json!({ "sessionId": s_git })).await;
    assert_eq!(
        after_fail.pointer("/runtime/pendingMessages").and_then(Value::as_array).map(Vec::len),
        Some(0),
        "failed turn must clear unconsumed steering input"
    );

    // Atomic rewind with invalid resubmit input (missing file attachment) must fail WITHOUT truncating history.
    let missing_file = app_core::display_path(&env.workspace.join("missing_file.txt"));
    let invalid_err = client
        .rpc_err(
            "session.rewind",
            json!({
                "sessionId": s_git,
                "messageId": turn2_user_msg_id,
                "resubmit": { "text": "edited text", "files": [missing_file] }
            }),
        )
        .await;
    assert!(!invalid_err.is_empty(), "missing attachment must fail rewind before truncation");
    let untouched = client.rpc_ok("session.read", json!({ "sessionId": s_git })).await;
    assert_eq!(
        untouched.pointer("/history/turns").and_then(Value::as_array).map(Vec::len),
        Some(3),
        "failed atomic rewind must leave session history intact"
    );

    // Atomic rewind with valid resubmit truncates and immediately starts the edited turn.
    server.enqueue(ScriptedResponse::sse(messages_text_sse(None, "TURN_2_EDITED_OK", 70, 0, 15, "end_turn")));
    let tracked_abs = app_core::display_path(&tracked);
    let atomic_resubmit = client
        .rpc_ok(
            "session.rewind",
            json!({
                "sessionId": s_git,
                "messageId": turn2_user_msg_id,
                "resubmit": { "text": "edited prompt", "files": [tracked_abs] }
            }),
        )
        .await;
    assert_eq!(
        atomic_resubmit.pointer("/runtime/phase").and_then(Value::as_str),
        Some("running"),
        "atomic rewind+resubmit must start the edited turn immediately"
    );
    client.wait_settled(&s_git).await;
    let after_edit = client.rpc_ok("session.read", json!({ "sessionId": s_git })).await;
    assert!(after_edit.to_string().contains("TURN_2_EDITED_OK"));
    assert!(!after_edit.to_string().contains("TURN_2_MUTATED"));
    let edited_turn2_user_msg_id = after_edit
        .pointer("/history/turns/1/items")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|item| {
                    item.get("type").and_then(Value::as_str) == Some("message")
                        && item.get("role").and_then(Value::as_str) == Some("user")
                })
                .and_then(|item| item.get("id").and_then(Value::as_str))
        })
        .expect("edited turn 2 user message id")
        .to_string();

    // Pure rewind truncates history from turn 2 user message while keeping workspace files, ThreadName, and settings intact.
    let rewound = client
        .rpc_ok("session.rewind", json!({ "sessionId": s_git, "messageId": edited_turn2_user_msg_id }))
        .await;
    let rewound_turns = rewound.pointer("/history/turns").and_then(Value::as_array).expect("rewound turns");
    assert_eq!(rewound_turns.len(), 1, "rewind must truncate turn 2 and failed turn from history");
    assert!(!rewound.to_string().contains("TURN_2_MUTATED"));
    assert_eq!(rewound.pointer("/history/summary/title").and_then(Value::as_str), Some("Keep Title"));
    assert_eq!(
        std::fs::read_to_string(&tracked).expect("read tracked after pure rewind"),
        "v2\n",
        "pure rewind must leave workspace files untouched"
    );
    assert!(extra.exists(), "pure rewind must leave extra.txt untouched");

    server.assert_no_protocol_violations();
    client.shutdown().await;

    let mut restarted = BackendClient::spawn(&env.home).await;
    let after_restart = restarted.rpc_ok("session.read", json!({ "sessionId": s_git })).await;
    assert_eq!(
        rewound.pointer("/history/turns"),
        after_restart.pointer("/history/turns"),
        "rewound session history must persist identically across restart"
    );
    assert_eq!(
        after_restart.pointer("/history/summary/title").and_then(Value::as_str),
        Some("Keep Title")
    );
    restarted.shutdown().await;
}
