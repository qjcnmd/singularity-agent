mod support;

use assert_cmd::Command;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Once;
use support::{
    FakeAppServer, Scenario, agent_loop_capability, capture_params, exit, print_stderr, respond,
    send, sleep_ms, thread as fake_thread, turn as fake_turn, write_text,
};

const APP_SERVER_BIN_ENV: &str = "SINGULARITY_APP_SERVER_BIN";
const APP_SERVER_DB_ENV: &str = "SINGULARITY_APP_SERVER_DB";
const EVAL_OUTPUT_DIR_ENV: &str = "SINGULARITY_EVAL_OUTPUT_DIR";
const DEFAULT_APP_SERVER_BIN: &str = "singularity_app_server";
const FAKE_APP_SERVER_EXIT_CODE: i32 = 7;
const JSON_RPC_SERVER_ERROR_CODE: i64 = -32000;
const NON_MATCHING_RESPONSE_ID: i64 = 999;
const POST_RESPONSE_DELAY_MS: u64 = 25;

#[test]
fn cli_exposes_app_server_protocol_mode_without_direct_core_runtime() {
    let mut command = Command::cargo_bin("sg").expect("binary");
    command.arg("--help").assert().success();
}

#[test]
fn cli_requires_an_end_user_command_and_hides_protocol_debug_commands() {
    let output = Command::cargo_bin("sg")
        .expect("binary")
        .output()
        .expect("run cli without command");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("Usage:"));

    let help = Command::cargo_bin("sg")
        .expect("binary")
        .arg("--help")
        .output()
        .expect("run cli help");
    assert!(help.status.success(), "stderr={}", stderr(&help));
    let help = stdout(&help);
    for internal in ["protocol-init", "thread-start", "daemon"] {
        assert!(!help.contains(internal), "help exposed {internal}: {help}");
    }
}

#[test]
fn cli_help_does_not_expose_agent_host_selector() {
    let output = Command::cargo_bin("sg")
        .expect("binary")
        .args(["run", "--help"])
        .output()
        .expect("run help");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(!stdout(&output).contains("--agent-host"));
}

#[test]
fn cli_run_continue_threads_trace_and_approvals_use_app_server_protocol() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let thread = fake_thread("thread_fake");
    let turn = fake_turn("turn_fake", "thread_fake", "completed", "completed");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .interaction(
                "thread/start",
                vec![
                    send(json!({"method": "thread/started", "params": {"thread": thread.clone()}})),
                    respond(json!({"thread": thread.clone()})),
                ],
            )
            .respond("thread/resume", json!({"thread": thread.clone()}))
            .respond("thread/list", json!({"threads": [thread]}))
            .interaction(
                "turn/start",
                vec![
                    send(json!({"method": "turn/started", "params": {"turn": turn.clone()}})),
                    respond(json!({"turn": turn})),
                ],
            )
            .respond(
                "trace/tail",
                json!({"events": [{"event_id": "trace_1", "component": "thread", "summary": "thread started"}]}),
            )
            .respond("approval/list", json!({"approvals": []}))
            .shutdown(),
    );

    let run = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests", "--model", "gpt-test"])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "stderr={}", stderr(&run));
    let run_stdout = stdout(&run);
    assert!(run_stdout.contains("thread/started"));
    assert!(run_stdout.contains("turn/started"));
    assert!(!run_stdout.contains("item/agentMessage/delta"));
    assert!(!run_stdout.contains("assistant input accepted"));
    let thread_id = run_stdout
        .lines()
        .find_map(|line| line.strip_prefix("thread "))
        .expect("thread id")
        .to_string();

    let threads = cli_with_fake_app_server(&fake_server, &db_path)
        .arg("threads")
        .output()
        .expect("threads cli");
    assert!(threads.status.success(), "stderr={}", stderr(&threads));
    assert!(stdout(&threads).contains(&thread_id));

    let continued = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["continue", &thread_id, "add docs"])
        .output()
        .expect("continue cli");
    assert!(continued.status.success(), "stderr={}", stderr(&continued));
    assert!(stdout(&continued).contains("turn/started"));

    let trace = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["trace", &thread_id, "--limit", "5"])
        .output()
        .expect("trace cli");
    assert!(trace.status.success(), "stderr={}", stderr(&trace));
    assert!(stdout(&trace).contains("thread started"));

    let approvals = cli_with_fake_app_server(&fake_server, &db_path)
        .arg("approvals")
        .output()
        .expect("approvals cli");
    assert!(approvals.status.success(), "stderr={}", stderr(&approvals));

    let doctor = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["config", "doctor"])
        .output()
        .expect("doctor cli");
    assert!(doctor.status.success(), "stderr={}", stderr(&doctor));
    assert!(stdout(&doctor).contains("client=protocol-only"));
}

