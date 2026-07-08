use assert_cmd::Command;
use std::path::{Path, PathBuf};
use std::sync::Once;

const APP_SERVER_BIN_ENV: &str = "SINGULARITY_APP_SERVER_BIN";
const APP_SERVER_DB_ENV: &str = "SINGULARITY_APP_SERVER_DB";
const EVAL_OUTPUT_DIR_ENV: &str = "SINGULARITY_EVAL_OUTPUT_DIR";
const PYTHON_SIDECAR_ENV: &str = "SINGULARITY_PYTHON_SIDECAR";
const PYTHON_SIDECAR_PROJECT_ROOT_ENV: &str = "SINGULARITY_SIDECAR_PROJECT_ROOT";
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
    let app_server_bin = write_fake_app_server(
        temp.path(),
        r#"
import json
import sys

THREAD = {"thread_id": "thread_fake", "model": None, "cwd": None, "status": "active"}
TURN = {"turn_id": "turn_fake", "thread_id": "thread_fake", "status": "completed", "agent_loop_status": "completed"}

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "agent/capability":
        print(json.dumps({"id": request_id, "result": {"nativeAgentLoop": {"available": True, "status": "completed", "reason": "enabled", "blockers": []}}}), flush=True)
    elif method == "thread/start":
        print(json.dumps({"method": "thread/started", "params": {"thread": THREAD}}), flush=True)
        print(json.dumps({"id": request_id, "result": {"thread": THREAD}}), flush=True)
    elif method == "thread/read":
        print(json.dumps({"id": request_id, "result": {"thread": THREAD}}), flush=True)
    elif method == "thread/list":
        print(json.dumps({"id": request_id, "result": {"threads": [THREAD]}}), flush=True)
    elif method == "turn/start":
        print(json.dumps({"method": "turn/started", "params": {"turn": TURN}}), flush=True)
        print(json.dumps({"id": request_id, "result": {"turn": TURN}}), flush=True)
    elif method == "trace/tail":
        print(json.dumps({"id": request_id, "result": {"events": [{"event_id": "trace_1", "component": "thread", "summary": "thread started"}]}}), flush=True)
    elif method == "approval/list":
        print(json.dumps({"id": request_id, "result": {"approvals": []}}), flush=True)
    elif method == "server/shutdown":
        print(json.dumps({"id": request_id, "result": {"shutdown": True}}), flush=True)
        break
"#,
    );

    let app_server_bin = path_str(&app_server_bin);

    let run = cli_with_app_server(app_server_bin, &db_path)
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

    let threads = cli_with_app_server(app_server_bin, &db_path)
        .arg("threads")
        .output()
        .expect("threads cli");
    assert!(threads.status.success(), "stderr={}", stderr(&threads));
    assert!(stdout(&threads).contains(&thread_id));

    let continued = cli_with_app_server(app_server_bin, &db_path)
        .args(["continue", &thread_id, "add docs"])
        .output()
        .expect("continue cli");
    assert!(continued.status.success(), "stderr={}", stderr(&continued));
    assert!(stdout(&continued).contains("turn/started"));

    let trace = cli_with_app_server(app_server_bin, &db_path)
        .args(["trace", &thread_id, "--limit", "5"])
        .output()
        .expect("trace cli");
    assert!(trace.status.success(), "stderr={}", stderr(&trace));
    assert!(stdout(&trace).contains("thread started"));

    let approvals = cli_with_app_server(app_server_bin, &db_path)
        .arg("approvals")
        .output()
        .expect("approvals cli");
    assert!(approvals.status.success(), "stderr={}", stderr(&approvals));

    let doctor = cli_with_app_server(app_server_bin, &db_path)
        .args(["config", "doctor"])
        .output()
        .expect("doctor cli");
    assert!(doctor.status.success(), "stderr={}", stderr(&doctor));
    assert!(stdout(&doctor).contains("client=protocol-only"));
}

