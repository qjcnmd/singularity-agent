use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::harness::{
    BackendClient, DEFAULT_TIMEOUT, MockModelServer, ScriptedResponse, TestEnv, messages_text_sse,
    messages_tools_sse, write_test_mcp_script,
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
            vec!["mcp:probe", "skills"],
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
                && frame.pointer("/payload/task/state/kind").and_then(Value::as_str) != Some("running")
        })
        .await;
    assert_ne!(stop_event.pointer("/payload/task/state/kind").and_then(Value::as_str), Some("running"));

    client.shutdown().await;
}
