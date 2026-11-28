use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::harness::{
    BackendClient, DEFAULT_TIMEOUT, MockModelServer, ScriptedResponse, TestEnv, find_tool_result,
    messages_text_sse, messages_tools_sse, provider_config_json, responses_sse, write_test_mcp_script,
};

#[tokio::test]
async fn mcp_lazy_loading_and_module_gating() {
    let env = TestEnv::new("mcp-gating");
    let server = MockModelServer::start().await;
    let ws = env.workspace_str();
    let mcp_script = write_test_mcp_script(&env.home);

    let skill_dir = env.workspace.join(".singularity").join("skills").join("demo-skill");
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: demo skill desc\n---\nSKILL_SECRET_BODY\n",
    )
    .expect("write skill");

    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![("c_schema", "mcp_schema", json!({ "server": "probe", "tools": ["echo"] }))],
        100,
        30,
        "tool_use",
    )));
    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![(
            "c_call",
            "mcp_call",
            json!({ "server": "probe", "tool": "echo", "arguments": { "text": "hello-mcp" } }),
        )],
        120,
        30,
        "tool_use",
    )));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(None, "mcp completed", 150, 0, 20, "end_turn")));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(None, "gated completed", 100, 0, 20, "end_turn")));

    let mut client = BackendClient::spawn(&env.home).await;
    client
        .setup_workspace_and_provider(&ws, "local", "messages", &server.base_url, "demo", 64000, 4096)
        .await;
    client.register_test_mcp(&ws, &mcp_script).await;
    client
        .rpc_ok(
            "model.saveWebSearch",
            json!({ "baseUrl": "https://search.example.com/v1", "apiKey": "ws-test-key" }),
        )
        .await;

    let s1 = client
        .start_session(&ws, "local", "demo", None, "inspect and call mcp", vec![], vec![])
        .await;
    client.wait_settled(&s1).await;

    let reqs = server.model_requests();
    assert_eq!(reqs.len(), 3);
    let system_text = reqs[0].body.get("system").map(Value::to_string).unwrap_or_default();
    assert!(system_text.contains("TEST_MCP_INSTRUCTIONS_MARKER"));
    assert!(system_text.contains("ping"));
    let tool_names: Vec<&str> = reqs[0]
        .body
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools")
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    assert!(tool_names.contains(&"mcp_schema") && tool_names.contains(&"mcp_call"));
    assert!(
        tool_names.contains(&"web_search") && tool_names.contains(&"web_fetch"),
        "configured web search must expose web_search and web_fetch tools when enabled"
    );
    assert!(
        reqs[1].body.to_string().contains("delay_ms"),
        "mcp_schema must return full parameter schema"
    );
    assert!(reqs[2].body.to_string().contains("ECHO:hello-mcp"), "mcp_call result must reach model");

    let s2 = client
        .start_session(
            &ws,
            "local",
            "demo",
            None,
            "run without mcp and skills",
            vec![],
            vec!["mcp:probe", "skills", "web"],
        )
        .await;
    client.wait_settled(&s2).await;

    let gated_req = &server.model_requests()[3].body;
    let gated_system = gated_req.get("system").map(Value::to_string).unwrap_or_default();
    let gated_tools: Vec<&str> = gated_req
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools")
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    assert!(!gated_system.contains("TEST_MCP_INSTRUCTIONS_MARKER"));
    assert!(!gated_system.contains("demo-skill"));
    assert!(!gated_tools.contains(&"mcp_call"));
    assert!(!gated_tools.contains(&"skill"));
    assert!(
        !gated_tools.contains(&"web_search") && !gated_tools.contains(&"web_fetch"),
        "disabling web module must hide web_search and web_fetch tools"
    );

    let http_url = format!(
        "{}?token=http_tok_99&mode=fast",
        server.base_url.replacen("http://", "http://mcp_user_1:mcp_pass_1@", 1)
    );
    server.enqueue(ScriptedResponse::json(
        401,
        json!({ "error": "denied mcp_user_1 mcp_pass_1 http_tok_99 Bearer hdr_secret_42 public_ok" }),
    ));
    let secrets = [
        "mcp_user_1",
        "mcp_pass_1",
        "http_tok_99",
        "hdr_secret_42",
        "mcp_user_2",
        "mcp_pass_2",
        "stdio_tok_77",
        "svc_secret_88",
    ];
    let leaky_servers = [
        (
            "redact-http",
            json!({
                "type": "http", "url": http_url,
                "headers": { "Authorization": "Bearer hdr_secret_42", "X-Custom": "public_ok" }
            }),
        ),
        (
            "redact-stdio",
            json!({
                "type": "stdio", "command": "node", "cwd": ws,
                "args": [
                    "-e",
                    "console.error('err', process.env.serviceKey, process.env.PUBLIC_TAG, process.argv.slice(1).join(' ')); process.exit(1)",
                    "--",
                    "https://mcp_user_2:mcp_pass_2@example.invalid/sse?token=stdio_tok_77"
                ],
                "env": { "serviceKey": "svc_secret_88", "PUBLIC_TAG": "public_ok" }
            }),
        ),
    ];
    for (id, transport) in leaky_servers {
        let cfg =
            json!({ "serverId": id, "startupTimeoutSec": 5, "toolTimeoutSec": 5, "transport": transport });
        client.rpc_ok("mcp.save", json!({ "workspaceRoot": ws, "server": cfg })).await;
        let state = client.rpc_ok("mcp.reconnect", json!({ "workspaceRoot": ws, "serverId": id })).await;
        let err = state.get("error").and_then(Value::as_str).expect("redacted mcp error");
        assert!(err.contains("[redacted]") && err.contains("public_ok"), "{id}: {err}");
        assert!(secrets.iter().all(|secret| !err.contains(secret)), "{id} leaked secret in `{err}`");
    }
    let listed = client.rpc_ok("mcp.list", json!({ "workspaceRoot": ws })).await;
    let listed_errors: Vec<&str> = listed
        .as_array()
        .expect("mcp.list array")
        .iter()
        .filter_map(|entry| entry.get("error").and_then(Value::as_str))
        .collect();
    assert_eq!(listed_errors.len(), 2, "expected 2 failed mcp entries in mcp.list");
    for err in listed_errors {
        assert!(err.contains("[redacted]") && secrets.iter().all(|s| !err.contains(s)), "mcp.list: {err}");
    }

    client.shutdown().await;
}