#[test]
fn cli_config_doctor_reports_redacted_native_sidecar_and_eval_readiness() {
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
    elif method == "agent/capability":
        print(json.dumps({"id": request_id, "result": {"nativeAgentLoop": {"available": True, "status": "completed", "reason": "enabled", "blockers": []}}}), flush=True)
    elif method == "server/shutdown":
        print(json.dumps({"id": request_id, "result": {"shutdown": True}}), flush=True)
        break
"#,
    );

    let doctor = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["config", "doctor"])
        .env("SINGULARITY_API_KEY", "secret-value")
        .env("SINGULARITY_BASE_URL", "https://provider.example/v1")
        .env("SINGULARITY_MODEL", "gpt-test")
        .output()
        .expect("doctor cli");

    assert!(doctor.status.success(), "stderr={}", stderr(&doctor));
    let stdout = stdout(&doctor);
    assert!(stdout.contains("client=protocol-only"));
    assert!(stdout.contains("native_agent_loop=completed"));
    assert!(stdout.contains("sidecar_oracle=explicit"));
    assert!(stdout.contains("evaluation=rust_native"));
    assert!(stdout.contains("SINGULARITY_API_KEY=present(redacted)"));
    assert!(stdout.contains("SINGULARITY_BASE_URL=present(redacted)"));
    assert!(stdout.contains("SINGULARITY_MODEL=present(redacted)"));
    assert!(!stdout.contains("secret-value"));
    assert!(!stdout.contains("https://provider.example/v1"));
    assert!(!stdout.contains("gpt-test"));
}

#[test]
fn cli_rejects_native_run_when_capability_is_disabled() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let trace_path = temp.path().join("native_disabled_methods.txt");
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import os
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    with open(os.environ["METHOD_TRACE"], "a", encoding="utf-8") as trace:
        trace.write(f"{method}\n")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "agent/capability":
        print(json.dumps({"id": request_id, "result": {"nativeAgentLoop": {"available": False, "status": "not_migrated", "reason": "not migrated", "blockers": ["model_provider_adapter"]}}}), flush=True)
    elif method == "server/shutdown":
        print(json.dumps({"id": request_id, "result": {"shutdown": True}}), flush=True)
        break
"#,
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["run", "write tests", "--agent-host", "native"])
        .env("METHOD_TRACE", &trace_path)
        .output()
        .expect("run cli");

    assert!(!output.status.success());
    assert!(stderr(&output).contains("native AgentLoop is not production-ready"));
    let trace = std::fs::read_to_string(trace_path).expect("method trace");
    assert!(trace.contains("initialize"));
    assert!(trace.contains("agent/capability"));
    assert!(!trace.contains("turn/start"));
}

#[test]
fn cli_sends_native_agent_host_after_capability_allows_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let trace_path = temp.path().join("native_enabled_turn.json");
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import os
import pathlib
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "agent/capability":
        print(json.dumps({"id": request_id, "result": {"nativeAgentLoop": {"available": True, "status": "running", "reason": "enabled", "blockers": []}}}), flush=True)
    elif method == "thread/start":
        print(json.dumps({"id": request_id, "result": {"thread": {"thread_id": "thread_native", "model": None, "cwd": None, "status": "active"}}}), flush=True)
    elif method == "turn/start":
        pathlib.Path(os.environ["TURN_TRACE"]).write_text(json.dumps(message["params"]), encoding="utf-8")
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": "turn_native", "thread_id": "thread_native", "status": "running", "agent_loop_status": "not_migrated"}}}), flush=True)
    elif method == "server/shutdown":
        print(json.dumps({"id": request_id, "result": {"shutdown": True}}), flush=True)
        break
"#,
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["run", "write tests", "--agent-host", "native"])
        .env("TURN_TRACE", &trace_path)
        .output()
        .expect("run cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    let params: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(trace_path).expect("turn trace"))
            .expect("turn trace json");
    assert_eq!(params["agentHost"], "native");
    assert_eq!(params["threadId"], "thread_native");
}

#[test]
fn cli_rejects_partial_native_capability_until_blockers_clear() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let trace_path = temp.path().join("partial_native_methods.txt");
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import os
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    with open(os.environ["METHOD_TRACE"], "a", encoding="utf-8") as trace:
        trace.write(f"{method}\n")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "agent/capability":
        print(json.dumps({"id": request_id, "result": {"nativeAgentLoop": {"available": True, "status": "running", "reason": "partial", "blockers": ["strict_command_sandbox"]}}}), flush=True)
    elif method == "server/shutdown":
        print(json.dumps({"id": request_id, "result": {"shutdown": True}}), flush=True)
        break