#[test]
fn cli_config_doctor_reports_redacted_agent_loop_and_eval_readiness() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .respond(
                "agent/capability",
                json!({
                    "agentLoop": {
                        "available": true,
                        "status": "completed",
                        "reason": "enabled",
                        "blockers": [],
                    },
                    "providerConfiguration": {
                        "source": "project_env",
                        "snapshotId": "provider_snapshot_cli_test",
                        "configured": false,
                        "configurationBlocker": "required_env_missing",
                        "apiKeyPresent": false,
                        "baseUrlPresent": true,
                        "modelPresent": false,
                    },
                }),
            )
            .shutdown(),
    );

    std::fs::write(
        temp.path().join(".env"),
        concat!(
            "SINGULARITY_MODEL=project-model\n",
            "SINGULARITY_BASE_URL=https://project-provider.example/v1\n",
            "SINGULARITY_API_KEY=project-secret\n",
        ),
    )
    .expect("write synthetic project env");

    let doctor = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["config", "doctor"])
        .current_dir(temp.path())
        .env("SINGULARITY_API_KEY", "secret-value")
        .env("SINGULARITY_BASE_URL", "https://provider.example/v1")
        .env("SINGULARITY_MODEL", "gpt-test")
        .output()
        .expect("doctor cli");

    assert!(doctor.status.success(), "stderr={}", stderr(&doctor));
    let doctor_stdout = stdout(&doctor);
    assert!(doctor_stdout.contains("client=protocol-only"));
    assert!(doctor_stdout.contains("agent_loop=completed"));
    assert!(doctor_stdout.contains("evaluation=agent_loop"));
    assert!(doctor_stdout.contains("provider_config_source=project_env"));
    assert!(doctor_stdout.contains("provider_snapshot_id=provider_snapshot_cli_test"));
    assert!(doctor_stdout.contains("provider_configured=false"));
    assert!(doctor_stdout.contains("provider_configuration_blocker=required_env_missing"));
    assert!(doctor_stdout.contains("SINGULARITY_API_KEY=missing"));
    assert!(doctor_stdout.contains("SINGULARITY_BASE_URL=present(redacted)"));
    assert!(doctor_stdout.contains("SINGULARITY_MODEL=missing"));
    for secret in [
        "secret-value",
        "https://provider.example/v1",
        "gpt-test",
        "project-model",
        "project-provider.example",
        "project-secret",
    ] {
        assert!(!doctor_stdout.contains(secret));
    }
}

#[test]
fn cli_prefers_sibling_app_server_over_path_lookup() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_path_dir = temp.path().join("fake-path");
    std::fs::create_dir(&fake_path_dir).expect("fake path dir");
    let stale_server = FakeAppServer::new(
        temp.path(),
        Scenario::new().startup(vec![
            print_stderr("old app-server should not run"),
            exit(FAKE_APP_SERVER_EXIT_CODE),
        ]),
    );
    let fake_app_server = stale_server.copy_binary_as(&fake_path_dir, DEFAULT_APP_SERVER_BIN);
    ensure_app_server_binary();

    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![fake_path_dir];
    paths.extend(std::env::split_paths(&original_path));
    let path = std::env::join_paths(paths).expect("join path");

    let mut command = Command::cargo_bin("sg").expect("binary");
    stale_server.configure(&mut command);
    let output = command
        .args(["config", "doctor"])
        .env_remove(APP_SERVER_BIN_ENV)
        .env(APP_SERVER_DB_ENV, &db_path)
        .env("PATH", path)
        .output()
        .expect("doctor cli");

    assert!(
        output.status.success(),
        "fake_app_server={}
stderr={}",
        fake_app_server.display(),
        stderr(&output)
    );
    assert!(stdout(&output).contains(&format!("agent_loop={}", expected_agent_loop_status())));
    assert!(!stderr(&output).contains("old app-server should not run"));
}