#[tokio::test]
async fn tool_scheduling_question_barrier_and_queued_input() {
    let env = TestEnv::new("scheduling-invariants");
    let server = MockModelServer::start().await;
    let ws = env.workspace_str();
    let mcp_script = write_test_mcp_script(&env.home);

    let skill_dir = env.workspace.join(".singularity").join("skills").join("snap-skill");
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    let skill_file = skill_dir.join("SKILL.md");
    std::fs::write(&skill_file, "---\nname: snap-skill\ndescription: snapshot\n---\nORIGINAL_SKILL_V1\n")
        .expect("write skill v1");

    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![
            (
                "c_fast",
                "mcp_call",
                json!({ "server": "probe", "tool": "echo", "arguments": { "text": "fast", "delay_ms": 120 } }),
            ),
            (
                "c_fail",
                "mcp_call",
                json!({ "server": "probe", "tool": "fail_tool", "arguments": {} }),
            ),
            (
                "c_big",
                "mcp_call",
                json!({ "server": "probe", "tool": "large_output", "arguments": { "kb": 140 } }),
            ),
            (
                "c_ask",
                "ask_user_question",
                json!({
                    "questions": [{
                        "id": "q1",
                        "question": "Pick deployment mode",
                        "options": [{ "label": "canary" }, { "label": "full" }]
                    }]
                }),
            ),
        ],
        100,
        50,
        "tool_use",
    )));

    let stream_gate = Arc::new(Notify::new());
    server.enqueue(ScriptedResponse::sse_gated(
        messages_text_sse(None, "after question", 180, 0, 20, "end_turn"),
        Arc::clone(&stream_gate),
    ));

    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "consumed queued input",
        220,
        0,
        20,
        "end_turn",
    )));

    let mut client = BackendClient::spawn(&env.home).await;
    client
        .setup_workspace_and_provider(&ws, "local", "messages", &server.base_url, "demo", 64000, 4096)
        .await;
    client.register_test_mcp(&ws, &mcp_script).await;

    let session_id = client
        .start_session(&ws, "local", "demo", None, "start parallel + question", vec![], vec![])
        .await;

    let question_frame = client
        .wait_event(DEFAULT_TIMEOUT, |frame| {
            frame.get("type").and_then(Value::as_str) == Some("session_changed")
                && frame.get("sessionId").and_then(Value::as_str) == Some(session_id.as_str())
                && frame.pointer("/payload/pendingQuestion/itemId").and_then(Value::as_str).is_some()
        })
        .await;
    let question_item_id = question_frame
        .pointer("/payload/pendingQuestion/itemId")
        .and_then(Value::as_str)
        .expect("question item id")
        .to_string();
    assert_eq!(
        question_frame.pointer("/payload/modelUsage/usagePresent").and_then(Value::as_bool),
        Some(true),
        "running session runtime must push live modelUsage after tool-only model step: {question_frame}"
    );
    assert_eq!(
        question_frame.pointer("/payload/modelUsage/totalTokens").and_then(Value::as_u64),
        Some(150),
        "running session runtime must accumulate tokens from prior model step: {question_frame}"
    );

    let events_before_question = client.take_buffered_events();
    let finished_tool_count = events_before_question
        .iter()
        .filter(|e| e.pointer("/payload/method").and_then(Value::as_str) == Some("tool/finished"))
        .count();
    assert_eq!(
        finished_tool_count, 3,
        "all preceding parallel tools must finish before ask_user_question pauses"
    );

    client
        .rpc_ok(
            "session.answerQuestion",
            json!({
                "sessionId": session_id,
                "itemId": question_item_id,
                "answers": [{ "id": "q1", "selected": ["canary"], "text": "", "skipped": false }]
            }),
        )
        .await;

    server.wait_for_request_count(2).await;
    client.submit(&session_id, "follow up with /snap-skill").await;

    std::fs::write(&skill_file, "---\nname: snap-skill\ndescription: snapshot\n---\nMUTATED_SKILL_V2\n")
        .expect("mutate skill v2");

    stream_gate.notify_one();
    client.wait_settled(&session_id).await;

    let reqs = server.model_requests();
    assert_eq!(reqs.len(), 3, "queued input must trigger follow-up model call before settling");
    let req2 = reqs[1].body.to_string();
    assert!(req2.contains("ECHO:fast"));
    assert!(req2.contains("SIMULATED_MCP_FAILURE"));
    assert!(req2.contains("canary"));
    let req3 = reqs[2].body.to_string();
    assert!(
        req3.contains("ORIGINAL_SKILL_V1"),
        "queued input must preserve skill snapshot at submit time"
    );
    assert!(
        !req3.contains("MUTATED_SKILL_V2"),
        "disk mutation after enqueue must not alter queued input"
    );

    let outputs_dir = env.home.join("sessions").join("outputs").join(&session_id);
    assert!(outputs_dir.exists(), "large MCP tool output must spill to outputs directory");

    client.shutdown().await;
}

