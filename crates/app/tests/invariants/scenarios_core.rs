use std::process::Stdio;

use serde_json::{Value, json};
use tokio::process::Command;

use crate::harness::{
    BackendClient, MockModelServer, ScriptedResponse, TINY_PNG_DATA_URL, TestEnv, messages_text_sse,
    messages_tools_sse, responses_sse,
};

fn provider_config_json(
    provider_id: &str,
    protocol: &str,
    display_name: &str,
    base_url: &str,
    model_id: &str,
    max_context_tokens: u32,
) -> Value {
    json!({
        "apiProtocol": protocol,
        "providerId": provider_id,
        "displayName": display_name,
        "baseUrl": base_url,
        "models": [{
            "modelId": model_id,
            "maxContextTokens": max_context_tokens,
            "maxOutputTokens": 4096,
            "reasoningVariants": ["off", "high"]
        }]
    })
}

#[tokio::test]
async fn workspace_config_and_single_instance_lock() {
    let env = TestEnv::new("core-config");
    let server = MockModelServer::start().await;
    server.set_models(vec!["alpha-model", "beta-model"]);

    let global_skill = env.home.join("skills").join("shared-skill");
    let project_skill = env.workspace.join(".singularity").join("skills").join("shared-skill");
    std::fs::create_dir_all(&global_skill).expect("create global skill dir");
    std::fs::create_dir_all(&project_skill).expect("create project skill dir");
    std::fs::write(
        global_skill.join("SKILL.md"),
        "---\nname: shared-skill\ndescription: global version\n---\nglobal body\n",
    )
    .expect("write global skill");
    std::fs::write(
        project_skill.join("SKILL.md"),
        "---\nname: shared-skill\ndescription: project version\n---\nproject body\n",
    )
    .expect("write project skill");
    std::fs::write(env.workspace.join("hello.txt"), "workspace file content\n").expect("write sample file");

    let mut client = BackendClient::spawn(&env.home).await;

    let second = Command::new(env!("CARGO_BIN_EXE_singularity"))
        .env("SINGULARITY_HOME", &env.home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("run second backend");
    assert!(!second.status.success(), "second process on same SINGULARITY_HOME must fail");

    let ws = env.workspace_str();
    client.rpc_ok("workspace.add", json!({ "root": ws })).await;
    client
        .rpc_ok("instructions.save", json!({ "workspaceRoot": ws, "content": "PROJECT_RULE_MARKER" }))
        .await;
    let doc = client.rpc_ok("instructions.read", json!({ "workspaceRoot": ws })).await;
    assert_eq!(doc.get("content").and_then(Value::as_str), Some("PROJECT_RULE_MARKER"));

    let read_file = client.rpc_ok("file.read", json!({ "cwd": ws, "path": "hello.txt" })).await;
    assert!(
        read_file
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|c| c.contains("workspace file content"))
    );

    let skills = client.rpc_ok("skills.list", json!({ "workspaceRoot": ws })).await;
    let shared: Vec<&Value> = skills
        .get("skills")
        .and_then(Value::as_array)
        .expect("skills array")
        .iter()
        .filter(|s| s.get("name").and_then(Value::as_str) == Some("shared-skill"))
        .collect();
    assert_eq!(shared.len(), 1, "duplicate skill names must deduplicate: {shared:?}");
    assert_eq!(shared[0].get("description").and_then(Value::as_str), Some("project version"));

    let saved = client
        .rpc_ok(
            "model.saveProvider",
            json!({
                "provider": provider_config_json("local", "messages", "Local", &server.base_url, "alpha-model", 32000),
                "apiKey": "sk-super-secret-value"
            }),
        )
        .await;
    assert!(
        !saved.to_string().contains("sk-super-secret-value"),
        "RPC response must never leak apiKey"
    );
    assert_eq!(
        saved.pointer("/catalog/providers/0/credentialConfigured").and_then(Value::as_bool),
        Some(true)
    );

    let updated = client
        .rpc_ok(
            "model.saveProvider",
            json!({
                "provider": provider_config_json("local", "messages", "Local Renamed", &server.base_url, "alpha-model", 64000)
            }),
        )
        .await;
    assert_eq!(
        updated.pointer("/catalog/providers/0/credentialConfigured").and_then(Value::as_bool),
        Some(true)
    );

    let discovered = client
        .rpc_ok(
            "model.discover",
            json!({ "providerId": "local", "apiProtocol": "messages", "baseUrl": server.base_url, "apiKey": null }),
        )
        .await;
    let ids: Vec<&str> =
        discovered.as_array().expect("discovered").iter().filter_map(Value::as_str).collect();
    assert_eq!(ids, vec!["alpha-model", "beta-model"]);

    for cross_origin in [
        "https://other-provider.example.invalid/v1",
        "http://127.0.0.1:1/v1",
    ] {
        let save_cross_err = client
            .rpc_err(
                "model.saveProvider",
                json!({
                    "provider": provider_config_json("local", "messages", "Local Cross Origin", cross_origin, "alpha-model", 64000)
                }),
            )
            .await;
        assert!(!save_cross_err.is_empty(), "saveProvider must reject cross-origin reuse: {cross_origin}");

        let discover_cross_err = client
            .rpc_err(
                "model.discover",
                json!({ "providerId": "local", "apiProtocol": "messages", "baseUrl": cross_origin, "apiKey": null }),
            )
            .await;
        assert!(!discover_cross_err.is_empty(), "discover must reject cross-origin reuse: {cross_origin}");
    }

    let preview_before = client.rpc_ok("context.preview", json!({ "workspaceRoot": ws })).await;
    assert!(
        !preview_before
            .as_array()
            .expect("preview modules")
            .iter()
            .any(|m| m.get("id").and_then(Value::as_str) == Some("web")),
        "unconfigured web search must not expose web module"
    );

    let saved_web = client
        .rpc_ok(
            "model.saveWebSearch",
            json!({
                "baseUrl": "https://search.example.com/v1",
                "apiKey": "ws-super-secret-value"
            }),
        )
        .await;
    assert!(
        !saved_web.to_string().contains("ws-super-secret-value"),
        "model.saveWebSearch response must never leak apiKey"
    );
    assert_eq!(
        saved_web.pointer("/catalog/webSearch/credentialConfigured").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        saved_web.pointer("/catalog/webSearch/baseUrl").and_then(Value::as_str),
        Some("https://search.example.com/v1")
    );

    let same_origin_web = client
        .rpc_ok("model.saveWebSearch", json!({ "baseUrl": "https://search.example.com/v2" }))
        .await;
    assert_eq!(
        same_origin_web
            .pointer("/catalog/webSearch/credentialConfigured")
            .and_then(Value::as_bool),
        Some(true),
        "same-origin URL update must preserve stored web search credential"
    );

    let cross_origin_err = client
        .rpc_err("model.saveWebSearch", json!({ "baseUrl": "https://other.example.com/v1" }))
        .await;
    assert!(
        !cross_origin_err.is_empty(),
        "changing web search origin without providing a new apiKey must be rejected"
    );

    let preview_after = client.rpc_ok("context.preview", json!({ "workspaceRoot": ws })).await;
    assert!(
        preview_after
            .as_array()
            .expect("preview modules")
            .iter()
            .any(|m| m.get("id").and_then(Value::as_str) == Some("web")),
        "configured web search must expose web module in context preview"
    );

    let cleared_web = client
        .rpc_ok("model.saveWebSearch", json!({ "baseUrl": "https://search.example.com/v2", "apiKey": "" }))
        .await;
    assert_eq!(
        cleared_web.pointer("/catalog/webSearch/credentialConfigured").and_then(Value::as_bool),
        Some(false)
    );
    let preview_cleared = client.rpc_ok("context.preview", json!({ "workspaceRoot": ws })).await;
    assert!(
        !preview_cleared
            .as_array()
            .expect("preview modules")
            .iter()
            .any(|m| m.get("id").and_then(Value::as_str) == Some("web")),
        "clearing web search credential must hide web module"
    );

    client.shutdown().await;
}