#[test]
fn cli_fails_closed_without_explicit_or_sibling_app_server() {
    let temp = tempfile::tempdir().expect("temp dir");
    let cli_dir = temp.path().join("isolated-cli");
    let fake_path_dir = temp.path().join("fake-path");
    std::fs::create_dir(&cli_dir).expect("cli dir");
    std::fs::create_dir(&fake_path_dir).expect("fake path dir");
    let copied_cli = copy_current_cli_to(&cli_dir);
    let stale_server = FakeAppServer::new(
        temp.path(),
        Scenario::new().startup(vec![
            print_stderr("stale PATH app-server should not run"),
            exit(FAKE_APP_SERVER_EXIT_CODE),
        ]),
    );
    stale_server.copy_binary_as(&fake_path_dir, DEFAULT_APP_SERVER_BIN);
    let path = std::env::join_paths([fake_path_dir]).expect("join path");

    let mut command = std::process::Command::new(copied_cli);
    stale_server.configure_process(&mut command);
    let output = command
        .args(["config", "doctor"])
        .env_remove(APP_SERVER_BIN_ENV)
        .env("PATH", path)
        .output()
        .expect("doctor cli");

    let stderr = stderr(&output);
    assert!(!output.status.success());
    assert!(stderr.contains("not found beside sg"), "stderr={stderr}");
    assert!(!stderr.contains("stale PATH app-server should not run"));
}

#[test]
fn cli_rejects_run_when_agent_loop_capability_is_disabled() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let trace_path = temp.path().join("agent_loop_disabled_methods.txt");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .respond(
                "agent/capability",
                agent_loop_capability(
                    false,
                    "blocked",
                    "sandbox unavailable",
                    &["strict_command_sandbox_unavailable"],
                ),
            )
            .shutdown()
            .trace_methods_to(&trace_path),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");

    assert!(!output.status.success());
    assert!(stderr(&output).contains(
        "AgentLoop is not available: status=blocked; blockers=strict_command_sandbox_unavailable"
    ));
    let trace = std::fs::read_to_string(trace_path).expect("method trace");
    assert!(trace.contains("initialize"));
    assert!(trace.contains("agent/capability"));
    assert!(!trace.contains("turn/start"));
}

#[test]
fn cli_rejects_nonterminal_agent_loop_capability_without_blockers() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let trace_path = temp.path().join("agent_loop_running_methods.txt");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .respond(
                "agent/capability",
                agent_loop_capability(true, "running", "probe still running", &[]),
            )
            .shutdown()
            .trace_methods_to(&trace_path),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");

    assert!(!output.status.success());
    assert!(stderr(&output).contains("AgentLoop is not available: status=running; blockers=none"));
    let trace = std::fs::read_to_string(trace_path).expect("method trace");
    assert!(trace.contains("initialize"));
    assert!(trace.contains("agent/capability"));
    assert!(!trace.contains("turn/start"));
}

#[test]
fn cli_sends_turn_start_without_agent_host_after_capability_allows_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let trace_path = temp.path().join("agent_loop_enabled_turn.json");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .respond("thread/start", json!({"thread": fake_thread("thread_agent")}))
            .interaction(
                "turn/start",
                vec![
                    capture_params(&trace_path),
                    respond(json!({"turn": fake_turn("turn_agent", "thread_agent", "completed", "completed")})),
                ],
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    let params: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(trace_path).expect("turn trace"))
            .expect("turn trace json");
    assert!(params.get("agentHost").is_none());
    assert_eq!(params["threadId"], "thread_agent");
}

#[test]
fn cli_rejects_turn_without_agent_loop_terminal_status() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .respond(
                "thread/start",
                json!({"thread": fake_thread("thread_agent")}),
            )
            .respond(
                "turn/start",
                json!({"turn": fake_turn("turn_agent", "thread_agent", "running", "unknown")}),
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");

    assert!(!output.status.success());
    assert!(stderr(&output).contains("error running: turn running"));
}

#[test]
fn cli_run_json_outputs_turn_result_without_human_rendering() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let thread = fake_thread("thread_json");
    let turn = fake_turn("turn_json", "thread_json", "completed", "completed");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .respond("thread/start", json!({"thread": thread}))
            .interaction(
                "turn/start",
                vec![
                    send(json!({"method": "turn/started", "params": {"turn": turn.clone()}})),
                    send(json!({"method": "turn/diff/updated", "params": {"patch": "SECRET_DIFF_SHOULD_NOT_LEAK"}})),
                    send(json!({"method": "item/agentMessage/delta", "params": {"item": {"item_id": "item_json"}, "delta": "agent-loop-ok"}})),
                    respond(json!({"turn": turn})),
                ],
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "Reply exactly: agent-loop-ok", "--json"])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("run json");
    assert!(!stdout(&output).contains("SECRET_DIFF_SHOULD_NOT_LEAK"));
    assert_eq!(value["thread"]["thread_id"], "thread_json");
    assert_eq!(value["turn"]["turn_id"], "turn_json");
    assert_eq!(value["turn"]["agent_loop_status"], "completed");
    let events = value["events"].as_array().expect("events");
    assert!(events.iter().all(|event| event["method"].is_string()));
    let item_delta = events
        .iter()
        .find(|event| event["method"] == "item/agentMessage/delta")
        .expect("agent delta event");
    assert_eq!(item_delta["params"]["delta"], "agent-loop-ok");
    assert!(!stdout(&output).contains("turn turn_json completed"));
}