#[tokio::test]
async fn background_bash_resume_and_cleanup() {
    let env = TestEnv::new("bg-bash");
    let server = MockModelServer::start().await;
    let ws = env.workspace_str();

    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![(
            "bg_short",
            "bash",
            json!({
                "command": "while [ ! -f bg_go ]; do sleep 0.05; done; echo BG_DONE_MARKER",
                "description": "short background job",
                "yield_time_ms": 0
            }),
        )],
        100,
        30,
        "tool_use",
    )));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "started background job",
        130,
        0,
        20,
        "end_turn",
    )));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "observed background completion",
        160,
        0,
        20,
        "end_turn",
    )));

    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![(
            "bg_long",
            "bash",
            json!({
                "command": "sleep 60",
                "description": "long background job",
                "yield_time_ms": 0
            }),
        )],
        180,
        30,
        "tool_use",
    )));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "long job started",
        200,
        0,
        20,
        "end_turn",
    )));

    let mut client = BackendClient::spawn(&env.home).await;
    client
        .setup_workspace_and_provider(&ws, "local", "messages", &server.base_url, "demo", 64000, 4096)
        .await;

    let session_id = client.start_session(&ws, "local", "demo", None, "run bg task", vec![], vec![]).await;

    client.wait_settled(&session_id).await;
    std::fs::write(env.workspace.join("bg_go"), "go\n").expect("write background sentinel");
    client.wait_settled(&session_id).await;

    let reqs = server.model_requests();
    assert_eq!(reqs.len(), 3, "background completion must auto-resume follow-up turn");
    assert!(
        reqs[2].body.to_string().contains("BG_DONE_MARKER"),
        "auto-resumed turn must deliver background output to model"
    );

    client.submit(&session_id, "start long job").await;
    client.wait_settled(&session_id).await;

    let read = client.rpc_ok("session.read", json!({ "sessionId": session_id })).await;
    let long_task_id = read
        .get("backgroundTasks")
        .and_then(Value::as_array)
        .expect("backgroundTasks")
        .iter()
        .find(|t| t.get("description").and_then(Value::as_str) == Some("long background job"))
        .and_then(|t| t.get("taskId").and_then(Value::as_str))
        .expect("long task id")
        .to_string();

    client
        .rpc_ok("session.stopBackgroundTask", json!({ "sessionId": session_id, "taskId": long_task_id }))
        .await;

    let stop_event = client
        .wait_event(Duration::from_secs(10), |frame| {
            frame.get("type").and_then(Value::as_str) == Some("background_task_updated")
                && frame.pointer("/payload/task/taskId").and_then(Value::as_str)
                    == Some(long_task_id.as_str())
                && frame.pointer("/payload/task/state/status").and_then(Value::as_str) != Some("running")
        })
        .await;
    assert_eq!(stop_event.pointer("/payload/task/state/status").and_then(Value::as_str), Some("stopped"));

    client.shutdown().await;
}

