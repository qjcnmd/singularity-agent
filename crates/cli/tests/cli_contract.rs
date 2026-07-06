use assert_cmd::Command;
use std::path::{Path, PathBuf};
use std::sync::Once;

const APP_SERVER_BIN_ENV: &str = "SINGULARITY_APP_SERVER_BIN";
const APP_SERVER_DB_ENV: &str = "SINGULARITY_APP_SERVER_DB";
const FAKE_APP_SERVER_EXIT_CODE: i32 = 7;

#[test]
fn cli_exposes_app_server_protocol_mode_without_direct_core_runtime() {
    let mut command = Command::cargo_bin("sg").expect("binary");
    command.arg("--help").assert().success();
}

#[test]
fn cli_run_continue_threads_trace_and_approvals_use_app_server_protocol() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let app_server_bin = app_server_bin();

    let run = cli_with_app_server(&app_server_bin, &db_path)
        .args(["run", "write tests", "--model", "gpt-test"])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "stderr={}", stderr(&run));
    let run_stdout = stdout(&run);
    assert!(run_stdout.contains("thread/started"));
    assert!(run_stdout.contains("turn/started"));
    assert!(run_stdout.contains("item/agentMessage/delta"));
    let thread_id = run_stdout
        .lines()
        .find_map(|line| line.strip_prefix("thread "))
        .expect("thread id")
        .to_string();

    let threads = cli_with_app_server(&app_server_bin, &db_path)
        .arg("threads")
        .output()
        .expect("threads cli");
    assert!(threads.status.success(), "stderr={}", stderr(&threads));
    assert!(stdout(&threads).contains(&thread_id));

    let continued = cli_with_app_server(&app_server_bin, &db_path)
        .args(["continue", &thread_id, "add docs"])
        .output()
        .expect("continue cli");
    assert!(continued.status.success(), "stderr={}", stderr(&continued));
    assert!(stdout(&continued).contains("turn/started"));

    let trace = cli_with_app_server(&app_server_bin, &db_path)
        .args(["trace", &thread_id, "--limit", "5"])
        .output()
        .expect("trace cli");
    assert!(trace.status.success(), "stderr={}", stderr(&trace));
    assert!(stdout(&trace).contains("thread started"));

    let approvals = cli_with_app_server(&app_server_bin, &db_path)
        .arg("approvals")
        .output()
        .expect("approvals cli");
    assert!(approvals.status.success(), "stderr={}", stderr(&approvals));

    let doctor = cli_with_app_server(&app_server_bin, &db_path)
        .args(["config", "doctor"])
        .output()
        .expect("doctor cli");
    assert!(doctor.status.success(), "stderr={}", stderr(&doctor));
    assert!(stdout(&doctor).contains("client=protocol-only"));
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
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "thread/start":
        print(json.dumps({"id": request_id, "result": {"thread": {"thread_id": "thread_fake", "model": None, "cwd": None, "status": "active"}}}), flush=True)
    elif method == "turn/start":
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": "turn_fake", "thread_id": "thread_fake", "status": "running", "agent_loop_status": "not_migrated"}}}), flush=True)
"#,
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
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
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "thread/start":
        print(json.dumps({"id": request_id, "result": {"thread": {"thread_id": "thread_fake", "model": None, "cwd": None, "status": "active"}}}), flush=True)
    elif method == "turn/start":
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": "turn_fake", "thread_id": "thread_fake", "status": "running", "agent_loop_status": "not_migrated"}}}), flush=True)
        print(json.dumps({"id": 999, "result": {"late": True}}), flush=True)
"#,
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["run", "write tests"])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(stdout(&output).contains("thread thread_fake"));
}

#[test]
fn cli_reports_json_rpc_error_without_swallowing_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    request_id = message.get("id")
    print(json.dumps({"id": request_id, "error": {"code": -32000, "message": "forced failure"}}), flush=True)
"#,
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
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
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import sys