#[tokio::test]
async fn dual_protocol_roundtrip_and_restart_persistence() {
    for (protocol, provider_id) in [("messages", "msg-provider"), ("responses", "resp-provider")] {
        let env = TestEnv::new(protocol);
        let server = MockModelServer::start().await;
        let ws = env.workspace_str();
        std::fs::write(env.workspace.join("probe.txt"), "PROBE_FILE_CONTENT\n").expect("write probe file");

        if protocol == "messages" {
            server.enqueue(ScriptedResponse::sse(messages_tools_sse(
                None,
                vec![("call_read_1", "read", json!({ "path": "probe.txt" }))],
                100,
                25,
                "tool_use",
            )));
            server.enqueue(ScriptedResponse::sse(messages_text_sse(
                Some(("first thought", "sig_round_1")),
                "first answer",
                120,
                30,
                40,
                "end_turn",
            )));
            server.enqueue(ScriptedResponse::sse(messages_text_sse(
                None,
                "second answer",
                200,
                50,
                25,
                "end_turn",
            )));
        } else {
            server.enqueue(ScriptedResponse::sse(responses_sse(
                None,
                None,
                vec![("call_read_1", "read", json!({ "path": "probe.txt" }))],
                100,
                0,
                25,
                false,
            )));
            server.enqueue(ScriptedResponse::sse(responses_sse(
                Some(("first thought", "enc_reasoning_1")),
                Some("first answer"),
                vec![],
                120,
                30,
                40,
                false,
            )));
            server.enqueue(ScriptedResponse::sse(responses_sse(
                None,
                Some("second answer"),
                vec![],
                200,
                50,
                25,
                false,
            )));
        }

        let mut client = BackendClient::spawn(&env.home).await;
        client
            .setup_workspace_and_provider(
                &ws,
                provider_id,
                protocol,
                &server.base_url,
                "demo-model",
                64000,
                4096,
            )
            .await;

        let session_id = client
            .start_session(
                &ws,
                provider_id,
                "demo-model",
                Some("high"),
                "hello round 1",
                vec![json!({ "name": "img.png", "dataUrl": TINY_PNG_DATA_URL })],
                vec![],
            )
            .await;
        client.wait_settled(&session_id).await;

        client.submit(&session_id, "hello round 2").await;
        client.wait_settled(&session_id).await;

        let reqs = server.model_requests();
        assert_eq!(reqs.len(), 3, "{protocol}: expected 3 requests (tool call + reply + round 2)");
        assert!(
            reqs[1].body.to_string().contains("PROBE_FILE_CONTENT"),
            "{protocol}: tool result must reach model"
        );
        let round2_text = reqs[2].body.to_string();
        if protocol == "messages" {
            assert!(
                round2_text.contains("sig_round_1"),
                "Messages protocol must echo thinking signature back to same provider"
            );
        } else {
            assert!(
                round2_text.contains("enc_reasoning_1"),
                "Responses protocol must echo encrypted reasoning back to same provider"
            );
        }

        let before_restart = client.rpc_ok("session.read", json!({ "sessionId": session_id })).await;
        server.assert_no_protocol_violations();
        client.shutdown().await;

        let mut restarted = BackendClient::spawn(&env.home).await;
        let bootstrap = restarted.rpc_ok("app.bootstrap", json!({})).await;
        let has_session = bootstrap
            .get("sessionsByWorkspace")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|map| map.values())
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(|s| s.get("threadId").and_then(Value::as_str))
            .any(|id| id == session_id);
        assert!(has_session, "{protocol}: restarted bootstrap must list session");

        let after_restart = restarted.rpc_ok("session.read", json!({ "sessionId": session_id })).await;
        assert_eq!(
            before_restart.pointer("/history/turns"),
            after_restart.pointer("/history/turns"),
            "{protocol}: persisted history must match across restart"
        );
        assert_eq!(
            before_restart.pointer("/runtime/modelUsage"),
            after_restart.pointer("/runtime/modelUsage"),
            "{protocol}: runtime modelUsage must match across restart"
        );
        assert_eq!(
            after_restart.pointer("/runtime/modelUsage/usagePresent").and_then(Value::as_bool),
            Some(true),
            "{protocol}: settled session must report runtime modelUsage"
        );
        let turns = after_restart.pointer("/history/turns").and_then(Value::as_array).expect("turns array");
        assert_eq!(turns.len(), 2, "{protocol}: should persist 2 turns");
        restarted.shutdown().await;
    }

    let env = TestEnv::new("cross-provider");
    let server = MockModelServer::start().await;
    let ws = env.workspace_str();
    std::fs::write(env.workspace.join("cross.txt"), "CROSS_PROTOCOL_FILE\n").expect("write cross file");

    let mut client = BackendClient::spawn(&env.home).await;
    client
        .setup_workspace_and_provider(&ws, "msg-a", "messages", &server.base_url, "m-a", 64000, 4096)
        .await;
    for (id, protocol, model) in [("resp-b", "responses", "r-b"), ("msg-c", "messages", "m-c")] {
        client
            .rpc_ok(
                "model.saveProvider",
                json!({
                    "provider": provider_config_json(id, protocol, id, &server.base_url, model, 64000),
                    "apiKey": "test-secret-key"
                }),
            )
            .await;
    }

    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        Some(("msg-a thought", "sig_private_to_msg_a")),
        "ANSWER_FROM_MSG_A",
        100,
        0,
        30,
        "end_turn",
    )));
    server.enqueue(ScriptedResponse::sse(responses_sse(
        None,
        None,
        vec![("resp_call_1", "read", json!({ "path": "cross.txt" }))],
        150,
        0,
        25,
        false,
    )));
    server.enqueue(ScriptedResponse::sse(responses_sse(
        Some(("resp-b thought", "enc_private_to_resp_b")),
        Some("ANSWER_FROM_RESP_B"),
        vec![],
        180,
        0,
        30,
        false,
    )));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "ANSWER_FROM_MSG_C",
        220,
        0,
        20,
        "end_turn",
    )));

    let session_id = client
        .start_session(&ws, "msg-a", "m-a", Some("high"), "turn 1 on msg-a", vec![], vec![])
        .await;
    client.wait_settled(&session_id).await;

    client
        .rpc_ok(
            "session.updateSettings",
            json!({ "sessionId": session_id, "selection": { "provider": "resp-b", "model": "r-b", "reasoning": "high" } }),
        )
        .await;
    client.submit(&session_id, "turn 2 on resp-b").await;
    client.wait_settled(&session_id).await;

    client
        .rpc_ok(
            "session.updateSettings",
            json!({ "sessionId": session_id, "selection": { "provider": "msg-c", "model": "m-c", "reasoning": "high" } }),
        )
        .await;
    client.submit(&session_id, "turn 3 on msg-c").await;
    client.wait_settled(&session_id).await;

    let reqs = server.model_requests();
    assert_eq!(reqs.len(), 4, "cross-provider session expected 4 upstream requests");
    let resp_req_text = reqs[1].body.to_string();
    assert!(resp_req_text.contains("ANSWER_FROM_MSG_A"));
    assert!(
        !resp_req_text.contains("sig_private_to_msg_a"),
        "switching to Responses must not leak Messages thinking signature"
    );

    let msg_c_req_text = reqs[3].body.to_string();
    assert!(
        msg_c_req_text.contains("CROSS_PROTOCOL_FILE"),
        "tool output from Responses must carry over"
    );
    assert!(msg_c_req_text.contains("ANSWER_FROM_RESP_B"));
    assert!(
        !msg_c_req_text.contains("sig_private_to_msg_a"),
        "switching to another Messages provider must not leak old provider signature"
    );
    assert!(
        !msg_c_req_text.contains("enc_private_to_resp_b"),
        "switching from Responses to Messages must not leak encrypted_content"
    );

    server.assert_no_protocol_violations();
    client.shutdown().await;
}

