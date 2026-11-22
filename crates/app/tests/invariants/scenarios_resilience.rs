//! 韧性与系统级不变量：流式故障自愈/收敛、上下文超窗收敛与结构保真、并发停止/删除、日志修复与焦点流控。

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

    // 子场景 1（偶发故障自愈）：先返回 HTTP 429 (retry-after-ms: 10)，再返回半截断流（含部分可见文本），最后成功。
    // 不变量：最终成功、收到 message/discarded、历史中无半截脏文本残留。
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

    // 子场景 2（不可重试流内报错 + 部分输出隔离）：输出半截文本后流内报错（配额耗尽，不重试）。
    // 下一轮提交正常消息时，半截失败文本不得进入发往模型的上下文。
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

    // 子场景 3（超长 Retry-After 不挂起）：返回 Retry-After: 120，必须快速失败而非等待两分钟。
    server.enqueue(ScriptedResponse::json_with_headers(
        429,
        vec![("retry-after", "120")],
        json!({ "error": { "code": "rate_limit_exceeded", "message": "too long wait" } }),
    ));

    // 子场景 4（输出截断保护）：max_tokens 截断并带工具调用时，必须拒绝执行截断工具并记为失败。
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

    // 1. 偶发故障自愈
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

    // 2. 失败片段保留在历史展示中，但绝不进入下一轮模型请求上下文
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

    // 3. 超长 Retry-After 立即收敛失败
    let start_fast_fail = std::time::Instant::now();
    client.submit(&session_id, "trigger long retry-after").await;
    let settled4 = client.wait_settled(&session_id).await;
    assert_eq!(settled4.pointer("/summary/status").and_then(Value::as_str), Some("failed"));
    assert!(
        start_fast_fail.elapsed().as_secs() < 10,
        "Retry-After exceeding cap must fail promptly without sleeping"
    );

    // 4. 输出长度截断不执行未完整工具调用
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

    // 策略无关的动态上游桩：请求超限时返回 context_length_exceeded，每次请求自动校验协议不变量。
    let overflow_req_len = Arc::new(AtomicUsize::new(0));
    let overflow_len_recorder = Arc::clone(&overflow_req_len);
    let recovered_req_len = Arc::new(AtomicUsize::new(0));
    let recovered_len_recorder = Arc::clone(&recovered_req_len);
    let compaction_success_count = Arc::new(AtomicUsize::new(0));
    let compaction_ok_counter = Arc::clone(&compaction_success_count);
    let normal_step = Arc::new(AtomicUsize::new(0));
    let step_counter = Arc::clone(&normal_step);

    // 按末尾消息是否为测试用户提示识别内部压缩请求。
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

        // 第 1 步先写入中等大小工具结果，确保第 1 轮在 90% 自动压缩阈值以下完成，
        // 第 2 轮“LATEST_INTENT_MUST_SURVIVE”由上游直接触发 context_length_exceeded 溢出恢复。
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

        // 第 2 轮由上游触发超窗，验证溢出恢复保留最后 1 个完整平衡单元并压缩其之前前缀。
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

    // 累积超过 15% 保留预算的多轮历史后，验证手动压缩空摘要报错不破坏上下文，随后二轮手动压缩成功且保留 15% 尾部。
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

    // 触发 90% 自动压缩（窗口 20_000 -> >= 18_000 且 < 20_000）且返回空摘要失败时，
    // 回合应记录警告并直接继续发主请求完成生成，不中断当前回合。
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

    // 切到窗口更小的模型且当前历史达到新模型 90% 时：必须先用旧模型按新模型 15% 保留预算预压；
    // 旧模型预压失败时明确报错且不得向新模型发送超限主请求；预压成功后才切由新模型生成。
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

    // 约 800 tokens（> small-model 15% 保留预算 746），确保旧前缀可被独立切出压缩。
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

    // 1. 流式运行中停止、工具执行中停止、提问挂起时停止，随后继续对话不死锁且工具调用严格配对；
    //    同时验证项目有运行中任务时 workspace.remove 必须被拒绝。
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

    // 1b. 工具执行中停止（慢 bash + 后续挂起提问），停止后必须及时回收进程而非阻塞 30s。
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

    // 1c. 提问挂起时停止，再发送下一条消息续聊：所有被中断的 tool_use 必须合法配对且会话正常收敛。
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

    // 2. 运行中删除会话（session.delete 两段锁）：不死锁，且彻底清理 JSONL、images/ 和 outputs/。
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

    // 3. 焦点过滤不变量：非聚焦会话不推送高频 TextDelta，但仍推送终态 SessionSettled。
    let focus_gate = Arc::new(Notify::new());
    server.enqueue(ScriptedResponse::sse_gated(
        messages_text_sse(None, "unfocused stream delta", 50, 0, 10, "end_turn"),
        Arc::clone(&focus_gate),
    ));
    let expected_focus_reqs = server.model_requests().len() + 1;
    let s_unfocused = client
        .start_session(&ws, "local", "demo", None, "check focus filter", vec![], vec![])
        .await;
    // 把焦点切到另一个会话（s_abort），再放行 s_unfocused 的流式输出。
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

    // 4. 多会话压力（超过常见会话缓存数）+ 任意重读一致性 + 项目空闲移除与重新添加恢复。
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

    // 空闲时移除项目：从工作台隐藏但不删除磁盘日志，重新添加后历史会话重新可见。
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

    // 5. 日志崩溃尾行自愈 & 完整行损坏显式报错（对齐 docs/decisions.md）。
    let target_log = env.home.join("sessions").join(format!("{}.jsonl", batch_ids[0]));
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&target_log).expect("open log");
        // 写入末尾缺少换行的半截 JSON，模拟写盘中断。
        f.write_all(b"{\"type\":\"message\",\"truncated\":true").expect("append partial tail");
    }

    let mut repaired_client = BackendClient::spawn(&env.home).await;
    let repaired = repaired_client.rpc_ok("session.read", json!({ "sessionId": batch_ids[0] })).await;
    assert!(
        repaired.to_string().contains("reply_0"),
        "trailing partial line without newline must auto-repair"
    );
    repaired_client.shutdown().await;

    // 写入带换行的完整损坏行，bootstrap 必须显式报错而非静默跳过。
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