"#,
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["run", "write tests", "--agent-host", "native"])
        .env("METHOD_TRACE", &trace_path)
        .output()
        .expect("run cli");

    assert!(!output.status.success());
    assert!(stderr(&output).contains("native AgentLoop is not production-ready"));
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
fn cli_eval_run_uses_native_app_server_and_reports_verification_result() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let eval_output = temp.path().join("eval-output");
    let manifest = temp.path().join("eval.json");
    let native_trace = temp.path().join("native-turn.json");
    let sidecar_trace = temp.path().join("sidecar-env.txt");
    std::fs::write(
        &manifest,
        format!(
            r#"{{
  "schema_version": "evaluation.task_set/v1",
  "tasks": [{{
    "task_id": "fixture_native",
    "workspace": {{"type": "fixture", "files": {{"solution.py": "value = 0\n"}}}},
    "user_task": "Change solution.py so value is 1.",
    "allowed_paths": ["solution.py"],
    "expected_file_changes": ["solution.py"],
    "verification_command": "{} -c \"from solution import value; assert value == 1\"",
    "public_verification_command": "{} -c \"from solution import value; assert value == 1\"",
    "hidden_verification_command": "{} -c \"from solution import value; assert value == 1\"",
    "success": {{"type": "verification_exit_code", "exit_code": 0}}
  }}]
}}"#,
            python_bin(),
            python_bin(),
            python_bin()
        ),
    )
    .expect("write manifest");
    let fake_server = write_fake_app_server(
        temp.path(),
        &format!(
            r#"
import json
import os
import pathlib
import sys

native_trace = pathlib.Path(r"{}")
sidecar_trace = pathlib.Path(r"{}")
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({{"id": request_id, "result": {{"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}}}), flush=True)
    elif method == "eval/run":
        native_trace.write_text(json.dumps(message["params"]), encoding="utf-8")
        sidecar_trace.write_text(os.environ.get("SINGULARITY_PYTHON_SIDECAR", "unset"), encoding="utf-8")
        print(json.dumps({{"id": request_id, "result": {{"run_id": "eval_native", "manifest": message["params"]["manifest"], "runner": "rust_native", "status": "completed", "blocker": None, "evaluation_passed": True, "tasks": [{{"task_id": "fixture_native", "agent_completed": True, "tests_passed": True, "evaluation_passed": True, "local_process_fallback_count": 0}}]}}}}), flush=True)
    elif method == "server/shutdown":
        print(json.dumps({{"id": request_id, "result": {{"shutdown": True}}}}), flush=True)
        break
"#,
            native_trace.display(),
            sidecar_trace.display()
        ),
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
        .args([
            "eval",
            "run",
            path_str(&manifest),
            "--run-id",
            "eval_native",
            "--json",
        ])
        .env(EVAL_OUTPUT_DIR_ENV, &eval_output)
        .env(PYTHON_SIDECAR_ENV, "1")
        .output()
        .expect("eval cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("eval json");
    assert_eq!(value["runner"], "rust_native");
    assert_eq!(value["evaluation_passed"], true);
    assert_eq!(value["tasks"][0]["agent_completed"], true);
    assert_eq!(value["tasks"][0]["tests_passed"], true);
    assert_eq!(value["tasks"][0]["local_process_fallback_count"], 0);
    let params: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(native_trace).expect("native trace"))
            .expect("native turn params");
    assert_eq!(params["runId"], "eval_native");
    assert_eq!(params["manifest"], path_str(&manifest));
    assert_eq!(params["outputRoot"], path_str(&eval_output));
    assert_eq!(
        std::fs::read_to_string(sidecar_trace).expect("sidecar trace"),
        "unset"
    );
}

#[test]
fn cli_run_can_enable_python_sidecar_without_raw_env_plumbing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import os
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "agent/capability":
        print(json.dumps({"id": request_id, "result": {"nativeAgentLoop": {"available": True, "status": "completed", "reason": "enabled", "blockers": []}}}), flush=True)
    elif method == "thread/start":
        print(json.dumps({"id": request_id, "result": {"thread": {"thread_id": "thread_fake", "model": None, "cwd": None, "status": "active"}}}), flush=True)
    elif method == "turn/start":
        enabled = os.environ.get("SINGULARITY_PYTHON_SIDECAR") == "1"
        project_root = bool(os.environ.get("SINGULARITY_SIDECAR_PROJECT_ROOT"))
        status = "completed" if enabled and project_root else "failed"
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": "turn_fake", "thread_id": "thread_fake", "status": status, "agent_loop_status": status}}}), flush=True)
"#,
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["run", "write tests", "--agent-host", "python"])
        .env_remove(PYTHON_SIDECAR_ENV)
        .env_remove(PYTHON_SIDECAR_PROJECT_ROOT_ENV)
        .output()
        .expect("run cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(stdout(&output).contains("agent_loop_status=completed"));
}

#[test]
fn cli_default_run_does_not_inherit_python_sidecar_env() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import os
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "agent/capability":
        print(json.dumps({"id": request_id, "result": {"nativeAgentLoop": {"available": True, "status": "completed", "reason": "enabled", "blockers": []}}}), flush=True)
    elif method == "thread/start":
        print(json.dumps({"id": request_id, "result": {"thread": {"thread_id": "thread_fake", "model": None, "cwd": None, "status": "active"}}}), flush=True)
    elif method == "turn/start":
        inherited = os.environ.get("SINGULARITY_PYTHON_SIDECAR") == "1"
        status = "failed" if inherited else "completed"
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": "turn_fake", "thread_id": "thread_fake", "status": status, "agent_loop_status": status}}}), flush=True)
"#,
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["run", "write tests"])
        .env(PYTHON_SIDECAR_ENV, "1")
        .output()
        .expect("run cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(stdout(&output).contains("agent_loop_status=completed"));
}

#[test]
fn cli_continue_can_enable_python_sidecar_without_raw_env_plumbing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import os
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "thread/read":
        print(json.dumps({"id": request_id, "result": {"thread": {"thread_id": "thread_fake", "model": None, "cwd": None, "status": "active"}}}), flush=True)
    elif method == "turn/start":
        enabled = os.environ.get("SINGULARITY_PYTHON_SIDECAR") == "1"
        project_root = bool(os.environ.get("SINGULARITY_SIDECAR_PROJECT_ROOT"))
        status = "completed" if enabled and project_root else "failed"
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": "turn_fake", "thread_id": "thread_fake", "status": status, "agent_loop_status": status}}}), flush=True)
"#,
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
        .args([
            "continue",
            "thread_fake",
            "add docs",
            "--agent-host",
            "python",
        ])
        .env_remove(PYTHON_SIDECAR_ENV)
        .env_remove(PYTHON_SIDECAR_PROJECT_ROOT_ENV)
        .output()
        .expect("continue cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(stdout(&output).contains("agent_loop_status=completed"));
}

#[test]
fn cli_renders_agent_host_status_and_sidecar_answer() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import os
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "thread/start":
        print(json.dumps({"method": "thread/started", "params": {"thread": {"thread_id": "thread_fake", "model": None, "cwd": None, "status": "active"}}}), flush=True)
        print(json.dumps({"id": request_id, "result": {"thread": {"thread_id": "thread_fake", "model": None, "cwd": None, "status": "active"}}}), flush=True)
    elif method == "turn/start":
        print(json.dumps({"method": "turn/started", "params": {"turn": {"turn_id": "turn_fake", "thread_id": "thread_fake", "status": "completed", "agent_loop_status": "completed"}}}), flush=True)
        print(json.dumps({"method": "item/agentMessage/delta", "params": {"item": {"item_id": "item_fake"}, "delta": "sidecar completed"}}), flush=True)
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": "turn_fake", "thread_id": "thread_fake", "status": "completed", "agent_loop_status": "completed"}}}), flush=True)
"#,
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["run", "write tests", "--agent-host", "python"])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    let stdout = stdout(&output);
    assert!(stdout.contains("thread thread_fake"));
    assert!(stdout.contains("turn turn_fake completed agent_loop_status=completed"));
    assert!(stdout.contains("assistant sidecar completed"));
}

#[test]
fn cli_exits_nonzero_for_sidecar_failed_turn_without_raw_payload() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import os
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "agent/capability":
        print(json.dumps({"id": request_id, "result": {"nativeAgentLoop": {"available": True, "status": "completed", "reason": "enabled", "blockers": []}}}), flush=True)
    elif method == "thread/start":
        print(json.dumps({"id": request_id, "result": {"thread": {"thread_id": "thread_fake", "model": None, "cwd": None, "status": "active"}}}), flush=True)
    elif method == "turn/start":
        print(json.dumps({"method": "item/agentMessage/delta", "params": {"item": {"item_id": "item_fake"}, "delta": "sidecar failed"}}), flush=True)
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": "turn_failed", "thread_id": "thread_fake", "status": "failed", "agent_loop_status": "failed"}}}), flush=True)
"#,
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["run", "write tests", "--agent-host", "python"])
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
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import os
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "turn/status":
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": message["params"]["turnId"], "thread_id": "thread_fake", "status": "running", "agent_loop_status": "running"}}}), flush=True)
    elif method == "turn/interrupt":
        print(json.dumps({"id": request_id, "result": {"turnId": message["params"]["turnId"], "status": "interrupted"}}), flush=True)
    elif method == "approval/decision":
        print(json.dumps({"id": request_id, "result": {"decision": message["params"]}}), flush=True)
    elif method == "trace/show":
        print(json.dumps({"id": request_id, "result": {"event": {"event_id": message["params"]["eventId"], "event_type": "trace.event", "run_id": "run_fake", "session_id": "session_fake", "task_id": None, "phase_id": None, "action_id": None, "parent_event_id": None, "timestamp": None, "monotonic_ms": None, "component": "python_sidecar", "severity": "info", "summary": "sidecar trace", "payload": {}, "artifact_refs": [], "policy_decision_id": None, "approval_grant_id": None, "sandbox_id": None, "command_id": None, "transaction_id": None, "verification_id": None, "span_id": None, "redaction_applied": True, "payload_hash": ""}}}), flush=True)