fn find_tool_result<'a>(body: &'a Value, call_id: &str) -> &'a Value {
    body.get("messages")
        .and_then(Value::as_array)
        .expect("messages array")
        .iter()
        .filter_map(|msg| msg.get("content").and_then(Value::as_array))
        .flatten()
        .find(|block| {
            block.get("type").and_then(Value::as_str) == Some("tool_result")
                && block.get("tool_use_id").and_then(Value::as_str) == Some(call_id)
        })
        .unwrap_or_else(|| panic!("missing tool_result for {call_id}"))
}

#[tokio::test]
async fn builtin_workspace_tools_and_attachments() {
    let env = TestEnv::new("builtin-tools");
    let server = MockModelServer::start().await;
    let ws = env.workspace_str();

    std::fs::create_dir_all(env.workspace.join(".git")).expect("create .git dir");
    std::fs::create_dir_all(env.workspace.join("ignored_dir")).expect("create ignored_dir");
    std::fs::create_dir_all(env.workspace.join("src")).expect("create src dir");
    std::fs::write(env.workspace.join(".gitignore"), "ignored_dir/\n").expect("write .gitignore");
    std::fs::write(env.workspace.join(".git/config"), "NEEDLE_TOKEN in git\n").expect("write .git/config");
    std::fs::write(env.workspace.join("ignored_dir/skip.rs"), "NEEDLE_TOKEN ignored\n")
        .expect("write ignored file");
    std::fs::write(env.workspace.join("src/match.rs"), "fn visible() { /* NEEDLE_TOKEN */ }\n")
        .expect("write src/match.rs");
    std::fs::write(env.workspace.join("src/crlf.txt"), "pre\r\nANCHOR_CRLF_LINE\r\npost\r\n")
        .expect("write src/crlf.txt");
    std::fs::write(env.workspace.join("bin.dat"), b"NEEDLE_TOKEN\0binary_payload")
        .expect("write binary file");
    let attached_path = env.workspace.join("attached.txt");
    std::fs::write(&attached_path, "ATTACHED_FILE_BODY\n").expect("write attached file");
    let attached_display = format!("{ws}/attached.txt");

    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![("call_w", "write", json!({ "path": "notes/demo.txt", "content": "line one\nline two\n" }))],
        100,
        30,
        "tool_use",
    )));
    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![(
            "call_e",
            "edit",
            json!({
                "path": "notes/demo.txt",
                "edits": [{ "old_text": "line two", "new_text": "line TWO edited" }]
            }),
        )],
        120,
        30,
        "tool_use",
    )));
    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![
            ("call_r_ok", "read", json!({ "path": "notes/demo.txt" })),
            ("call_r_bin", "read", json!({ "path": "bin.dat" })),
            ("call_r_zero", "read", json!({ "path": "notes/demo.txt", "limit": 0 })),
            ("call_grep_dir", "grep", json!({ "pattern": "NEEDLE_TOKEN" })),
            (
                "call_grep_crlf",
                "grep",
                json!({ "pattern": "^ANCHOR_CRLF_LINE$", "path": "src/crlf.txt" }),
            ),
            ("call_glob_rs", "glob", json!({ "pattern": "*.rs" })),
            (
                "call_todo",
                "todo",
                json!({
                    "todos": [
                        { "content": "verify tools", "status": "in_progress" },
                        { "content": "wrap up", "status": "pending" }
                    ]
                }),
            ),
            (
                "call_bash",
                "bash",
                json!({
                    "command": "printf 'BASH_OK\\n'; yes '0123456789abcdef0123456789abcdef' | head -n 4000",
                    "description": "produce large stdout"
                }),
            ),
        ],
        150,
        60,
        "tool_use",
    )));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "all workspace tools verified",
        200,
        0,
        20,
        "end_turn",
    )));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "attachment turn verified",
        230,
        0,
        20,
        "end_turn",
    )));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "search modules disabled",
        100,
        0,
        15,
        "end_turn",
    )));

    let mut client = BackendClient::spawn(&env.home).await;
    client
        .setup_workspace_and_provider(&ws, "local", "messages", &server.base_url, "demo", 64000, 4096)
        .await;

    let session_id = client
        .start_session(
            &ws,
            "local",
            "demo",
            None,
            "run builtin workspace checks",
            vec![json!({ "name": "pixel.png", "dataUrl": TINY_PNG_DATA_URL })],
            vec![],
        )
        .await;
    client.wait_settled(&session_id).await;

    let disk_content = std::fs::read_to_string(env.workspace.join("notes").join("demo.txt"))
        .expect("demo.txt must exist on disk");
    assert!(disk_content.contains("line TWO edited"), "unexpected file content: {disk_content}");

    let reqs = server.model_requests();
    let final_req = &reqs.last().expect("final request").body;
    let final_text = final_req.to_string();
    assert!(final_text.contains("line TWO edited"), "read result must reach model");
    assert!(final_text.contains("BASH_OK"), "head+tail bash truncation must preserve opening output");
    assert!(final_text.contains("call_r_bin"), "binary read result must be paired");
    assert!(final_text.contains("call_r_zero"), "limit=0 read result must be paired");

    let todo_res = find_tool_result(final_req, "call_todo");
    assert_ne!(todo_res.get("is_error").and_then(Value::as_bool), Some(true), "todo tool must succeed");
    assert!(!todo_res.to_string().is_empty(), "todo tool result must reach model");

    let grep_dir_res = find_tool_result(final_req, "call_grep_dir");
    assert_ne!(grep_dir_res.get("is_error").and_then(Value::as_bool), Some(true));
    let grep_dir_text = grep_dir_res.to_string();
    assert!(grep_dir_text.contains("src/match.rs"), "grep must find visible file: {grep_dir_text}");
    assert!(!grep_dir_text.contains("ignored_dir"), "grep must respect .gitignore: {grep_dir_text}");
    assert!(!grep_dir_text.contains(".git/config"), "grep must skip .git: {grep_dir_text}");
    assert!(!grep_dir_text.contains("bin.dat"), "grep must skip binary file: {grep_dir_text}");

    let grep_crlf_res = find_tool_result(final_req, "call_grep_crlf");
    assert_ne!(grep_crlf_res.get("is_error").and_then(Value::as_bool), Some(true));
    assert!(
        grep_crlf_res.to_string().contains("ANCHOR_CRLF_LINE"),
        "grep ^...$ must match CRLF lines: {grep_crlf_res}"
    );

    let glob_rs_res = find_tool_result(final_req, "call_glob_rs");
    assert_ne!(glob_rs_res.get("is_error").and_then(Value::as_bool), Some(true));
    let glob_rs_text = glob_rs_res.to_string();
    assert!(
        glob_rs_text.contains("src/match.rs"),
        "glob must match workspace .rs files: {glob_rs_text}"
    );
    assert!(!glob_rs_text.contains("ignored_dir"), "glob must respect .gitignore: {glob_rs_text}");
    assert!(!glob_rs_text.contains(".git"), "glob must skip .git: {glob_rs_text}");

    let outputs_dir = env.home.join("sessions").join("outputs").join(&session_id);
    assert!(
        outputs_dir.exists() && std::fs::read_dir(&outputs_dir).expect("read outputs").next().is_some(),
        "large bash output must spill to sessions/outputs/<id>/"
    );

    let rel_err = client
        .rpc_err(
            "session.submit",
            json!({ "sessionId": session_id, "text": "", "images": [], "files": ["attached.txt"] }),
        )
        .await;
    assert!(!rel_err.is_empty(), "relative attachment path must be rejected");
    let missing_err = client
        .rpc_err(
            "session.submit",
            json!({ "sessionId": session_id, "text": "", "images": [], "files": [format!("{ws}/no_such_file.txt")] }),
        )
        .await;
    assert!(!missing_err.is_empty(), "nonexistent attachment path must be rejected");

    client
        .rpc_ok(
            "session.submit",
            json!({
                "sessionId": session_id,
                "text": "check attached file",
                "images": [],
                "files": [attached_display]
            }),
        )
        .await;
    client.wait_settled(&session_id).await;
    let attach_req = server.model_requests().last().expect("attach req").body.to_string();
    assert!(attach_req.contains("attached.txt"), "attachment path must be delivered to model");

    let read_before = client.rpc_ok("session.read", json!({ "sessionId": session_id })).await;
    let has_valid_todo = read_before
        .pointer("/history/turns/0/items")
        .and_then(Value::as_array)
        .expect("turn 0 items")
        .iter()
        .any(|item| {
            item.get("type").and_then(Value::as_str) == Some("tool")
                && item.get("name").and_then(Value::as_str) == Some("todo")
                && item.get("status").and_then(Value::as_str) == Some("completed")
                && item.pointer("/result/isError").and_then(Value::as_bool) == Some(false)
        });
    assert!(has_valid_todo, "persisted history must contain completed non-error todo tool call");
    let image_id = read_before
        .pointer("/history/turns/0/items/0/images/0/id")
        .and_then(Value::as_str)
        .expect("uploaded image id")
        .to_string();

    let s_disabled = client
        .start_session(
            &ws,
            "local",
            "demo",
            None,
            "check disabled search tools",
            vec![],
            vec!["grep", "glob"],
        )
        .await;
    client.wait_settled(&s_disabled).await;
    let disabled_reqs = server.model_requests();
    let disabled_tools: Vec<&str> = disabled_reqs
        .last()
        .expect("disabled search request")
        .body
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array")
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    assert!(
        !disabled_tools.contains(&"grep") && !disabled_tools.contains(&"glob"),
        "disabledModules must hide grep and glob tools from model: {disabled_tools:?}"
    );

    server.assert_no_protocol_violations();
    client.shutdown().await;

    let mut restarted = BackendClient::spawn(&env.home).await;
    let data_url = restarted
        .rpc_ok("session.imageRead", json!({ "sessionId": session_id, "imageId": image_id }))
        .await;
    assert!(
        data_url.as_str().is_some_and(|s| s.starts_with("data:image/png;base64,")),
        "restarted backend must serve persisted image"
    );
    let after_restart = restarted.rpc_ok("session.read", json!({ "sessionId": session_id })).await;
    assert!(
        after_restart.to_string().contains("attached.txt"),
        "restarted backend must preserve attached file metadata"
    );
    restarted.shutdown().await;
}