#[tokio::test]
async fn web_tools_http_boundary_invariants() {
    let env = TestEnv::new("web-http-boundary");
    let model_server = MockModelServer::start().await;
    let web_server = MockModelServer::start().await;
    let ws = env.workspace_str();
    let web_secret = "ws-bearer-secret-99";

    web_server.set_fallback(move |req, _| {
        if req.path == "/v1/tools/search" {
            return match req.body.get("query").and_then(Value::as_str) {
                Some("ok-search") => ScriptedResponse::json(
                    200,
                    json!({
                        "results": [{
                            "title": "Singularity Docs",
                            "url": "https://docs.example.test/ok",
                            "summary": "WEB_SEARCH_RESULT_MARKER"
                        }]
                    }),
                ),
                _ => ScriptedResponse::json(
                    401,
                    json!({ "error": format!("auth rejected Bearer {web_secret} raw={web_secret}") }),
                ),
            };
        }
        if req.path == "/v1/tools/fetch" {
            return match req.body.get("url").and_then(Value::as_str) {
                Some("https://docs.example.test/ok") => ScriptedResponse::json(
                    200,
                    json!({ "title": "Guide Page", "content": "WEB_FETCH_CONTENT_MARKER" }),
                ),
                _ => ScriptedResponse::json_with_headers(
                    302,
                    vec![("Location", "https://redirected.example.test/target")],
                    json!({ "message": "moved" }),
                ),
            };
        }
        ScriptedResponse::json(404, json!({ "error": "unexpected path" }))
    });

    model_server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![
            ("c_search_ok", "web_search", json!({ "query": "ok-search", "count": 3 })),
            ("c_fetch_ok", "web_fetch", json!({ "url": "https://docs.example.test/ok" })),
            ("c_fetch_redir", "web_fetch", json!({ "url": "https://docs.example.test/redir" })),
            ("c_search_err", "web_search", json!({ "query": "err-search" })),
        ],
        120,
        50,
        "tool_use",
    )));
    model_server.enqueue(ScriptedResponse::sse(messages_text_sse(
        None,
        "web boundary verified",
        200,
        0,
        20,
        "end_turn",
    )));

    let mut client = BackendClient::spawn(&env.home).await;
    client
        .setup_workspace_and_provider(&ws, "local", "messages", &model_server.base_url, "demo", 64000, 4096)
        .await;
    client
        .rpc_ok("model.saveWebSearch", json!({ "baseUrl": web_server.origin_url(), "apiKey": web_secret }))
        .await;

    let session_id = client
        .start_session(&ws, "local", "demo", None, "exercise web search and fetch", vec![], vec![])
        .await;
    let settled = client.wait_settled(&session_id).await;
    assert_eq!(settled.pointer("/summary/status").and_then(Value::as_str), Some("completed"));

    let web_reqs = web_server.all_requests();
    assert_eq!(web_reqs.len(), 4, "expected 4 direct web tool HTTP calls without following redirects");
    let expected_auth = format!("Bearer {web_secret}");
    assert!(
        web_reqs.iter().all(|req| req.authorization.as_deref() == Some(expected_auth.as_str())),
        "all web tool requests must send Bearer authorization header"
    );

    let model_reqs = model_server.model_requests();
    let final_req = &model_reqs.last().expect("final model req").body;

    let search_ok = find_tool_result(final_req, "c_search_ok");
    assert_ne!(search_ok.get("is_error").and_then(Value::as_bool), Some(true));
    assert!(search_ok.to_string().contains("WEB_SEARCH_RESULT_MARKER"));

    let fetch_ok = find_tool_result(final_req, "c_fetch_ok");
    assert_ne!(fetch_ok.get("is_error").and_then(Value::as_bool), Some(true));
    assert!(fetch_ok.to_string().contains("WEB_FETCH_CONTENT_MARKER"));

    let fetch_redir = find_tool_result(final_req, "c_fetch_redir");
    assert_eq!(fetch_redir.get("is_error").and_then(Value::as_bool), Some(true));
    assert!(
        fetch_redir.to_string().contains("302"),
        "302 redirect must be rejected as an error without following: {fetch_redir}"
    );

    let search_err = find_tool_result(final_req, "c_search_err");
    assert_eq!(search_err.get("is_error").and_then(Value::as_bool), Some(true));
    let err_text = search_err.to_string();
    assert!(
        !err_text.contains(web_secret),
        "HTTP error response must redact web Bearer credential: {err_text}"
    );
    assert!(
        err_text.contains("[redacted]"),
        "redaction marker expected in error diagnostic: {err_text}"
    );

    let history = client.rpc_ok("session.read", json!({ "sessionId": session_id })).await;
    assert!(
        !history.to_string().contains(web_secret),
        "persisted session history must never contain web search secret"
    );

    model_server.assert_no_protocol_violations();
    client.shutdown().await;
}