"#,
    );

    let status = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["turn", "status", "turn_fake"])
        .output()
        .expect("turn status cli");
    assert!(status.status.success(), "stderr={}", stderr(&status));
    assert!(stdout(&status).contains("turn turn_fake running agent_loop_status=running"));

    let interrupt = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["turn", "interrupt", "turn_fake"])
        .output()
        .expect("turn interrupt cli");
    assert!(interrupt.status.success(), "stderr={}", stderr(&interrupt));
    assert!(stdout(&interrupt).contains("turn turn_fake interrupted"));

    let approve = cli_with_app_server(path_str(&fake_server), &db_path)
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

    let trace_show = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["trace", "show", "event_fake"])
        .output()
        .expect("trace show cli");
    assert!(
        trace_show.status.success(),
        "stderr={}",
        stderr(&trace_show)
    );
    assert!(stdout(&trace_show).contains("trace event_fake python_sidecar sidecar trace"));
}

#[test]
fn cli_turn_lifecycle_status_and_interrupt_render_agent_loop_status() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import os
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "turn/status":
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": message["params"]["turnId"], "thread_id": "thread_fake", "status": "running", "agent_loop_status": "running"}}}), flush=True)
    elif method == "turn/interrupt":
        print(json.dumps({"id": request_id, "result": {"turnId": message["params"]["turnId"], "status": "interrupted", "agent_loop_status": "cancel_requested"}}), flush=True)