#[test]
fn cli_run_json_preserves_fail_closed_turn_status() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .respond(
                "thread/start",
                json!({"thread": fake_thread("thread_json")}),
            )
            .respond(
                "turn/start",
                json!({"turn": fake_turn("turn_json", "thread_json", "blocked", "blocked")}),
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests", "--json"])
        .output()
        .expect("run cli");

    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("run json");
    assert_eq!(value["turn"]["status"], "blocked");
    assert_eq!(value["turn"]["agent_loop_status"], "blocked");
}

#[test]
fn cli_rejects_partial_agent_loop_capability_until_blockers_clear() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let trace_path = temp.path().join("partial_agent_loop_methods.txt");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .respond(
                "agent/capability",
                agent_loop_capability(true, "running", "partial", &["strict_command_sandbox"]),
            )
            .shutdown()
            .trace_methods_to(&trace_path),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains(
            "AgentLoop is not available: status=running; blockers=strict_command_sandbox"
        )
    );
    let trace = std::fs::read_to_string(trace_path).expect("method trace");
    assert!(trace.contains("agent/capability"));
    assert!(!trace.contains("turn/start"));
}

#[test]
fn cli_eval_command_is_script_friendly_and_validates_manifest() {
    let temp = tempfile::tempdir().expect("temp dir");
    let missing_manifest = temp.path().join("missing.json");
    let manifest = temp.path().join("eval.json");
    std::fs::write(&manifest, "not json").expect("write manifest");

    let output = Command::cargo_bin("sg")
        .expect("binary")
        .args([
            "eval",
            "run",
            path_str(&missing_manifest),
            "--run-id",
            "eval_contract",
            "--json",
        ])
        .output()
        .expect("eval cli");

    assert!(!output.status.success());
    assert!(stderr(&output).contains("eval manifest not found"));

    let db_path = temp.path().join("sessions.sqlite3");
    let invalid = cli_with_app_server(&app_server_bin(), &db_path)
        .args([
            "eval",
            "run",
            path_str(&manifest),
            "--run-id",
            "eval_invalid",
            "--json",
        ])
        .output()
        .expect("eval cli invalid");

    assert!(!invalid.status.success());
    assert!(stderr(&invalid).contains("invalid eval manifest"));
}