fn provider_ids(catalog: &Value) -> Vec<&str> {
    catalog
        .pointer("/catalog/providers")
        .and_then(Value::as_array)
        .expect("providers array")
        .iter()
        .filter_map(|p| p.get("providerId").and_then(Value::as_str))
        .collect()
}

fn mcp_server_ids(list: &Value) -> Vec<&str> {
    list.as_array()
        .expect("mcp list array")
        .iter()
        .filter_map(|e| e.pointer("/server/serverId").and_then(Value::as_str))
        .collect()
}

#[tokio::test]
async fn model_remove_provider_invariants() {
    let env = TestEnv::new("model-remove-provider");
    let server = MockModelServer::start().await;
    let ws = env.workspace_str();
    server.enqueue(ScriptedResponse::sse(responses_sse(
        None,
        Some("SECONDARY_OK"),
        vec![],
        80,
        0,
        15,
        false,
    )));

    let mut client = BackendClient::spawn(&env.home).await;
    client.rpc_ok("workspace.add", json!({ "root": ws })).await;
    for (id, proto, model, key) in [
        ("primary", "messages", "m-primary", "sk-primary-secret"),
        ("secondary", "responses", "m-secondary", "sk-secondary-secret"),
    ] {
        let provider = provider_config_json(id, proto, id, &server.base_url, model, 64000, 4096);
        client.rpc_ok("model.saveProvider", json!({ "provider": provider, "apiKey": key })).await;
    }
    let web_cfg = json!({ "baseUrl": "https://search.example.com/v1", "apiKey": "sk-web-secret" });
    client.rpc_ok("model.saveWebSearch", web_cfg).await;

    let before = client.rpc_ok("model.read", json!({})).await;
    assert_eq!(provider_ids(&before), vec!["primary", "secondary"]);
    assert_eq!(
        before.pointer("/catalog/webSearch/credentialConfigured").and_then(Value::as_bool),
        Some(true)
    );

    let missing = client.rpc_err("model.removeProvider", json!({ "providerId": "missing" })).await;
    assert!(!missing.is_empty(), "removing nonexistent provider must fail explicitly");

    let after = client.rpc_ok("model.removeProvider", json!({ "providerId": "primary" })).await;
    let after_text = after.to_string();
    assert!(
        !after_text.contains("sk-primary-secret")
            && !after_text.contains("sk-secondary-secret")
            && !after_text.contains("sk-web-secret"),
        "model.removeProvider response must never leak credentials"
    );
    assert_eq!(provider_ids(&after), vec!["secondary"]);
    assert_eq!(
        after.pointer("/catalog/providers/0/credentialConfigured").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        after.pointer("/catalog/webSearch/credentialConfigured").and_then(Value::as_bool),
        Some(true)
    );

    let disk: Value = serde_json::from_str(
        &std::fs::read_to_string(env.home.join("config.json")).expect("read config.json"),
    )
    .expect("parse config.json");
    assert!(disk.pointer("/providers/primary").is_none());
    assert!(disk.pointer("/providers/secondary").is_some());
    assert_eq!(disk.pointer("/web_search/api_key").and_then(Value::as_str), Some("sk-web-secret"));

    let sid = client
        .start_session(&ws, "secondary", "m-secondary", None, "use remaining", vec![], vec![])
        .await;
    assert_eq!(
        client.wait_settled(&sid).await.pointer("/summary/status").and_then(Value::as_str),
        Some("completed")
    );
    let sel_err = client
        .rpc_err(
            "session.updateSettings",
            json!({ "sessionId": sid, "selection": { "provider": "primary", "model": "m-primary", "reasoning": null } }),
        )
        .await;
    assert!(!sel_err.is_empty(), "selecting removed provider must be rejected");

    server.assert_no_protocol_violations();
    client.shutdown().await;

    let mut restarted = BackendClient::spawn(&env.home).await;
    let boot = restarted.rpc_ok("app.bootstrap", json!({})).await;
    assert!(boot.pointer("/models/error").is_none_or(Value::is_null));
    let reread = restarted.rpc_ok("model.read", json!({})).await;
    assert_eq!(provider_ids(&reread), vec!["secondary"]);
    assert_eq!(
        reread.pointer("/catalog/webSearch/credentialConfigured").and_then(Value::as_bool),
        Some(true)
    );

    let emptied = restarted.rpc_ok("model.removeProvider", json!({ "providerId": "secondary" })).await;
    assert!(provider_ids(&emptied).is_empty());
    assert_eq!(
        emptied.pointer("/catalog/webSearch/credentialConfigured").and_then(Value::as_bool),
        Some(true)
    );
    assert!(
        restarted
            .rpc_ok("app.bootstrap", json!({}))
            .await
            .pointer("/models/error")
            .is_none_or(Value::is_null)
    );
    restarted.shutdown().await;
}

