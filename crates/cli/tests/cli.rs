//! 验证 CLI 仅通过 app-server 协议工作，并保持输出与失败边界稳定。

#[allow(dead_code)]
mod support;

use assert_cmd::Command;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Once;
use support::{
    FakeAppServer as RawFakeAppServer, Scenario as RawScenario, agent_loop_capability,
    capture_params, exit, print_stderr, sleep_ms, thread as raw_fake_thread, turn as fake_turn,
    write_text,
};

const APP_SERVER_BIN_ENV: &str = "SINGULARITY_APP_SERVER_BIN";
const APP_SERVER_DB_ENV: &str = "SINGULARITY_APP_SERVER_DB";
/// CLI 测试用 fake app-server 只讲 stdio JSON-RPC；CLI 默认走 TCP，故测试显式回退到 stdio。
const APP_SERVER_TRANSPORT_ENV: &str = "SINGULARITY_APP_SERVER_TRANSPORT";
const DEFAULT_APP_SERVER_BIN: &str = "singularity_app_server";
const FAKE_APP_SERVER_EXIT_CODE: i32 = 7;
const JSON_RPC_SERVER_ERROR_CODE: i64 = -32000;
const NON_MATCHING_RESPONSE_ID: i64 = 999;
const POST_RESPONSE_DELAY_MS: u64 = 25;

/// 为既有 fake app-server 补齐严格 JSON-RPC 2.0 envelope。
struct FakeAppServer(RawFakeAppServer);

impl FakeAppServer {
    fn new(dir: &Path, scenario: Scenario) -> Self {
        Self(RawFakeAppServer::new(dir, scenario.0))
    }

    fn binary(&self) -> &Path {
        self.0.binary()
    }

    fn configure(&self, command: &mut Command) {
        self.0.configure(command);
    }

    fn configure_process(&self, command: &mut std::process::Command) {
        self.0.configure_process(command);
    }

    fn copy_binary_as(&self, dir: &Path, name: &str) -> PathBuf {
        self.0.copy_binary_as(dir, name)
    }
}

struct Scenario(RawScenario);

impl Scenario {
    fn new() -> Self {
        Self(RawScenario::new())
    }

    fn startup(self, actions: Vec<serde_json::Value>) -> Self {
        Self(self.0.startup(actions))
    }

    fn interaction(self, method: &str, actions: Vec<serde_json::Value>) -> Self {
        Self(self.0.interaction(method, actions))
    }

    fn respond(self, method: &str, result: serde_json::Value) -> Self {
        self.interaction(method, vec![respond(result)])
    }

    fn initialized(self) -> Self {
        self.respond(
            "initialize",
            json!({"userAgent":"fake","platformFamily":"local","platformOs":"test"}),
        )
    }

    fn agent_loop_ready(self) -> Self {
        self.respond(
            "agent/capability",
            agent_loop_capability(true, "completed", "enabled", &[]),
        )
    }

    fn shutdown(self) -> Self {
        self.interaction(
            "server/shutdown",
            vec![respond(json!({"shutdown": true})), exit(0)],
        )
    }

    fn error(self, method: &str, code: i64, message: &str) -> Self {
        self.interaction(
            method,
            vec![json!({"respond":{"jsonrpc":"2.0","error":{"code":code,"message":message}}})],
        )
    }

    fn trace_methods_to(self, path: &Path) -> Self {
        Self(self.0.trace_methods_to(path))
    }
}

fn respond(result: serde_json::Value) -> serde_json::Value {
    json!({"respond":{"jsonrpc":"2.0","result":result}})
}

fn send(mut message: serde_json::Value) -> serde_json::Value {
    message
        .as_object_mut()
        .expect("fake JSON-RPC message object")
        .insert("jsonrpc".to_string(), json!("2.0"));
    json!({"send":message})
}

fn fake_thread(thread_id: &str) -> serde_json::Value {
    raw_fake_thread(thread_id)
}

// 验证 metrics CLI 只请求服务端派生结果，并原样渲染服务端聚合值。
#[test]
fn cli_exposes_app_server_protocol_mode_without_direct_core_runtime() {
    let mut command = Command::cargo_bin("sg").expect("binary");
    command.arg("--help").assert().success();
}

// 确认必须提供终端用户命令且不暴露内部调试命令。
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
    for internal in [
        "protocol-init",
        "thread-start",
        "daemon",
        "turn status",
        "turn pause",
        "turn resume",
        "turn input",
    ] {
        assert!(!help.contains(internal), "help exposed {internal}: {help}");
    }
}