thread_started = False

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "thread/start":
        print(json.dumps({"id": request_id, "result": {"thread": {"thread_id": "thread_fake", "model": None, "cwd": None, "status": "active"}}}), flush=True)
    elif method == "turn/start" and not thread_started:
        thread_started = True
        print(json.dumps({"id": 999, "result": {"turn": {"turn_id": "wrong_turn", "thread_id": "thread_fake", "status": "running", "agent_loop_status": "not_migrated"}}}), flush=True)
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": "turn_fake", "thread_id": "thread_fake", "status": "running", "agent_loop_status": "not_migrated"}}}), flush=True)
"#,
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
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
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import sys

thread_started = False

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "thread/start":
        print(json.dumps({"id": request_id, "result": {"thread": {"thread_id": "thread_fake", "model": None, "cwd": None, "status": "active"}}}), flush=True)
    elif method == "turn/start" and not thread_started:
        thread_started = True
        print(json.dumps({"id": 999, "error": {"code": -32000, "message": "stale failure"}}), flush=True)
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": "turn_fake", "thread_id": "thread_fake", "status": "running", "agent_loop_status": "not_migrated"}}}), flush=True)
"#,
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
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
    let fake_server = write_fake_app_server(
        temp.path(),
        &format!("import sys\nsys.exit({FAKE_APP_SERVER_EXIT_CODE})\n"),
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
        .arg("threads")
        .output()
        .expect("threads cli");

    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert!(is_app_server_unavailable_error(&stderr), "stderr={stderr}");
}

#[test]
fn cli_outputs_json_rpc_initialize_request() {
    let mut command = Command::cargo_bin("sg").expect("binary");
    let output = command.arg("protocol-init").output().expect("run cli");
    assert!(output.status.success());

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("json-rpc initialize output");
    assert_eq!(value["method"], "initialize");
    assert_eq!(value["params"]["clientInfo"]["name"], "singularity_cli");
    assert!(value.get("jsonrpc").is_none());
    assert!(value.get("error").is_none());
    assert!(value.get("result").is_none());
}

#[test]
fn cli_outputs_json_rpc_thread_start_request() {
    let mut command = Command::cargo_bin("sg").expect("binary");
    let output = command
        .args(["thread-start", "--model", "gpt-test"])
        .output()
        .expect("run cli");
    assert!(output.status.success());

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("json-rpc thread/start output");
    assert_eq!(value["method"], "thread/start");
    assert_eq!(value["params"]["model"], "gpt-test");
}

#[test]
fn cli_manifest_does_not_depend_on_core_runtime_crates() {
    let manifest_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = std::fs::read_to_string(manifest_path).expect("read cli manifest");

    for forbidden in [
        "singularity_agent",
        "singularity_model",
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

fn write_fake_app_server(dir: &Path, script: &str) -> PathBuf {
    let script_path = dir.join("fake_app_server.py");
    std::fs::write(&script_path, script).expect("write fake app-server script");
    if cfg!(windows) {
        let launcher = dir.join("fake_app_server.cmd");
        std::fs::write(
            &launcher,
            format!(
                "@echo off\r\npython \"{}\"\r\nexit /b %ERRORLEVEL%\r\n",
                script_path.display()
            ),
        )
        .expect("write fake app-server launcher");
        launcher
    } else {
        let launcher = dir.join("fake_app_server");
        std::fs::write(
            &launcher,
            format!("#!/bin/sh\nexec python3 '{}' \n", script_path.display()),
        )
        .expect("write fake app-server launcher");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(&launcher)
                .expect("fake launcher metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&launcher, permissions).expect("fake launcher executable");
        }
        launcher
    }
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
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
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.pop();
        path.pop();
        path.push("target");
        path.push("debug");
        path.push(format!(
            "singularity_app_server{}",
            std::env::consts::EXE_SUFFIX
        ));
        path.to_string_lossy().to_string()
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
            ])
            .current_dir(workspace_root())
            .status()
            .expect("build app-server binary");
        assert!(status.success(), "failed to build app-server binary");
    });
}