#[test]
fn cli_eval_run_uses_app_server_and_reports_verification_result() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let eval_output = temp.path().join("eval-output");
    let manifest = temp.path().join("eval.json");
    let agent_loop_trace = temp.path().join("agent-loop-turn.json");
    std::fs::write(
        &manifest,
        r#"{
  "schema_version": "evaluation.task_set/v3",
  "tasks": [{
    "task_id": "fixture_agent",
    "description": "Exercise the AgentLoop evaluation transport.",
    "workspace": {"source": {"type": "local", "path": "."}},
    "agent": {
      "instructions": "Change solution.txt so value is 1.",
      "allowed_paths": ["solution.txt"],
      "allowed_tools": ["builtin.read", "builtin.edit"]
    },
    "evaluator": {
      "baseline": {"commands": [{"argv": ["rustc", "--version"]}]},
      "public": {"commands": [{"argv": ["rustc", "--version"]}]},
      "hidden": {"commands": [{"argv": ["rustc", "--version", "--verbose"]}]}
    }
  }]
}"#,
    )
    .expect("write manifest");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .interaction(
                "eval/run",
                vec![
                    capture_params(&agent_loop_trace),
                    respond(json!({
                        "run_id": "eval_agent",
                        "manifest": path_str(&manifest),
                        "runner": "agent_loop",
                        "status": "completed",
                        "blocker": null,
                        "evaluation_passed": true,
                        "tasks": [{
                            "task_id": "fixture_agent",
                            "status": "completed",
                            "blocker": null,
                            "stages": {
                                "baseline": {"status": "passed", "blocker": null},
                                "agent": {"status": "passed", "blocker": null},
                                "public": {"status": "passed", "blocker": null},
                                "hidden": {"status": "passed", "blocker": null}
                            },
                            "agent_completed": true,
                            "tests_passed": true,
                            "evaluation_passed": true,
                            "diagnostics": {
                                "smoke_command_satisfied": true,
                                "local_process_fallback_count": 0
                            }
                        }],
                        "result_path": path_str(&eval_output.join("eval_agent/result.json")),
                        "report_path": path_str(&eval_output.join("eval_agent/report.json"))
                    })),
                ],
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args([
            "eval",
            "run",
            path_str(&manifest),
            "--run-id",
            "eval_agent",
            "--json",
        ])
        .env(EVAL_OUTPUT_DIR_ENV, &eval_output)
        .output()
        .expect("eval cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("eval json");
    assert_eq!(value["runner"], "agent_loop");
    assert_eq!(value["evaluation_passed"], true);
    assert_eq!(value["tasks"][0]["agent_completed"], true);
    assert_eq!(value["tasks"][0]["tests_passed"], true);
    assert_eq!(
        value["tasks"][0]["diagnostics"]["local_process_fallback_count"],
        0
    );
    assert!(value["result_path"].as_str().is_some());
    assert!(value["report_path"].as_str().is_some());
    let params: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(agent_loop_trace).expect("agent loop trace"))
            .expect("agent loop turn params");
    assert_eq!(params["runId"], "eval_agent");
    assert_eq!(params["manifest"], path_str(&manifest));
    assert_eq!(params["outputRoot"], path_str(&eval_output));
}

#[test]
fn cli_renders_agent_loop_status_and_answer() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let thread = fake_thread("thread_fake");
    let turn = fake_turn("turn_fake", "thread_fake", "completed", "completed");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .interaction(
                "thread/start",
                vec![
                    send(json!({"method": "thread/started", "params": {"thread": thread.clone()}})),
                    respond(json!({"thread": thread})),
                ],
            )
            .interaction(
                "turn/start",
                vec![
                    send(json!({"method": "turn/started", "params": {"turn": turn.clone()}})),
                    send(json!({"method": "item/agentMessage/delta", "params": {"item": {"item_id": "item_fake"}, "delta": "agent loop completed"}})),
                    respond(json!({"turn": turn})),
                ],
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    let stdout = stdout(&output);
    assert!(stdout.contains("thread thread_fake"));
    assert!(stdout.contains("turn turn_fake completed agent_loop_status=completed"));
    assert!(stdout.contains("assistant agent loop completed"));
}

#[test]
fn cli_exits_nonzero_for_failed_turn_without_raw_payload() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .respond("thread/start", json!({"thread": fake_thread("thread_fake")}))
            .interaction(
                "turn/start",
                vec![
                    send(json!({"method": "item/agentMessage/delta", "params": {"item": {"item_id": "item_fake"}, "delta": "agent loop failed"}})),
                    respond(json!({"turn": fake_turn("turn_failed", "thread_fake", "failed", "failed")})),
                ],
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");

    assert!(!output.status.success());
    assert!(stdout(&output).contains("turn turn_failed failed agent_loop_status=failed"));
    assert!(!stdout(&output).to_lowercase().contains("raw_prompt"));
}

#[test]
fn cli_exits_nonzero_for_immediate_blocked_turn() {
    assert_immediate_terminal_turn_exits_nonzero("blocked", "blocked");
}

#[test]
fn cli_exits_nonzero_for_immediate_interrupted_turn() {
    assert_immediate_terminal_turn_exits_nonzero("interrupted", "cancelled");
}