// 确认 run 帮助不提供已移除的 agent host 选择器。
#[test]
fn cli_help_does_not_expose_agent_host_selector() {
    let output = Command::cargo_bin("sg")
        .expect("binary")
        .args(["run", "--help"])
        .output()
        .expect("run help");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(!stdout(&output).contains("--agent-host"));
    assert!(!stdout(&output).contains("danger-full-access"));
    assert!(!stdout(&output).contains("untrusted"));
}

// 验证 run 只发送 thread/model/cwd 参数并渲染 thread 身份，不再携带 policy 快照。
#[test]
fn cli_run_renders_thread_identity_without_policy_params() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let params_path = temp.path().join("thread_start_params.json");
    let thread = fake_thread("thread_policy");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .interaction(
                "thread/start",
                vec![
                    capture_params(&params_path),
                    respond(json!({"thread": thread})),
                ],
            )
            .respond(
                "turn/start",
                json!({"turn": fake_turn("turn_policy", "thread_policy", "completed", "completed")}),
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(stdout(&output).contains("thread thread_policy"));

    let params: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(params_path).expect("thread params"))
            .expect("thread params json");
    assert!(params.get("sandboxMode").is_none());
    assert!(params.get("approvalPolicy").is_none());
}

// 验证 run、continue、threads 与 doctor 共用 app-server 协议。
#[test]
fn cli_run_continue_threads_and_doctor_use_app_server_protocol() {
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

    let doctor = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["config", "doctor"])
        .output()
        .expect("doctor cli");
    assert!(doctor.status.success(), "stderr={}", stderr(&doctor));
    assert!(stdout(&doctor).contains("client=protocol-only"));
}

// 验证 threads 默认展示全部会话并携带 cwd。
#[test]
fn cli_threads_renders_all_sessions_with_cwd() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .respond(
                "thread/list",
                json!({"threads": [
                    raw_fake_thread("session-a"),
                    raw_fake_thread("session-b")
                ]}),
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .arg("threads")
        .output()
        .expect("threads cli");
    assert!(output.status.success(), "stderr={}", stderr(&output));
    let stdout = stdout(&output);
    assert!(stdout.contains("session-a active"));
    assert!(stdout.contains("session-b active"));
}

// 验证 session read 只渲染摘要与最近片段，不渲染全文。
#[test]
fn cli_session_read_renders_summary_and_recent_entries() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .respond(
                "session/read",
                json!({
                    "sessionId": "session-read",
                    "cwd": "/tmp/work",
                    "title": "fix tests",
                    "model": null,
                    "status": "active",
                    "createdAt": "2026-08-15T00:00:00Z",
                    "updatedAt": "2026-08-15T00:01:00Z",
                    "tokenUsage": {},
                    "summary": "compact summary",
                    "recentEntries": [
                        {"id":"entry-1","parentId":null,"type":"message"}
                    ],
                    "totalEntries": 2
                }),
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["session", "read", "session-read"])
        .output()
        .expect("session read cli");
    assert!(output.status.success(), "stderr={}", stderr(&output));
    let stdout = stdout(&output);
    assert!(stdout.contains("session session-read"));
    assert!(stdout.contains("summary compact summary"));
    assert!(stdout.contains("total_entries 2"));
    assert!(stdout.contains("entry-1"));
    assert!(!stdout.contains("full rollout"));
}

// 验证 session delete 打印服务端删除确认。
#[test]
fn cli_session_delete_renders_deleted_confirmation() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .respond(
                "session/delete",
                json!({"sessionId":"session-delete","deleted":true}),
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["session", "delete", "session-delete"])
        .output()
        .expect("session delete cli");
    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(stdout(&output).contains("session session-delete deleted=true"));
}

