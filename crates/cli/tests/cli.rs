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
const EVAL_OUTPUT_DIR_ENV: &str = "SINGULARITY_EVAL_OUTPUT_DIR";
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
        let scenario = self.respond(
            "initialize",
            json!({"userAgent":"fake","platformFamily":"local","platformOs":"test"}),
        );
        scenario.interaction(
            "event/subscribe",
            vec![
                send(json!({
                    "method": "event/gap",
                    "params": {
                        "gap": {"reason": "cursor_not_replayed", "fromCursor": 1, "toCursor": 1},
                        "event": {
                            "sequence": 1,
                            "cursor": 1,
                            "class": "gap",
                            "delivery": "gap",
                            "gap": {"reason": "cursor_not_replayed", "fromCursor": 1, "toCursor": 1}
                        }
                    }
                })),
                respond(json!({
                    "subscriptionId": "subscription_app_server_events",
                    "eventTypes": [],
                    "cursor": 1
                })),
            ],
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
    let mut thread = raw_fake_thread(thread_id);
    thread["sandboxMode"] = json!("workspace-write");
    thread["approvalPolicy"] = json!("on-request");
    thread
}

fn fake_trace_event(event_id: &str, summary: &str) -> serde_json::Value {
    json!({
        "event_id": event_id,
        "event_type": "trace.event",
        "run_id": "run_fake",
        "session_id": "session_fake",
        "task_id": null,
        "phase_id": null,
        "action_id": null,
        "parent_event_id": null,
        "timestamp": null,
        "monotonic_ms": null,
        "component": "thread",
        "severity": "info",
        "summary": summary,
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
    })
}

// 构造 trace/metrics 的类型化响应测试数据。
fn fake_trace_metrics_result(run_id: &str, metrics: serde_json::Value) -> serde_json::Value {
    json!({
        "metrics": {
            "runId": run_id,
            "metrics": metrics,
        }
    })
}

// 验证 metrics CLI 只请求服务端派生结果，并原样渲染服务端聚合值。
#[test]
fn cli_trace_metrics_requests_typed_rpc_and_renders_server_aggregates() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let params_path = temp.path().join("trace-metrics-params.json");
    let methods_path = temp.path().join("trace-metrics-methods.txt");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .interaction(
                "trace/metrics",
                vec![
                    capture_params(&params_path),
                    respond(fake_trace_metrics_result(
                        "run_metrics",
                        json!([
                            {
                                "name": "task_duration_ms",
                                "availability": {"state": "available"},
                                "distribution": {
                                    "count": 7,
                                    "sum": 987654,
                                    "min": 13,
                                    "max": 9999,
                                    "mean": 123.456789,
                                    "p50": 321,
                                    "p95": 9876
                                }
                            },
                            {
                                "name": "provider_error_count",
                                "availability": {"state": "available"},
                                "distribution": {
                                    "count": 1,
                                    "sum": 0,
                                    "min": 0,
                                    "max": 0,
                                    "mean": 0.0,
                                    "p50": 0,
                                    "p95": 0
                                }
                            },
                            {
                                "name": "provider_input_tokens",
                                "availability": {
                                    "state": "unavailable",
                                    "reason": "missing_usage"
                                }
                            }
                        ]),
                    )),
                ],
            )
            .shutdown()
            .trace_methods_to(&methods_path),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["trace", "metrics", "run_metrics"])
        .output()
        .expect("trace metrics cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    let params: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(params_path).expect("trace metrics params"))
            .expect("trace metrics params json");
    assert_eq!(params, json!({"runId": "run_metrics"}));

    let methods = std::fs::read_to_string(methods_path).expect("trace metrics methods");
    assert_eq!(
        methods,
        "initialize\ninitialized\nevent/subscribe\ntrace/metrics\nserver/shutdown\n"
    );

    assert_eq!(
        stdout(&output),
        concat!(
            "trace metrics run_metrics\n",
            "metric task_duration_ms available count=7 sum=987654 min=13 max=9999 mean=123.456789 p50=321 p95=9876\n",
            "metric provider_error_count available count=1 sum=0 min=0 max=0 mean=0.000000 p50=0 p95=0\n",
            "metric provider_input_tokens unavailable reason=missing_usage\n",
        )
    );
}