"#,
    );

    let status = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["turn", "status", "turn_fake"])
        .output()
        .expect("turn status cli");
    assert!(status.status.success(), "stderr={}", stderr(&status));
    assert!(stdout(&status).contains("turn turn_fake running agent_loop_status=running"));

    let interrupt = cli_with_app_server(path_str(&fake_server), &db_path)
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
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    request_id = message.get("id")
    method = message.get("method")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "turn/interrupt":
        print(json.dumps({"id": request_id, "error": {"code": -32000, "message": "cancel failed"}}), flush=True)
"#,
    );

    let interrupt = cli_with_app_server(path_str(&fake_server), &db_path)
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
    let fake_server = write_fake_app_server(
        temp.path(),
        &format!(
            r#"
import json
import pathlib
import sys

log_path = pathlib.Path(r"{}")
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({{"id": request_id, "result": {{"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}}}), flush=True)
    elif method == "thread/list":
        print(json.dumps({{"id": request_id, "result": {{"threads": []}}}}), flush=True)
    elif method == "server/shutdown":
        log_path.write_text("shutdown", encoding="utf-8")
        print(json.dumps({{"id": request_id, "result": {{"shutdown": True}}}}), flush=True)
        break
"#,
            shutdown_log.display()
        ),
    );

    let threads = cli_with_app_server(path_str(&fake_server), &db_path)
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
    elif method == "agent/capability":
        print(json.dumps({"id": request_id, "result": {"nativeAgentLoop": {"available": True, "status": "completed", "reason": "enabled", "blockers": []}}}), flush=True)
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
    elif method == "agent/capability":
        print(json.dumps({"id": request_id, "result": {"nativeAgentLoop": {"available": True, "status": "completed", "reason": "enabled", "blockers": []}}}), flush=True)
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
fn cli_run_polls_active_sidecar_turn_before_shutdown() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = write_fake_app_server(
        temp.path(),
        r#"
import json
import sys

status_calls = 0
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "agent/capability":
        print(json.dumps({"id": request_id, "result": {"nativeAgentLoop": {"available": True, "status": "completed", "reason": "enabled", "blockers": []}}}), flush=True)
    elif method == "thread/start":
        print(json.dumps({"id": request_id, "result": {"thread": {"thread_id": "thread_fake", "model": None, "cwd": None, "status": "active"}}}), flush=True)
    elif method == "turn/start":
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": "turn_active", "thread_id": "thread_fake", "status": "running", "agent_loop_status": "running"}}}), flush=True)
    elif method == "turn/status":
        status_calls += 1
        status = "completed" if status_calls >= 2 else "running"
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": "turn_active", "thread_id": "thread_fake", "status": status, "agent_loop_status": status}}}), flush=True)
    elif method == "server/shutdown":
        print(json.dumps({"id": request_id, "result": {"shutdown": True}}), flush=True)
        break
"#,
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["run", "write tests", "--agent-host", "python"])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(stdout(&output).contains("turn turn_active completed agent_loop_status=completed"));
}

#[test]
fn cli_run_polling_exits_nonzero_for_interrupted_turn() {
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
    elif method == "agent/capability":
        print(json.dumps({"id": request_id, "result": {"nativeAgentLoop": {"available": True, "status": "completed", "reason": "enabled", "blockers": []}}}), flush=True)
    elif method == "thread/start":
        print(json.dumps({"id": request_id, "result": {"thread": {"thread_id": "thread_fake", "model": None, "cwd": None, "status": "active"}}}), flush=True)
    elif method == "turn/start":
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": "turn_active", "thread_id": "thread_fake", "status": "running", "agent_loop_status": "running"}}}), flush=True)
    elif method == "turn/status":
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": "turn_active", "thread_id": "thread_fake", "status": "interrupted", "agent_loop_status": "cancelled"}}}), flush=True)
    elif method == "server/shutdown":
        print(json.dumps({"id": request_id, "result": {"shutdown": True}}), flush=True)
        break
"#,
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["run", "write tests", "--agent-host", "python"])
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
    elif method == "agent/capability":
        print(json.dumps({"id": request_id, "result": {"nativeAgentLoop": {"available": True, "status": "completed", "reason": "enabled", "blockers": []}}}), flush=True)
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
    elif method == "agent/capability":
        print(json.dumps({"id": request_id, "result": {"nativeAgentLoop": {"available": True, "status": "completed", "reason": "enabled", "blockers": []}}}), flush=True)
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

fn assert_immediate_terminal_turn_exits_nonzero(status: &str, agent_loop_status: &str) {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let fake_server = write_fake_app_server(
        temp.path(),
        &format!(
            r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({{"id": request_id, "result": {{"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}}}), flush=True)
    elif method == "thread/start":
        print(json.dumps({{"id": request_id, "result": {{"thread": {{"thread_id": "thread_fake", "model": None, "cwd": None, "status": "active"}}}}}}), flush=True)
    elif method == "turn/start":
        print(json.dumps({{"id": request_id, "result": {{"turn": {{"turn_id": "turn_terminal", "thread_id": "thread_fake", "status": "{status}", "agent_loop_status": "{agent_loop_status}"}}}}}}), flush=True)
    elif method == "server/shutdown":
        print(json.dumps({{"id": request_id, "result": {{"shutdown": True}}}}), flush=True)
        break
"#
        ),
    );

    let output = cli_with_app_server(path_str(&fake_server), &db_path)
        .args(["run", "write tests", "--agent-host", "python"])
        .output()
        .expect("run cli");

    assert!(!output.status.success());
    assert!(stdout(&output).contains(&format!(
        "turn turn_terminal {status} agent_loop_status={agent_loop_status}"
    )));
    assert!(stderr(&output).contains(&format!("turn {status}")));
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

fn python_bin() -> String {
    std::env::var("PYTHON").unwrap_or_else(|_| {
        if cfg!(windows) {
            "python".to_string()
        } else {
            "python3".to_string()
        }
    })
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