// 验证 run 识别 "查看会话 <ID>"，自动注入 session/read 摘要+最近片段。
#[test]
fn cli_run_view_session_injects_summary_and_recent_entries() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let turn_params_path = temp.path().join("turn_params.json");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .respond(
                "session/read",
                json!({
                    "sessionId": "session-context",
                    "cwd": "/tmp/work",
                    "title": null,
                    "model": null,
                    "status": "active",
                    "createdAt": "2026-08-15T00:00:00Z",
                    "updatedAt": "2026-08-15T00:01:00Z",
                    "tokenUsage": {},
                    "summary": "之前修好了计费 bug",
                    "recentEntries": [{"id":"e1","type":"message"}],
                    "totalEntries": 2
                }),
            )
            .agent_loop_ready()
            .respond("thread/start", json!({"thread": fake_thread("thread-context")}))
            .interaction(
                "turn/start",
                vec![
                    capture_params(&turn_params_path),
                    respond(json!({"turn": fake_turn("turn_context", "thread-context", "completed", "completed")})),
                ],
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "查看会话 session-context 分析下一步"])
        .output()
        .expect("run view session cli");
    assert!(output.status.success(), "stderr={}", stderr(&output));
    let params: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(turn_params_path).expect("turn params"))
            .expect("turn params json");
    let text = params["input"][0]["text"].as_str().expect("goal text");
    assert!(text.contains("[会话摘要 session-context]"), "{text}");
    assert!(text.contains("之前修好了计费 bug"), "{text}");
    assert!(text.contains("[最近片段]"), "{text}");
    assert!(
        text.contains("查看会话 session-context 分析下一步"),
        "{text}"
    );
}

// 验证 doctor 输出脱敏的 AgentLoop 与 provider readiness。
#[test]
fn cli_config_doctor_reports_redacted_agent_loop_and_provider_readiness() {
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
                        "source": "process_env",
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

    let doctor = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["config", "doctor"])
        .output()
        .expect("doctor cli");

    assert!(doctor.status.success(), "stderr={}", stderr(&doctor));
    let doctor_stdout = stdout(&doctor);
    assert!(doctor_stdout.contains("client=protocol-only"));
    assert!(doctor_stdout.contains("agent_loop=completed"));
    assert!(!doctor_stdout.contains("evaluation="));
    assert!(doctor_stdout.contains("provider_config_source=process_env"));
    assert!(doctor_stdout.contains("provider_snapshot_id=provider_snapshot_cli_test"));
    assert!(doctor_stdout.contains("provider_configured=false"));
    assert!(doctor_stdout.contains("provider_configuration_blocker=required_env_missing"));
    assert!(doctor_stdout.contains("SINGULARITY_API_KEY=missing"));
    assert!(doctor_stdout.contains("SINGULARITY_BASE_URL=present(redacted)"));
    assert!(doctor_stdout.contains("SINGULARITY_MODEL=missing"));
    for secret in ["secret-value", "https://provider.example/v1", "gpt-test"] {
        assert!(!doctor_stdout.contains(secret));
    }
}

// 验证存在相邻 app-server 时优先使用它而非 PATH 中的同名程序。
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

// 验证未配置显式或相邻 app-server 时保持 fail closed。
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

// 验证 AgentLoop 被禁用时 run 不会发出 turn/start。
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

// 验证 capability 尚未到 completed 即使无 blocker 也不能启动 run。
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

// 验证 capability 允许后 turn/start 不再携带 agent host。
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

// 验证 JSON 模式只输出脱敏结果与允许公开的事件摘要。
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

// 验证 JSON 模式保留 failed 状态并以失败退出。
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
                json!({"turn": fake_turn("turn_json", "thread_json", "failed", "failed")}),
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests", "--json"])
        .output()
        .expect("run cli");

    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("run json");
    assert_eq!(value["turn"]["status"], "failed");
    assert_eq!(value["turn"]["agent_loop_status"], "failed");
}

// 验证部分 capability 在 blocker 清除前仍不能启动 turn。
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

// 产品 CLI 不暴露开发期 Evaluation runner。
#[test]
fn cli_exposes_eval_subcommand_without_legacy_eval_args() {
    // W7-2：`sg eval` 是正式评估子命令；旧的开发评估参数形态（run manifest.json）不再存在。
    let help = Command::cargo_bin("sg")
        .expect("binary")
        .args(["eval", "--help"])
        .output()
        .expect("sg cli");
    assert!(help.status.success());
    let help_text = stdout(&help);
    assert!(help_text.contains("--config"));
    assert!(help_text.contains("--models"));

    let legacy = Command::cargo_bin("sg")
        .expect("binary")
        .args(["eval", "run", "manifest.json", "--run-id", "run"])
        .output()
        .expect("sg cli");
    assert!(!legacy.status.success());
    let error = stderr(&legacy);
    assert!(error.contains("unrecognized subcommand") || error.contains("unexpected argument"));
    assert!(error.contains("eval"));
}
// 验证完成的 turn 会渲染 AgentLoop 状态与 assistant answer。
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

// 验证失败 turn 退出非零且不泄露 raw payload。
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

// 验证立即 interrupted 的 turn 以非零状态退出。
#[test]
fn cli_exits_nonzero_for_immediate_interrupted_turn() {
    assert_immediate_terminal_turn_exits_nonzero("interrupted", "cancelled");
}

