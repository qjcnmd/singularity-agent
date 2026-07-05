use assert_cmd::Command;

#[test]
fn cli_exposes_app_server_protocol_mode_without_direct_core_runtime() {
    let mut command = Command::cargo_bin("singularity-rs").expect("binary");
    command.arg("--help").assert().success();
}

#[test]
fn cli_outputs_json_rpc_initialize_request() {
    let mut command = Command::cargo_bin("singularity-rs").expect("binary");
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
    let mut command = Command::cargo_bin("singularity-rs").expect("binary");
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
