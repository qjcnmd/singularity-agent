use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::harness::{
    BackendClient, DEFAULT_TIMEOUT, MockModelServer, ScriptedResponse, TINY_PNG_DATA_URL, TestEnv,
    messages_text_sse, messages_tools_sse,
};

fn run_git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(dir)
        .args(["-c", "core.autocrlf=false"])
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

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

    client.rpc_ok("workspace.remove", json!({ "workspaceRoot": ws })).await;
    let boot_removed = client.rpc_ok("app.bootstrap", json!({})).await;
    assert!(
        boot_removed.get("workspaces").and_then(Value::as_array).is_some_and(Vec::is_empty),
        "removed workspace must disappear from bootstrap"
    );
    let preserved_log = env.home.join("sessions").join(format!("{s_abort}.jsonl"));
    assert!(preserved_log.exists(), "removing a workspace must preserve session logs on disk");

    client.rpc_ok("workspace.add", json!({ "root": ws })).await;
    let boot_readded = client.rpc_ok("app.bootstrap", json!({})).await;
    assert!(
        boot_readded.to_string().contains(&s_abort),
        "re-adding workspace must restore historical sessions in bootstrap"
    );

    server.assert_no_protocol_violations();
    client.shutdown().await;

    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&preserved_log).expect("open log");
        f.write_all(b"{\"type\":\"message\",\"truncated\":true").expect("append partial tail");
    }

    let mut repaired_client = BackendClient::spawn(&env.home).await;
    let repaired = repaired_client.rpc_ok("session.read", json!({ "sessionId": s_abort })).await;
    assert!(
        repaired.to_string().contains("CONTINUED_AFTER_TOOL_AND_QUESTION_ABORTS"),
        "trailing partial line without newline must auto-repair"
    );
    repaired_client.shutdown().await;

    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&preserved_log).expect("open log");
        f.write_all(b"{corrupt json line}\n").expect("append corrupt complete line");
    }
    let mut corrupt_client = BackendClient::spawn(&env.home).await;
    let err = corrupt_client.rpc_err("app.bootstrap", json!({})).await;
    assert!(!err.is_empty(), "corrupt complete session line must surface explicit error");
    corrupt_client.shutdown().await;
}