// 验证 continue 遇到非终态 turn 时展示服务端可操作提示（含 turn ID），
// 不重试、不自动 resume，退出非零。
#[test]
fn cli_continue_shows_actionable_nonterminal_turn_hint() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let methods_path = temp.path().join("continue-nonterminal-methods.txt");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .respond(
                "thread/resume",
                json!({"thread": fake_thread("thread_fake")}),
            )
            .error(
                "turn/start",
                JSON_RPC_SERVER_ERROR_CODE,
                "thread already has an active or pending turn turn_active; use sg turn resume/pause/input turn_active",
            )
            .shutdown()
            .trace_methods_to(&methods_path),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["continue", "thread_fake", "next instruction"])
        .output()
        .expect("continue nonterminal cli");

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("use sg turn resume/pause/input turn_active"),
        "stderr={}",
        stderr(&output)
    );
    let methods = std::fs::read_to_string(&methods_path).expect("continue methods");
    assert!(
        !methods.contains("turn/resume"),
        "continue must not auto-resume; methods={methods}"
    );
    assert!(
        !methods.contains("turn/input"),
        "continue must not auto-append input; methods={methods}"
    );
}

// 验证 CLI drop 前先向 app-server 请求 shutdown。
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

// 验证 continue 只恢复 thread 并发送新输入，不上传历史。
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
// 验证无效 thread id 由 app-server 错误原样归因。
#[test]
fn cli_continue_rejects_invalid_thread_id_through_app_server() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .error(
                "thread/resume",
                JSON_RPC_SERVER_ERROR_CODE,
                "Thread not found",
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["continue", "thread_missing", "add docs"])
        .output()
        .expect("continue cli");

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Thread not found"),
        "stderr={}",
        stderr(&output)
    );
}

// 验证 app-server 在响应前退出时 CLI 报告不可用。
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

// 验证没有 notification 的 turn 响应仍能正常完成。
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

// 验证匹配响应到达后不等待其后的无关消息。
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

// 验证 JSON-RPC error 不被吞掉并传递到 CLI stderr。
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

// 验证非匹配成功响应会被忽略，随后继续等待匹配响应。
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

// 验证非匹配错误响应不会污染后续匹配请求。
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

// 验证 app-server 在请求响应前退出时返回稳定错误。
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

// 验证 CLI manifest 不直接依赖核心 runtime crate。
#[test]
fn cli_manifest_does_not_depend_on_core_runtime_crates() {
    let manifest_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = std::fs::read_to_string(manifest_path).expect("read cli manifest");

    for forbidden in ["singularity_agent", "singularity_store"] {
        assert!(
            !manifest.contains(forbidden),
            "cli must not depend directly on {forbidden}"
        );
    }
    assert!(manifest.contains("singularity_protocol"));
}

// 构造指向指定 app-server 和数据库的 CLI 命令。
fn cli_with_app_server(app_server_bin: &str, db_path: &std::path::Path) -> Command {
    let mut command = Command::cargo_bin("sg").expect("binary");
    command.env(APP_SERVER_BIN_ENV, app_server_bin);
    command.env(APP_SERVER_DB_ENV, db_path);
    // 测试使用 stdio fake app-server；显式选择 stdio 承载，避免默认 TCP 连接失败。
    command.env(APP_SERVER_TRANSPORT_ENV, "stdio");
    command
}

// 构造并配置使用 fake app-server 的 CLI 命令。
fn cli_with_fake_app_server(fake_server: &FakeAppServer, db_path: &Path) -> Command {
    let mut command = cli_with_app_server(path_str(fake_server.binary()), db_path);
    fake_server.configure(&mut command);
    command
}

// 复用统一场景断言立即终态会导致 CLI 非零退出。
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

// 返回当前平台预期的 AgentLoop 状态。
fn expected_agent_loop_status() -> &'static str {
    "completed"
}

// 将测试路径转换为 fake server 可消费的 UTF-8 字符串。
fn path_str(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

// 将当前 CLI 二进制复制到隔离目录。
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

// 从测试 crate manifest 推导 workspace 根目录。
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root")
        .to_path_buf()
}

// 断言 stderr 属于 app-server 进程不可用错误。
fn assert_app_server_unavailable_error(output: &std::process::Output) {
    let stderr = stderr(output);
    assert!(is_app_server_unavailable_error(&stderr), "stderr={stderr}");
}