#[test]
fn cli_turn_status_interrupt_approval_decision_and_trace_show_use_protocol() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .respond(
                "turn/status",
                json!({"turn": fake_turn("turn_fake", "thread_fake", "running", "running")}),
            )
            .respond(
                "turn/interrupt",
                json!({"turnId": "turn_fake", "status": "interrupted"}),
            )
            .respond(
                "approval/decision",
                json!({
                    "decision": {
                        "approvalId": "approval_fake",
                        "decision": "allow",
                        "reason": "operator approved"
                    }
                }),
            )
            .respond(
                "trace/show",
                json!({
                    "event": {
                        "event_id": "event_fake",
                        "event_type": "trace.event",
                        "run_id": "run_fake",
                        "session_id": "session_fake",
                        "task_id": null,
                        "phase_id": null,
                        "action_id": null,
                        "parent_event_id": null,
                        "timestamp": null,
                        "monotonic_ms": null,
                        "component": "agent_loop",
                        "severity": "info",
                        "summary": "agent trace",
                        "payload": {},
                        "artifact_refs": [],
                        "policy_decision_id": null,
                        "approval_grant_id": null,
                        "sandbox_id": null,
                        "command_id": null,
                        "transaction_id": null,
                        "verification_id": null,
                        "span_id": null,
                        "redaction_applied": true,
                        "payload_hash": ""
                    }
                }),
            ),
    );

    let status = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["turn", "status", "turn_fake"])
        .output()
        .expect("turn status cli");
    assert!(status.status.success(), "stderr={}", stderr(&status));
    assert!(stdout(&status).contains("turn turn_fake running agent_loop_status=running"));

    let interrupt = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["turn", "interrupt", "turn_fake"])
        .output()
        .expect("turn interrupt cli");
    assert!(interrupt.status.success(), "stderr={}", stderr(&interrupt));
    assert!(stdout(&interrupt).contains("turn turn_fake interrupted"));

    let approve = cli_with_fake_app_server(&fake_server, &db_path)
        .args([
            "approve",
            "approval_fake",
            "--decision",
            "allow",
            "--reason",
            "operator approved",
        ])
        .output()
        .expect("approve cli");
    assert!(approve.status.success(), "stderr={}", stderr(&approve));
    assert!(stdout(&approve).contains("approval approval_fake allow"));

    let trace_show = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["trace", "show", "event_fake"])
        .output()
        .expect("trace show cli");
    assert!(
        trace_show.status.success(),
        "stderr={}",
        stderr(&trace_show)
    );
    assert!(stdout(&trace_show).contains("trace event_fake agent_loop agent trace"));
}

#[test]
fn cli_turn_lifecycle_status_and_interrupt_render_agent_loop_status() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .respond(
                "turn/status",
                json!({"turn": fake_turn("turn_fake", "thread_fake", "running", "running")}),
            )
            .respond(
                "turn/interrupt",
                json!({
                    "turnId": "turn_fake",
                    "status": "interrupted",
                    "agent_loop_status": "cancel_requested"
                }),
            ),
    );

    let status = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["turn", "status", "turn_fake"])
        .output()
        .expect("turn status cli");
    assert!(status.status.success(), "stderr={}", stderr(&status));
    assert!(stdout(&status).contains("turn turn_fake running agent_loop_status=running"));

    let interrupt = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["turn", "interrupt", "turn_fake"])
        .output()
        .expect("turn interrupt cli");
    assert!(interrupt.status.success(), "stderr={}", stderr(&interrupt));
    assert!(
        stdout(&interrupt)
            .contains("turn turn_fake interrupted agent_loop_status=cancel_requested")
    );
}

#[test]
fn cli_turn_interrupt_error_exits_nonzero() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new().initialized().error(
            "turn/interrupt",
            JSON_RPC_SERVER_ERROR_CODE,
            "cancel failed",
        ),
    );

    let interrupt = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["turn", "interrupt", "turn_fake"])
        .output()
        .expect("turn interrupt cli");

    assert!(!interrupt.status.success());
    assert!(stderr(&interrupt).contains("cancel failed"));
}

#[test]
fn cli_requests_server_shutdown_before_process_teardown() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let shutdown_log = temp.path().join("shutdown.log");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .respond("thread/list", json!({"threads": []}))
            .interaction(
                "server/shutdown",
                vec![
                    write_text(&shutdown_log, "shutdown"),
                    respond(json!({"shutdown": true})),
                    exit(0),
                ],
            ),
    );

    let threads = cli_with_fake_app_server(&fake_server, &db_path)
        .arg("threads")
        .output()
        .expect("threads cli");

    assert!(threads.status.success(), "stderr={}", stderr(&threads));
    assert_eq!(
        std::fs::read_to_string(shutdown_log).expect("shutdown log"),
        "shutdown"
    );
}