#[tokio::test]
async fn git_diff_and_rewind_invariants() {
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
    run_git(&env.workspace, &["init", "-q"]);
    run_git(&env.workspace, &["config", "user.email", "test@example.invalid"]);
    run_git(&env.workspace, &["config", "user.name", "Test"]);
    run_git(&env.workspace, &["add", "-A"]);
    run_git(&env.workspace, &["commit", "-q", "-m", "baseline"]);
    // Hostile user git config must not break batch diff parsing or path matching.
    run_git(&env.workspace, &["config", "diff.noprefix", "true"]);
    run_git(&env.workspace, &["config", "diff.mnemonicPrefix", "true"]);
    run_git(&env.workspace, &["config", "diff.relative", "true"]);

    server.enqueue(ScriptedResponse::sse(messages_text_sse(None, "NON_GIT_OK", 40, 0, 10, "end_turn")));
    server.enqueue(ScriptedResponse::sse(messages_text_sse(None, "TURN_1_BASELINE", 60, 0, 15, "end_turn")));
    server.enqueue(ScriptedResponse::sse(messages_tools_sse(
        None,
        vec![
            ("r_tracked", "read", json!({ "path": "tracked.txt" })),
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

    // Non-git workspace returns empty changedFiles cleanly.
    let s_plain = client.start_session(&plain_ws, "local", "demo", None, "plain turn", vec![], vec![]).await;
    client.wait_settled(&s_plain).await;
    let plain_changed = client.rpc_ok("session.changedFiles", json!({ "workspaceRoot": plain_ws })).await;
    assert_eq!(plain_changed.as_array().map(Vec::len), Some(0));

    let s_git = client.start_session(&ws, "local", "demo", None, "baseline turn", vec![], vec![]).await;
    client.wait_settled(&s_git).await;

    client.submit(&s_git, "mutate tracked files").await;
    client.wait_settled(&s_git).await;
    assert_eq!(std::fs::read_to_string(&tracked).expect("read mutated file"), "v2\n");
    assert!(extra.exists(), "extra.txt must exist after turn 2");

    // Rename after turn 2 to verify rewind preserves ThreadName and ThreadSettings.
    client.rpc_ok("session.rename", json!({ "sessionId": s_git, "name": "Keep Title" })).await;

    let before_rewind = client.rpc_ok("session.read", json!({ "sessionId": s_git })).await;
    let turns = before_rewind.pointer("/history/turns").and_then(Value::as_array).expect("turns");
    assert_eq!(turns.len(), 2);
    let turn2_items = turns[1].get("items").and_then(Value::as_array).expect("turn 2 items");
    let created_flags: Vec<(&str, Option<bool>)> = turn2_items
        .iter()
        .filter(|item| {
            item.get("type").and_then(Value::as_str) == Some("tool")
                && item.get("name").and_then(Value::as_str) == Some("write")
        })
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
    run_git(&env.workspace, &["mv", "mv_old.txt", "mv_new.txt"]);
    std::fs::write(&mv_new, "completely\ndifferent\nrewritten\nbody\nnow\n").expect("rewrite mv_new");
    std::fs::write(&bracket_file, "export const id = 2\n").expect("modify [id]");
    std::fs::write(&plain_i_file, "export const i = 2\n").expect("modify i");

    let changed = client.rpc_ok("session.changedFiles", json!({ "workspaceRoot": ws })).await;
    let changed_items = changed.as_array().expect("changed files");
    let changed_paths: Vec<&str> =
        changed_items.iter().filter_map(|f| f.get("path").and_then(Value::as_str)).collect();
    assert!(changed_paths.contains(&"tracked.txt"), "changedFiles: {changed_paths:?}");
    assert!(changed_paths.contains(&"extra.txt"), "changedFiles: {changed_paths:?}");
    assert!(changed_paths.contains(&"mv_old.txt"), "split mv must include deletion: {changed_paths:?}");
    assert!(changed_paths.contains(&"mv_new.txt"), "split mv must include addition: {changed_paths:?}");
    let mv_old_entry = changed_items
        .iter()
        .find(|f| f.get("path").and_then(Value::as_str) == Some("mv_old.txt"))
        .expect("mv_old.txt entry");
    assert_eq!(mv_old_entry.get("status").and_then(Value::as_str), Some("deleted"));
    let mv_old_diff = client
        .rpc_ok("session.fileDiff", json!({ "workspaceRoot": ws, "path": "mv_old.txt" }))
        .await;
    assert!(mv_old_diff.get("output").and_then(Value::as_str).unwrap_or_default().contains("-old1"));

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
    run_git(&unborn_dir, &["init", "-q"]);
    std::fs::write(unborn_dir.join("am.txt"), "staged\n").expect("write staged am.txt");
    run_git(&unborn_dir, &["add", "am.txt"]);
    std::fs::write(unborn_dir.join("am.txt"), "staged\nworktree\n").expect("edit am.txt");
    client.rpc_ok("workspace.add", json!({ "root": unborn_ws })).await;
    let unborn_changed = client.rpc_ok("session.changedFiles", json!({ "workspaceRoot": unborn_ws })).await;
    assert!(
        unborn_changed
            .as_array()
            .is_some_and(|arr| arr.iter().any(|f| f.get("path").and_then(Value::as_str) == Some("am.txt"))),
        "unborn AM file must appear in changedFiles: {unborn_changed}"
    );
    let unborn_diff = client
        .rpc_ok("session.fileDiff", json!({ "workspaceRoot": unborn_ws, "path": "am.txt" }))
        .await;
    let am_diff = unborn_diff.get("output").and_then(Value::as_str).unwrap_or_default();
    assert!(
        am_diff.contains("+staged") && am_diff.contains("+worktree"),
        "unborn AM file must reflect working tree content: {am_diff}"
    );

    let diff = client
        .rpc_ok("session.fileDiff", json!({ "workspaceRoot": ws, "path": "tracked.txt" }))
        .await;
    let diff_out = diff.get("output").and_then(Value::as_str).unwrap_or_default();
    assert!(diff_out.contains("-v1") && diff_out.contains("+v2"), "unexpected diff: {diff_out}");

    let untracked_diff = client
        .rpc_ok("session.fileDiff", json!({ "workspaceRoot": ws, "path": "extra.txt" }))
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

    // Atomic rewind with invalid message input (missing file attachment) must fail WITHOUT truncating history.
    let missing_file = app_core::display_path(&env.workspace.join("missing_file.txt"));
    let invalid_err = client
        .rpc_err(
            "session.rewind",
            json!({
                "sessionId": s_git,
                "messageId": turn2_user_msg_id,
                "text": "edited text",
                "files": [missing_file]
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

    // Atomic rewind truncates history from turn 2 user message, keeps workspace files and ThreadName intact, and starts the edited turn.
    server.enqueue(ScriptedResponse::sse(messages_text_sse(None, "TURN_2_EDITED_OK", 70, 0, 15, "end_turn")));
    let tracked_abs = app_core::display_path(&tracked);
    let atomic_resubmit = client
        .rpc_ok(
            "session.rewind",
            json!({
                "sessionId": s_git,
                "messageId": turn2_user_msg_id,
                "text": "edited prompt",
                "files": [tracked_abs]
            }),
        )
        .await;
    assert_eq!(
        atomic_resubmit.pointer("/runtime/phase").and_then(Value::as_str),
        Some("running"),
        "atomic rewind+resubmit must start the edited turn immediately"
    );
    assert_eq!(
        atomic_resubmit.pointer("/history/turns").and_then(Value::as_array).map(Vec::len),
        Some(1),
        "rewind must truncate turn 2 and failed turn from history"
    );
    client.wait_settled(&s_git).await;
    let after_edit = client.rpc_ok("session.read", json!({ "sessionId": s_git })).await;
    let edited_turns = after_edit.pointer("/history/turns").and_then(Value::as_array).expect("edited turns");
    assert_eq!(edited_turns.len(), 2, "edited session must contain turn 1 plus replacement turn 2");
    assert!(after_edit.to_string().contains("TURN_2_EDITED_OK"));
    assert!(!after_edit.to_string().contains("TURN_2_MUTATED"));
    assert_eq!(after_edit.pointer("/history/summary/title").and_then(Value::as_str), Some("Keep Title"));
    assert_eq!(
        std::fs::read_to_string(&tracked).expect("read tracked after rewind"),
        "v2\n",
        "rewind must leave workspace files untouched"
    );
    assert!(extra.exists(), "rewind must leave extra.txt untouched");

    server.assert_no_protocol_violations();
    client.shutdown().await;

    let mut restarted = BackendClient::spawn(&env.home).await;
    let after_restart = restarted.rpc_ok("session.read", json!({ "sessionId": s_git })).await;
    assert_eq!(
        after_edit.pointer("/history/turns"),
        after_restart.pointer("/history/turns"),
        "rewound session history must persist identically across restart"
    );
    assert_eq!(
        after_restart.pointer("/history/summary/title").and_then(Value::as_str),
        Some("Keep Title")
    );
    restarted.shutdown().await;
}

#[tokio::test]
async fn session_cache_lru_eviction_invariants() {
    let env = TestEnv::new("lru-eviction");
    let server = MockModelServer::start().await;
    let ws = env.workspace_str();

    let busy_gate = Arc::new(Notify::new());
    server.enqueue(ScriptedResponse::sse_gated(
        messages_text_sse(None, "BUSY_SESSION_ANSWER", 60, 0, 15, "end_turn"),
        Arc::clone(&busy_gate),
    ));
    server.set_fallback(|_, _| {
        ScriptedResponse::sse(messages_text_sse(None, "IDLE_SESSION_OK", 40, 0, 10, "end_turn"))
    });

    let mut client = BackendClient::spawn(&env.home).await;
    client
        .setup_workspace_and_provider(&ws, "local", "messages", &server.base_url, "demo", 64000, 4096)
        .await;

    // Start the busy session first so it is the oldest entry in SessionCache and must not be evicted while running.
    let busy_id = client
        .start_session(&ws, "local", "demo", None, "BUSY_SESSION_PROMPT", vec![], vec![])
        .await;
    server.wait_for_request_count(1).await;

    // Fill SessionCache to exact 16-slot capacity: 1 busy + 15 idle sessions.
    let mut idle_ids = Vec::with_capacity(16);
    let mut evicted_turns_before = Value::Null;
    let mut evicted_rev_before = 0u64;
    for idx in 0..15 {
        let prompt = format!("IDLE_SESSION_PROMPT_{idx}");
        let id = client.start_session(&ws, "local", "demo", None, &prompt, vec![], vec![]).await;
        let settled = client.wait_settled(&id).await;
        assert_eq!(settled.pointer("/summary/status").and_then(Value::as_str), Some("completed"));
        let _ = client.drain_events();
        if idx == 1 {
            let before = client.rpc_ok("session.read", json!({ "sessionId": id })).await;
            assert!(
                before.get("context").is_some_and(|v| !v.is_null()),
                "active cached session should retain loaded context before eviction"
            );
            evicted_turns_before = before.pointer("/history/turns").cloned().expect("idle[1] turns");
            evicted_rev_before = before.get("revision").and_then(Value::as_u64).expect("idle[1] revision");
        }
        idle_ids.push(id);
    }

    let full_boot = client.rpc_ok("app.bootstrap", json!({})).await;
    let full_states = full_boot.get("sessionStates").and_then(Value::as_object).expect("full sessionStates");
    assert_eq!(full_states.len(), 16, "cache must hold all 16 sessions before overflow");
    assert!(
        full_states.contains_key(&busy_id)
            && full_states.contains_key(&idle_ids[0])
            && full_states.contains_key(&idle_ids[1])
    );

    // Refresh idle[0] so it becomes MRU while idle[1] becomes the LRU idle candidate.
    let refreshed = client.rpc_ok("session.read", json!({ "sessionId": idle_ids[0] })).await;
    assert!(
        refreshed.get("context").is_some_and(|v| !v.is_null()),
        "idle[0] must still be cached at cap 16"
    );

    // Insert 17th total session (1 busy + 16 idle) to trigger exact 16-slot LRU eviction.
    let overflow_id = client
        .start_session(&ws, "local", "demo", None, "IDLE_SESSION_PROMPT_15", vec![], vec![])
        .await;
    let overflow_settled = client.wait_settled(&overflow_id).await;
    assert_eq!(overflow_settled.pointer("/summary/status").and_then(Value::as_str), Some("completed"));
    let _ = client.drain_events();
    idle_ids.push(overflow_id);

    let after_boot = client.rpc_ok("app.bootstrap", json!({})).await;
    let after_states = after_boot
        .get("sessionStates")
        .and_then(Value::as_object)
        .expect("post-eviction sessionStates");
    assert_eq!(after_states.len(), 16, "SessionCache must evict at exact capacity 16");
    assert!(after_states.contains_key(&busy_id), "busy session must remain cached");
    assert!(
        after_states.contains_key(&idle_ids[0]),
        "refreshed older session idle[0] must be retained (proves LRU, not FIFO)"
    );
    assert!(
        !after_states.contains_key(&idle_ids[1]),
        "un-refreshed LRU session idle[1] must be evicted at exact capacity 16"
    );
    let kept = client.rpc_ok("session.read", json!({ "sessionId": idle_ids[0] })).await;
    assert!(
        kept.get("context").is_some_and(|v| !v.is_null()),
        "retained idle[0] must stay warm in cache"
    );

    // Busy session must remain intact in cache and complete cleanly once unblocked.
    let busy_running = client.rpc_ok("session.read", json!({ "sessionId": busy_id })).await;
    assert_eq!(
        busy_running.pointer("/runtime/phase").and_then(Value::as_str),
        Some("running"),
        "busy session must not be evicted while actively running"
    );
    busy_gate.notify_one();
    let busy_settled = client.wait_settled(&busy_id).await;
    assert_eq!(busy_settled.pointer("/summary/status").and_then(Value::as_str), Some("completed"));
    let busy_done = client.rpc_ok("session.read", json!({ "sessionId": busy_id })).await;
    assert!(busy_done.to_string().contains("BUSY_SESSION_ANSWER"));

    // Evicted LRU idle session must reopen cleanly from disk with full history and monotonic revision.
    let evicted_id = &idle_ids[1];
    let reopened = client.rpc_ok("session.read", json!({ "sessionId": evicted_id })).await;
    assert!(
        reopened.get("context").is_some_and(Value::is_null),
        "evicted idle session must reopen cold from disk with lazily deferred context"
    );
    assert_eq!(
        reopened.pointer("/history/turns"),
        Some(&evicted_turns_before),
        "reopened evicted session must preserve full persisted history"
    );
    let rev_reopened = reopened.get("revision").and_then(Value::as_u64).expect("reopened revision");
    assert!(
        rev_reopened >= evicted_rev_before,
        "reopened session revision must be monotonic ({rev_reopened} < {evicted_rev_before})"
    );

    client.submit(evicted_id, "EVICTED_FOLLOWUP_PROMPT").await;
    let followup_settled = client.wait_settled(evicted_id).await;
    assert_eq!(followup_settled.pointer("/summary/status").and_then(Value::as_str), Some("completed"));
    let reopened_after = client.rpc_ok("session.read", json!({ "sessionId": evicted_id })).await;
    assert_eq!(
        reopened_after.pointer("/history/turns").and_then(Value::as_array).map(Vec::len),
        Some(2),
        "reopened session must support follow-up turns seamlessly"
    );
    let rev_after = reopened_after.get("revision").and_then(Value::as_u64).expect("after revision");
    assert!(
        rev_after > rev_reopened,
        "follow-up turn must advance session revision monotonically ({rev_after} <= {rev_reopened})"
    );

    let bootstrap = client.rpc_ok("app.bootstrap", json!({})).await;
    let listed_count = bootstrap
        .get("sessionsByWorkspace")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|map| map.values())
        .filter_map(Value::as_array)
        .flatten()
        .count();
    assert_eq!(listed_count, 1 + idle_ids.len(), "bootstrap must retain all 17 sessions");

    server.assert_no_protocol_violations();
    client.shutdown().await;
}