// 验证 typed metrics 结果必须绑定到 CLI 请求的 run id。
#[test]
fn cli_trace_metrics_rejects_response_for_different_run() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let params_path = temp.path().join("trace-metrics-mismatch-params.json");
    let methods_path = temp.path().join("trace-metrics-mismatch-methods.txt");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .interaction(
                "trace/metrics",
                vec![
                    capture_params(&params_path),
                    respond(fake_trace_metrics_result("run_other", json!([]))),
                ],
            )
            .shutdown()
            .trace_methods_to(&methods_path),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .args(["trace", "metrics", "run_expected"])
        .output()
        .expect("trace metrics mismatch cli");

    assert!(!output.status.success());
    assert!(stdout(&output).is_empty());
    assert_eq!(stderr(&output), "trace metrics response run mismatch\n");

    let params: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(params_path).expect("trace metrics mismatch params"),
    )
    .expect("trace metrics mismatch params json");
    assert_eq!(params, json!({"runId": "run_expected"}));

    let methods = std::fs::read_to_string(methods_path).expect("trace metrics mismatch methods");
    assert_eq!(
        methods,
        "initialize\ninitialized\nevent/subscribe\ntrace/metrics\nserver/shutdown\n"
    );
}

// 验证 availability 与 distribution 不一致时 CLI fail closed。
#[test]
fn cli_trace_metrics_rejects_malformed_availability_distribution() {
    for (metric, expected_error) in [
        (
            json!({
                "name": "task_duration_ms",
                "availability": {"state": "available"}
            }),
            "metric task_duration_ms is available without distribution",
        ),
        (
            json!({
                "name": "provider_input_tokens",
                "availability": {
                    "state": "unavailable",
                    "reason": "missing_usage"
                },
                "distribution": {
                    "count": 1,
                    "sum": 7,
                    "min": 7,
                    "max": 7,
                    "mean": 7.0,
                    "p50": 7,
                    "p95": 7
                }
            }),
            "metric provider_input_tokens is unavailable with distribution",
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        let db_path = temp.path().join("sessions.sqlite3");
        let fake_server = FakeAppServer::new(
            temp.path(),
            Scenario::new()
                .initialized()
                .respond(
                    "trace/metrics",
                    fake_trace_metrics_result("run_metrics", json!([metric])),
                )
                .shutdown(),
        );

        let output = cli_with_fake_app_server(&fake_server, &db_path)
            .args(["trace", "metrics", "run_metrics"])
            .output()
            .expect("malformed trace metrics cli");

        assert!(!output.status.success());
        assert_eq!(
            stderr(&output),
            format!("invalid trace metrics response: {expected_error}\n")
        );
        assert!(stdout(&output).is_empty());
    }
}

// 确认 CLI 能启动并暴露 app-server 协议模式。
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
    for internal in ["protocol-init", "thread-start", "daemon"] {
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

// 验证 CLI 只发送受控的 thread sandbox/approval 枚举，并渲染服务端快照。
#[test]
fn cli_run_sends_and_renders_thread_policy_snapshot() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let params_path = temp.path().join("thread_start_params.json");
    let mut thread = fake_thread("thread_policy");
    thread["sandboxMode"] = json!("read-only");
    thread["approvalPolicy"] = json!("never");
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
        .args([
            "run",
            "write tests",
            "--sandbox-mode",
            "read-only",
            "--approval-policy",
            "never",
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(stdout(&output).contains("thread_policy sandbox_mode=read-only approval_policy=never"));

    let params: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(params_path).expect("thread params"))
            .expect("thread params json");
    assert_eq!(params["sandboxMode"], "read-only");
    assert_eq!(params["approvalPolicy"], "never");
}

// 验证 run、continue、threads、trace、approval 与 doctor 共用 app-server 协议。
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
                json!({"events": [fake_trace_event("trace_1", "thread started")]}),
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

// 验证 doctor 输出脱敏的 AgentLoop 与 provider readiness。
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

// 验证缺少可确认的 AgentLoop 终态时拒绝 turn。
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

// 验证 JSON 模式保留 blocked 状态并以失败退出。
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

// 验证 eval 命令可脚本化校验 manifest 并拒绝缺失文件。
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
    assert!(stderr(&invalid).contains("app-server error: Invalid params"));
}

// 验证 eval run 通过 app-server 返回并渲染 verification 结果。
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
  "schema_version": "evaluation.task_set/v5",
  "trial_count": 1,
  "tasks": [{
    "task_id": "fixture_agent",
    "description": "Exercise the AgentLoop evaluation transport.",
    "capabilities": ["single_file_fix", "required_verification"],
    "workspace": {"source": {"type": "local", "path": "."}},
    "agent": {
      "instructions": "Change solution.txt so value is 1.",
      "allowed_paths": ["solution.txt"],
      "required_tool_capabilities": [
        {"capability": "workspace_read", "minimum_version": 1},
        {"capability": "workspace_write", "minimum_version": 1}
      ]
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
                                "local_process_fallback_count": 0,
                                "local_process_fallback_unknown_count": 0
                            }
                        }],
                        "result_path": path_str(&eval_output.join("eval_agent/result.json")),
                        "report_path": path_str(&eval_output.join("eval_agent/report.json")),
                        "evidence_path": path_str(&eval_output.join("eval_agent/evidence.json"))
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
    assert_eq!(
        value["tasks"][0]["diagnostics"]["local_process_fallback_unknown_count"],
        0
    );
    assert!(value["result_path"].as_str().is_some());
    assert!(value["report_path"].as_str().is_some());
    assert!(value["evidence_path"].as_str().is_some());
    let params: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(agent_loop_trace).expect("agent loop trace"))
            .expect("agent loop turn params");
    assert_eq!(params["runId"], "eval_agent");
    assert_eq!(params["manifest"], path_str(&manifest));
    assert_eq!(params["outputRoot"], path_str(&eval_output));
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