#[test]
fn cli_continue_resumes_thread_and_does_not_upload_history() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let method_trace = temp.path().join("methods.log");
    let turn_params = temp.path().join("turn-params.json");
    let thread = fake_thread("thread_resume");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .respond("thread/resume", json!({"thread": thread}))
            .interaction(
                "turn/start",
                vec![
                    capture_params(&turn_params),
                    respond(json!({
                        "turn": fake_turn(
                            "turn_resume",
                            "thread_resume",
                            "completed",
                            "completed"
                        )
                    })),
                ],
            )
            .trace_methods_to(&method_trace)
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["continue", "thread_resume", "continue safely"])
        .output()
        .expect("continue cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    let methods = std::fs::read_to_string(method_trace).expect("method trace");
    assert!(methods.contains("thread/resume"));
    assert!(!methods.contains("thread/read"));
    let params: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(turn_params).expect("turn params"))
            .expect("turn params json");
    assert_eq!(params["threadId"], "thread_resume");
    assert_eq!(params["input"][0]["text"], "continue safely");
    assert!(params.get("history").is_none());
}
#[test]
fn cli_continue_rejects_invalid_thread_id_through_app_server() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let app_server_bin = app_server_bin();

    let output = cli_with_app_server(&app_server_bin, &db_path)
        .args(["continue", "thread_missing", "add docs"])
        .output()
        .expect("continue cli");

    assert!(!output.status.success());
    assert!(stderr(&output).contains("Thread not found"));
}

#[test]
fn cli_reports_interrupted_app_server_process() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let cli_bin = assert_cmd::cargo::cargo_bin("sg");

    let output = cli_with_app_server(cli_bin.to_string_lossy().as_ref(), &db_path)
        .arg("threads")
        .output()
        .expect("threads cli");

    assert!(!output.status.success());
    assert_app_server_unavailable_error(&output);
}

#[test]
fn cli_run_returns_when_turn_response_has_no_notifications() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .respond(
                "thread/start",
                json!({"thread": fake_thread("thread_fake")}),
            )
            .respond(
                "turn/start",
                json!({"turn": fake_turn("turn_fake", "thread_fake", "completed", "completed")}),
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(stdout(&output).contains("thread thread_fake"));
}

#[test]
fn cli_run_does_not_wait_for_post_response_messages() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .respond("thread/start", json!({"thread": fake_thread("thread_fake")}))
            .interaction(
                "turn/start",
                vec![
                    respond(json!({"turn": fake_turn("turn_fake", "thread_fake", "completed", "completed")})),
                    sleep_ms(POST_RESPONSE_DELAY_MS),
                    send(json!({"id": NON_MATCHING_RESPONSE_ID, "result": {"late": true}})),
                ],
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(stdout(&output).contains("thread thread_fake"));
}

#[test]
fn cli_run_polls_running_turn_before_shutdown() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .respond(
                "thread/start",
                json!({"thread": fake_thread("thread_fake")}),
            )
            .respond(
                "turn/start",
                json!({"turn": fake_turn("turn_active", "thread_fake", "running", "running")}),
            )
            .respond(
                "turn/status",
                json!({"turn": fake_turn("turn_active", "thread_fake", "running", "running")}),
            )
            .respond(
                "turn/status",
                json!({"turn": fake_turn("turn_active", "thread_fake", "completed", "completed")}),
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(stdout(&output).contains("turn turn_active completed agent_loop_status=completed"));
}

#[test]
fn cli_run_polling_exits_nonzero_for_interrupted_turn() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .respond("thread/start", json!({"thread": fake_thread("thread_fake")}))
            .respond(
                "turn/start",
                json!({"turn": fake_turn("turn_active", "thread_fake", "running", "running")}),
            )
            .respond(
                "turn/status",
                json!({"turn": fake_turn("turn_active", "thread_fake", "interrupted", "cancelled")}),
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");

    assert!(!output.status.success());
    assert!(stdout(&output).contains("turn turn_active interrupted agent_loop_status=cancelled"));
    assert!(stderr(&output).contains("turn turn_active interrupted"));
}

#[test]
fn cli_reports_json_rpc_error_without_swallowing_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new().error("initialize", JSON_RPC_SERVER_ERROR_CODE, "forced failure"),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .arg("threads")
        .output()
        .expect("threads cli");

    assert!(!output.status.success());
    assert!(stderr(&output).contains("forced failure"));
}

#[test]
fn cli_ignores_non_matching_response_before_next_matching_response() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .respond(
                "thread/start",
                json!({"thread": fake_thread("thread_fake")}),
            )
            .interaction(
                "turn/start",
                vec![
                    send(json!({
                        "id": NON_MATCHING_RESPONSE_ID,
                        "result": {
                            "turn": fake_turn("wrong_turn", "thread_fake", "running", "unknown")
                        }
                    })),
                    respond(json!({
                        "turn": fake_turn("turn_fake", "thread_fake", "completed", "completed")
                    })),
                ],
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(!stdout(&output).contains("wrong_turn"));
}