// 判断 stderr 是否表示 app-server 在响应前关闭。
fn is_app_server_unavailable_error(stderr: &str) -> bool {
    stderr.contains("app-server exited before response")
        || stderr.contains("app-server closed stdout")
}

// 解码 CLI stdout，便于测试断言。
fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

// 解码 CLI stderr，便于测试断言。
fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

// 只构建一次 app-server 二进制，供集成测试复用。
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

// 验证 sg trust 命令的查询/设置/重置三态与 wire 形状。
#[test]
fn cli_trust_command_sets_trusted_decision() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    let expected_path = std::fs::canonicalize(&project)
        .expect("canonical project")
        .to_string_lossy()
        .into_owned();
    let set_params = temp.path().join("set_params.json");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .interaction(
                "project/trust",
                vec![
                    capture_params(&set_params),
                    respond(json!({"path": expected_path, "decision": true})),
                ],
            )
            .shutdown(),
    );

    let set = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["trust", path_str(&project), "--decision", "trust"])
        .output()
        .expect("trust set cli");
    assert!(set.status.success(), "stderr={}", stderr(&set));
    assert!(
        stdout(&set).contains("=> trusted"),
        "stdout={}",
        stdout(&set)
    );

    let params: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&set_params).expect("set params"))
            .expect("set params json");
    assert_eq!(params["path"], expected_path);
    assert_eq!(params["decision"], true);
}

#[test]
fn cli_trust_command_queries_without_decision_field() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    let expected_path = std::fs::canonicalize(&project)
        .expect("canonical project")
        .to_string_lossy()
        .into_owned();
    let query_params = temp.path().join("query_params.json");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .interaction(
                "project/trust",
                vec![
                    capture_params(&query_params),
                    respond(json!({"path": expected_path})),
                ],
            )
            .shutdown(),
    );

    let query = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["trust", path_str(&project)])
        .output()
        .expect("trust query cli");
    assert!(query.status.success(), "stderr={}", stderr(&query));
    assert!(
        stdout(&query).contains("=> ask"),
        "stdout={}",
        stdout(&query)
    );

    let params: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&query_params).expect("query params"))
            .expect("query params json");
    assert_eq!(params["path"], expected_path);
    assert!(params.get("decision").is_none(), "query must omit decision");
}

#[test]
fn cli_trust_command_ask_resets_with_null_decision() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    let expected_path = std::fs::canonicalize(&project)
        .expect("canonical project")
        .to_string_lossy()
        .into_owned();
    let ask_params = temp.path().join("ask_params.json");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .interaction(
                "project/trust",
                vec![
                    capture_params(&ask_params),
                    respond(json!({"path": expected_path, "decision": null})),
                ],
            )
            .shutdown(),
    );

    let ask = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["trust", path_str(&project), "--decision", "ask"])
        .output()
        .expect("trust ask cli");
    assert!(ask.status.success(), "stderr={}", stderr(&ask));
    assert!(stdout(&ask).contains("=> ask"), "stdout={}", stdout(&ask));

    let params: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&ask_params).expect("ask params"))
            .expect("ask params json");
    assert_eq!(params["path"], expected_path);
    assert_eq!(params["decision"], serde_json::Value::Null);
}

// 验证非交互 stdin（如 eval 子进程）收到 -32010 时不提示、直接按不信任处理并重试。
#[test]
fn cli_run_auto_declines_trust_required_when_stdin_is_not_a_tty() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let thread = fake_thread("thread_trust");
    let trust_cwd = "C:/workspace";
    let trust_params = temp.path().join("trust_params.json");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .agent_loop_ready()
            .respond("thread/start", json!({"thread": thread.clone()}))
            .interaction(
                "turn/start",
                vec![json!({"respond": {"jsonrpc": "2.0", "error": {"code": -32010, "message": "trust required", "data": {"cwd": trust_cwd}}}})],
            )
            .interaction(
                "turn/start",
                vec![respond(json!({"turn": fake_turn("turn_trust", "thread_trust", "completed", "completed")}))],
            )
            .interaction(
                "project/trust",
                vec![
                    capture_params(&trust_params),
                    respond(json!({"path": trust_cwd, "decision": false})),
                ],
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "stderr={}", stderr(&output));
    // 非交互：不提示，写回不信任决策后重试原请求。
    assert!(!stderr(&output).contains("Trust project folder?"));
    let params: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&trust_params).expect("trust params"))
            .expect("trust params json");
    assert_eq!(params["path"], trust_cwd);
    assert_eq!(params["decision"], false);
}