#[tokio::test]
async fn mcp_remove_live_connection_invariants() {
    let env = TestEnv::new("mcp-remove");
    let server = MockModelServer::start().await;
    let ws = env.workspace_str();
    let mcp_script = write_test_mcp_script(&env.home);

    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![("c_ping", "mcp_call", json!({ "server": "probe", "tool": "ping", "arguments": {} }))],
        100,
        25,
        "tool_use",
    )));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(None, "MCP_PING_OK", 140, 0, 20, "end_turn")));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(None, "POST_REMOVE_OK", 100, 0, 20, "end_turn")));

    let mut client = BackendClient::spawn(&env.home).await;
    client
        .setup_workspace_and_provider(&ws, "local", "messages", &server.base_url, "demo", 64000, 4096)
        .await;
    client.register_test_mcp(&ws, &mcp_script).await;
    let keep_cfg = json!({
        "serverId": "keep", "startupTimeoutSec": 20, "toolTimeoutSec": 20,
        "transport": { "type": "stdio", "command": "node", "args": [mcp_script.to_string_lossy()], "cwd": ws, "env": {} }
    });
    client.rpc_ok("mcp.save", json!({ "workspaceRoot": ws, "server": keep_cfg })).await;

    let s1 = client.start_session(&ws, "local", "demo", None, "connect mcp", vec![], vec![]).await;
    assert_eq!(
        client.wait_settled(&s1).await.pointer("/summary/status").and_then(Value::as_str),
        Some("completed")
    );
    assert!(server.model_requests()[1].body.to_string().contains("PONG"));

    let before_list = client.rpc_ok("mcp.list", json!({ "workspaceRoot": ws })).await;
    let probe = before_list
        .as_array()
        .expect("mcp.list array")
        .iter()
        .find(|e| e.pointer("/server/serverId").and_then(Value::as_str) == Some("probe"))
        .expect("probe entry");
    assert_eq!(probe.get("status").and_then(Value::as_str), Some("connected"));

    let missing = client.rpc_err("mcp.remove", json!({ "workspaceRoot": ws, "serverId": "missing" })).await;
    assert!(!missing.is_empty(), "removing nonexistent MCP server must fail explicitly");

    let removed = client.rpc_ok("mcp.remove", json!({ "workspaceRoot": ws, "serverId": "probe" })).await;
    assert_eq!(mcp_server_ids(&removed), vec!["keep"]);
    let after = client.rpc_ok("mcp.list", json!({ "workspaceRoot": ws })).await;
    assert_eq!(mcp_server_ids(&after), vec!["keep"]);

    let disk: Value =
        serde_json::from_str(&std::fs::read_to_string(env.home.join("mcp.json")).expect("read mcp.json"))
            .expect("parse mcp.json");
    assert!(disk.pointer("/mcpServers/probe").is_none());
    assert!(disk.pointer("/mcpServers/keep").is_some());

    let s2 = client.start_session(&ws, "local", "demo", None, "post remove", vec![], vec![]).await;
    assert_eq!(
        client.wait_settled(&s2).await.pointer("/summary/status").and_then(Value::as_str),
        Some("completed")
    );
    let s2_sys = server
        .model_requests()
        .last()
        .expect("post-remove req")
        .body
        .get("system")
        .map(Value::to_string)
        .unwrap_or_default();
    assert!(s2_sys.contains("MCP server keep:") && !s2_sys.contains("MCP server probe:"));

    server.assert_no_protocol_violations();
    client.shutdown().await;

    let mut restarted = BackendClient::spawn(&env.home).await;
    let restarted_list = restarted.rpc_ok("mcp.list", json!({ "workspaceRoot": ws })).await;
    assert_eq!(mcp_server_ids(&restarted_list), vec!["keep"]);
    let final_list = restarted.rpc_ok("mcp.remove", json!({ "workspaceRoot": ws, "serverId": "keep" })).await;
    assert!(mcp_server_ids(&final_list).is_empty());
    restarted.shutdown().await;
}