#[test]
fn cli_ignores_non_matching_error_before_next_matching_response() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .respond(
                "thread/start",
                json!({"thread": fake_thread("thread_fake")}),
            )
            .interaction(
                "turn/start",
                vec![
                    send(json!({
                        "id": NON_MATCHING_RESPONSE_ID,
                        "error": {
                            "code": JSON_RPC_SERVER_ERROR_CODE,
                            "message": "stale failure"
                        }
                    })),
                    respond(json!({
                        "turn": fake_turn("turn_fake", "thread_fake", "completed", "completed")
                    })),
                ],
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(!stderr(&output).contains("stale failure"));
    assert!(stdout(&output).contains("thread thread_fake"));
}

#[test]
fn cli_reports_app_server_exit_before_response() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new().startup(vec![exit(FAKE_APP_SERVER_EXIT_CODE)]),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .arg("threads")
        .output()
        .expect("threads cli");

    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert!(is_app_server_unavailable_error(&stderr), "stderr={stderr}");
}

#[test]
fn cli_manifest_does_not_depend_on_core_runtime_crates() {
    let manifest_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = std::fs::read_to_string(manifest_path).expect("read cli manifest");

    for forbidden in [
        "singularity_agent",
        "singularity_tools",
        "singularity_store",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "cli must not depend directly on {forbidden}"
        );
    }
    assert!(manifest.contains("singularity_protocol"));
}

fn cli_with_app_server(app_server_bin: &str, db_path: &std::path::Path) -> Command {
    let mut command = Command::cargo_bin("sg").expect("binary");
    command.env(APP_SERVER_BIN_ENV, app_server_bin);
    command.env(APP_SERVER_DB_ENV, db_path);
    command
}

fn cli_with_fake_app_server(fake_server: &FakeAppServer, db_path: &Path) -> Command {
    let mut command = cli_with_app_server(path_str(fake_server.binary()), db_path);
    fake_server.configure(&mut command);
    command
}

fn assert_immediate_terminal_turn_exits_nonzero(status: &str, agent_loop_status: &str) {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .respond("thread/start", json!({"thread": fake_thread("thread_fake")}))
            .respond(
                "turn/start",
                json!({"turn": fake_turn("turn_terminal", "thread_fake", status, agent_loop_status)}),
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");

    assert!(!output.status.success());
    assert!(stdout(&output).contains(&format!(
        "turn turn_terminal {status} agent_loop_status={agent_loop_status}"
    )));
    assert!(stderr(&output).contains(&format!("turn {status}")));
}

fn expected_agent_loop_status() -> &'static str {
    if cfg!(windows) {
        "completed"
    } else {
        "blocked"
    }
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

fn copy_current_cli_to(dir: &Path) -> PathBuf {
    let source = assert_cmd::cargo::cargo_bin("sg");
    let target = dir.join(format!("sg{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(&source, &target).expect("copy sg binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(&target)
            .expect("copied cli metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&target, permissions).expect("copied cli executable");
    }
    target
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn assert_app_server_unavailable_error(output: &std::process::Output) {
    let stderr = stderr(output);
    assert!(is_app_server_unavailable_error(&stderr), "stderr={stderr}");
}

fn is_app_server_unavailable_error(stderr: &str) -> bool {
    stderr.contains("app-server exited before response")
        || stderr.contains("app-server closed stdout")
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn app_server_bin() -> String {
    std::env::var("CARGO_BIN_EXE_singularity_app_server").unwrap_or_else(|_| {
        ensure_app_server_binary();
        let target_dir = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace_root().join("target"));
        target_dir
            .join("debug")
            .join(format!(
                "singularity_app_server{}",
                std::env::consts::EXE_SUFFIX
            ))
            .to_string_lossy()
            .into_owned()
    })
}

fn ensure_app_server_binary() {
    static BUILD_APP_SERVER: Once = Once::new();
    BUILD_APP_SERVER.call_once(|| {
        let status = std::process::Command::new("cargo")
            .args([
                "build",
                "-p",
                "singularity_app_server",
                "--bin",
                "singularity_app_server",
                "--locked",
            ])
            .current_dir(workspace_root())
            .status()
            .expect("build app-server binary");
        assert!(status.success(), "failed to build app-server binary");
    });
}