// 验证立即 blocked 的 turn 以非零状态退出。
#[test]
fn cli_exits_nonzero_for_immediate_blocked_turn() {
    assert_immediate_terminal_turn_exits_nonzero("blocked", "blocked");
}

// 验证立即 interrupted 的 turn 以非零状态退出。
#[test]
fn cli_exits_nonzero_for_immediate_interrupted_turn() {
    assert_immediate_terminal_turn_exits_nonzero("interrupted", "cancelled");
}

// 验证 turn、approval 与 trace 查询都通过 app-server 协议渲染。
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
                        "request_id": "approval_fake",
                        "decision_id": "approval_fake_decision",
                        "outcome": "allow",
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

// 验证 turn 生命周期查询和中断输出 AgentLoop 状态。
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

// 验证 app-server 拒绝 turn interrupt 时 CLI 以非零退出。
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

#[test]
fn cli_initialize_requires_cursor_gap_before_subscription_response() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .respond(
                "initialize",
                json!({"userAgent":"fake","platformFamily":"local","platformOs":"test"}),
            )
            .respond(
                "event/subscribe",
                json!({
                    "subscriptionId":"subscription_app_server_events",
                    "eventTypes":[],
                    "cursor":1
                }),
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .arg("threads")
        .output()
        .expect("threads cli");

    assert!(!output.status.success());
    assert!(stderr(&output).contains("cursor-not-replayed gap"));
}

#[test]
fn cli_rejects_undeclared_sequence_gap_before_matching_response() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .interaction(
                "thread/list",
                vec![
                    send(json!({
                        "method":"thread/started",
                        "params":{
                            "thread":{"thread_id":"thread_gap","model":null,"cwd":null,"status":"active","sandboxMode":"workspace-write","approvalPolicy":"on-request"},
                            "event":{"sequence":4,"cursor":4,"class":"state","delivery":"reliable","recoveryQuery":{"method":"thread/read","params":{"threadId":"thread_gap"}}}
                        }
                    })),
                    respond(json!({"threads":[]})),
                ],
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .arg("threads")
        .output()
        .expect("threads cli");

    assert!(!output.status.success());
    assert!(stderr(&output).contains("event cursor gap is not declared"));
}

#[test]
fn cli_rejects_unsupported_event_recovery_query() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = FakeAppServer::new(
        temp.path(),
        Scenario::new()
            .initialized()
            .interaction(
                "thread/list",
                vec![
                    send(json!({
                        "method":"turn/diff/updated",
                        "params":{
                            "turnId":"turn_diff",
                            "event":{"sequence":2,"cursor":2,"class":"state","delivery":"reliable","recoveryQuery":{"method":"store/resync","params":{}}}
                        }
                    })),
                    respond(json!({"threads":[]})),
                ],
            )
            .shutdown(),
    );

    let output = cli_with_fake_app_server(&fake_server, &db_path)
        .arg("threads")
        .output()
        .expect("threads cli");

    assert!(!output.status.success());
    assert!(stderr(&output).contains("invalid typed metadata"));
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

// 验证 running turn 在关闭 app-server 前轮询到终态。
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

// 验证轮询期间进入 interrupted 的 turn 以非零退出。
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

// 构造指向指定 app-server 和数据库的 CLI 命令。
fn cli_with_app_server(app_server_bin: &str, db_path: &std::path::Path) -> Command {
    let mut command = Command::cargo_bin("sg").expect("binary");
    command.env(APP_SERVER_BIN_ENV, app_server_bin);
    command.env(APP_SERVER_DB_ENV, db_path);
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

// 定位或构建 app-server 二进制供生命周期测试使用。
fn app_server_bin() -> String {
    std::env::var("CARGO_BIN_EXE_singularity_app_server").unwrap_or_else(|_| {
        ensure_app_server_binary();
        let profile_dir = std::env::current_exe()
            .expect("current test binary")
            .parent()
            .and_then(Path::parent)
            .expect("Cargo test profile directory")
            .to_path_buf();
        profile_dir
            .join(format!(
                "singularity_app_server{}",
                std::env::consts::EXE_SUFFIX
            ))
            .to_string_lossy()
            .into_owned()
    })
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
