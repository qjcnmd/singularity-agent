//! Command request、sandbox result、取消和 backend capability 测试。

use schemars::schema_for;
use singularity_core::CancellationToken;
#[cfg(windows)]
use singularity_sandbox::SandboxNetworkMode;
use singularity_sandbox::{
    CommandExecutionStatus, CommandRequest, CommandResult, CommandScriptRequest,
    CommandSemanticStatus, SandboxBackend, SandboxBackendEnforcement, SandboxCapabilities,
    WindowsSandboxBackend, bound_command_output,
};
use std::path::Path;

const SANDBOX_SRC: &str = include_str!("../src/lib.rs");
const FORBIDDEN_LOCAL_PROCESS_SURFACES: [&str; 11] = [
    "CommandExecutor",
    "PatchExecutor",
    "ProcessManager",
    "run_local",
    "capture_pipe",
    "kill_process_tree",
    "fs::write",
    "fs::remove_file",
    "std::process::Command",
    ".spawn()",
    "taskkill",
];
const FORBIDDEN_RELAXED_SANDBOX_CONTRACTS: [&str; 2] = ["HostWorkspace", "Relaxed"];
#[cfg(windows)]
const CRASH_CALLER_CHILD_ENV: &str = "SINGULARITY_CRASH_CALLER_CHILD";
#[cfg(windows)]
const CRASH_CALLER_WORKSPACE_ENV: &str = "SINGULARITY_CRASH_CALLER_WORKSPACE";

#[test]
fn command_request_and_result_are_schema_backed_boundaries() {
    let request = CommandRequest::project_verification(
        "command_1",
        vec!["cargo".to_string(), "test".to_string()],
        ".",
        "C:/repo",
    );
    let result = CommandResult::completed("command_1", "passed");

    let request_value = serde_json::to_value(&request).expect("serialize command request");
    let result_value = serde_json::to_value(&result).expect("serialize command result");

    assert_eq!(request_value["network"]["mode"], "denied");
    assert_eq!(result_value["semantic_status"], "succeeded");
    assert_eq!(result_value["redacted"], true);
    assert_eq!(request.permission_resource(), "cargo test");
    assert_eq!(
        schema_for!(CommandRequest)
            .schema
            .metadata
            .unwrap()
            .title
            .unwrap(),
        "CommandRequest"
    );
    assert_eq!(
        schema_for!(CommandResult)
            .schema
            .metadata
            .unwrap()
            .title
            .unwrap(),
        "CommandResult"
    );
}

#[test]
fn trusted_workspace_preparation_is_not_serializable_authority() {
    let trusted = CommandRequest::trusted_workspace_preparation(
        "trusted_workspace_preparation",
        vec!["git".to_string(), "init".to_string()],
        ".",
        "C:/repo",
    );

    assert!(trusted.is_trusted_workspace_preparation());
    let value = serde_json::to_value(&trusted).expect("serialize command request");
    assert!(
        value.get("protected_path_enforcement").is_none(),
        "trusted control-plane authority must not enter the command protocol"
    );

    let restored: CommandRequest =
        serde_json::from_value(value).expect("deserialize command request");
    assert!(
        !restored.is_trusted_workspace_preparation(),
        "deserialized requests must restore protected-path enforcement"
    );
    assert!(
        !schema_for!(CommandRequest)
            .schema
            .object
            .expect("command request object schema")
            .properties
            .contains_key("protected_path_enforcement"),
        "trusted control-plane authority must not enter the JSON schema"
    );
}

#[test]
fn command_resource_normalization_belongs_to_command_boundary() {
    let request = CommandRequest::project_verification(
        "command_1",
        vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            "cargo".to_string(),
            "test".to_string(),
        ],
        ".",
        "C:/repo",
    );

    assert_eq!(request.permission_resource(), "cargo test");
}

#[test]
fn command_executor_fails_closed_when_sandbox_is_required_without_backend() {
    let request =
        CommandRequest::project_verification("command_1", vec!["git".to_string()], ".", "C:/repo");
    assert!(request.requires_sandbox());

    let result = CommandResult::sandbox_backend_unavailable(&request.command_id);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::BackendError
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
    assert!(result.stderr_preview.contains("sandbox-required"));
}

#[test]
fn direct_argv_backend_returns_typed_unsupported_for_model_script() {
    let backend = DirectArgvOnlyBackend;
    let request =
        CommandScriptRequest::agent_requested("script_unsupported", "cargo test", ".", "C:/repo");
    let result = backend.execute_script(&request);
    assert_eq!(result.execution_status, CommandExecutionStatus::Unsupported);
    assert_eq!(result.semantic_status, CommandSemanticStatus::Unsupported);

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let result = backend.execute_script_cancellable(&request, &cancellation);
    assert_eq!(result.execution_status, CommandExecutionStatus::Cancelled);
}

#[test]
fn cancelled_command_result_is_distinct_from_timeout() {
    let result = CommandResult::cancelled("command_cancelled", 25);

    assert_eq!(result.execution_status, CommandExecutionStatus::Cancelled);
    assert_eq!(result.semantic_status, CommandSemanticStatus::Cancelled);
    assert_eq!(result.duration_ms, 25);
    assert!(!result.timed_out);
    assert!(result.stderr_preview.contains("cancelled"));
}

#[test]
fn command_boundary_does_not_expose_host_workspace_local_process_executor() {
    for forbidden in FORBIDDEN_LOCAL_PROCESS_SURFACES
        .iter()
        .chain(FORBIDDEN_RELAXED_SANDBOX_CONTRACTS.iter())
    {
        assert!(
            !SANDBOX_SRC.contains(forbidden),
            "forbidden local process surface remains: {forbidden}",
        );
    }
}

#[test]
fn command_policy_denied_result_does_not_require_process_or_backend_execution() {
    let result = CommandResult::policy_denied("command_1", "policy denied command execution");

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::PolicyDenied
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
    assert!(result.stderr_preview.contains("policy denied"));
}

#[test]
fn sandbox_capabilities_report_actual_enforcement_strength() {
    assert!(SandboxCapabilities::strict().supports_command_execution());
    assert_eq!(
        SandboxCapabilities::strict().enforcement(),
        SandboxBackendEnforcement::Strict
    );
    assert!(SandboxCapabilities::restricted_token().supports_command_execution());
    assert_eq!(
        SandboxCapabilities::restricted_token().enforcement(),
        SandboxBackendEnforcement::RestrictedToken
    );
    assert!(!SandboxCapabilities::unavailable().supports_command_execution());
    assert_eq!(
        SandboxCapabilities::unavailable().enforcement(),
        SandboxBackendEnforcement::Unavailable
    );

    let mut portable_strict = SandboxCapabilities::strict();
    portable_strict.restricted_token = false;
    portable_strict.job_object = false;
    assert!(portable_strict.supports_command_execution());
    assert_eq!(
        portable_strict.enforcement(),
        SandboxBackendEnforcement::Strict
    );
}

#[test]
fn command_output_is_a_safe_bounded_payload() {
    let output = bound_command_output("abcdef", 3);
    let redacted_result =
        CommandResult::completed("command_secret", "Authorization: Bearer abc123");
    let bounded_result = CommandResult::completed("command_long", "x".repeat(40_010));
    let blocked_result =
        CommandResult::policy_denied("command_blocked", "raw_prompt provider_response abc123");
    assert_eq!(output.preview, "abc");
    assert!(output.truncated);
    assert_eq!(
        redacted_result.stdout_preview,
        "[redacted sensitive command output]"
    );
    assert!(redacted_result.output_truncated);
    assert_eq!(bounded_result.stdout_preview.chars().count(), 40_000);
    assert!(bounded_result.output_truncated);
    assert_eq!(
        blocked_result.stderr_preview,
        "[redacted sensitive command output]"
    );
    assert_eq!(
        CommandResult::completed(
            "command_key",
            "provider returned sk-abcdefghijklmnopqrstuvwxyz"
        )
        .stdout_preview,
        "[redacted sensitive command output]"
    );
    assert_eq!(
        CommandResult::completed(
            "command_gh",
            "provider returned ghp_abcdefghijklmnopqrstuvwxyz123456"
        )
        .stdout_preview,
        "[redacted sensitive command output]"
    );
    assert_eq!(
        CommandResult::completed(
            "command_jwt",
            "bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signature123"
        )
        .stdout_preview,
        "[redacted sensitive command output]"
    );
    assert_eq!(
        CommandResult::completed(
            "command_tokens",
            "token count is 42 and token budget is 100"
        )
        .stdout_preview,
        "token count is 42 and token budget is 100"
    );
}

#[cfg(windows)]
#[test]
fn windows_backend_reports_strict_maximum_capabilities() {
    let backend = WindowsSandboxBackend::new();

    assert_eq!(backend.name(), "windows");
    assert_eq!(
        backend.capabilities().enforcement(),
        SandboxBackendEnforcement::Strict
    );
    assert!(backend.capabilities().filesystem_isolation);
    assert!(backend.capabilities().network_isolation);
}

#[cfg(windows)]
#[test]
fn windows_backend_denies_sensitive_cwd_before_execution() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sensitive = workspace.path().join(".ssh");
    std::fs::create_dir(&sensitive).expect("sensitive dir");
    let request = CommandRequest::project_verification(
        "command_sensitive_cwd",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo denied".to_string(),
        ],
        path_str(&sensitive),
        path_str(workspace.path()),
    );

    let result = WindowsSandboxBackend::new().execute(&request);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::PolicyDenied
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
    assert_eq!(result.sandbox.backend, "windows");
    assert_eq!(
        result.sandbox.enforcement,
        SandboxBackendEnforcement::Unavailable
    );
}

#[cfg(windows)]
#[test]
fn windows_backend_denies_parent_traversal_before_execution() {
    let workspace = tempfile::tempdir().expect("workspace");
    let request = CommandRequest::project_verification(
        "command_parent_escape",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "type ..\\outside.txt".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );

    let result = WindowsSandboxBackend::new().execute(&request);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::PolicyDenied
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
}

#[cfg(windows)]
#[test]
fn windows_backend_reports_missing_host_tool_as_executable_unavailable() {
    let workspace = tempfile::tempdir().expect("workspace");
    let request = CommandRequest::project_verification(
        "command_missing_tool",
        vec!["singularity-tool-that-does-not-exist".to_string()],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );

    let result = WindowsSandboxBackend::new().execute(&request);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::ExecutableUnavailable
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::Unsupported);
    assert_eq!(
        result.sandbox.enforcement,
        SandboxBackendEnforcement::Strict
    );
    assert_eq!(
        result.stderr_preview,
        "sandbox command executable unavailable: required executable 'singularity-tool-that-does-not-exist' was not found on host PATH"
    );
    assert!(!result.sandbox.local_process_fallback);
}

#[cfg(windows)]
#[test]
#[ignore = "requires first-run Windows UAC sandbox setup"]
fn windows_elevated_backend_executes_network_denied_command() {
    let workspace = if let Some(root) =
        std::env::var_os("SINGULARITY_WINDOWS_SANDBOX_TEST_ROOT").map(std::path::PathBuf::from)
    {
        std::fs::create_dir_all(&root).expect("create live sandbox test root");
        tempfile::Builder::new()
            .prefix("singularity-live-")
            .tempdir_in(root)
            .expect("workspace")
    } else {
        tempfile::tempdir().expect("workspace")
    };
    let mut request = CommandRequest::project_verification(
        "command_elevated",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo windows-elevated-ok".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    request.network.mode = SandboxNetworkMode::Denied;

    let result = WindowsSandboxBackend::new().execute(&request);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::Completed,
        "{result:#?}"
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::Succeeded);
    assert_eq!(result.sandbox.backend, "windows_elevated");
    assert_eq!(
        result.sandbox.enforcement,
        SandboxBackendEnforcement::Strict
    );
    assert!(!result.sandbox.local_process_fallback);
}

#[cfg(windows)]
#[test]
#[ignore = "requires first-run Windows UAC sandbox setup and an additional ACL-authority elevation"]
fn windows_elevated_refreshes_acl_for_sandbox_owned_generated_protected_file() {
    let workspace = if let Some(root) =
        std::env::var_os("SINGULARITY_WINDOWS_SANDBOX_TEST_ROOT").map(std::path::PathBuf::from)
    {
        std::fs::create_dir_all(&root).expect("create live sandbox test root");
        tempfile::Builder::new()
            .prefix("singularity-live-acl-")
            .tempdir_in(root)
            .expect("workspace")
    } else {
        tempfile::tempdir().expect("workspace")
    };
    let backend = WindowsSandboxBackend::new();
    let generated = workspace.path().join("generated.pem");
    let mut create_request = CommandRequest::project_verification(
        "sandbox_owned_protected_create",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo generated>generated.pem".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    create_request.network.mode = SandboxNetworkMode::Denied;
    let create_result = backend.execute(&create_request);
    assert_eq!(
        create_result.execution_status,
        CommandExecutionStatus::Completed,
        "{create_result:#?}"
    );
    assert!(
        generated.exists(),
        "sandbox command must create the protected file"
    );

    let mut read_request = CommandRequest::project_verification(
        "sandbox_owned_protected_read",
        vec![
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            "$ErrorActionPreference='Stop'; try { Get-Content -LiteralPath 'generated.pem' -Raw | Out-File -LiteralPath 'readback.txt'; exit 9 } catch { exit 0 }".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    read_request.network.mode = SandboxNetworkMode::Denied;
    let read_result = backend.execute(&read_request);
    assert_eq!(
        read_result.execution_status,
        CommandExecutionStatus::Completed,
        "ACL refresh must not collapse into a backend-unavailable result: {read_result:#?}"
    );
    assert_eq!(read_result.sandbox.backend, "windows_elevated");
    assert_eq!(
        read_result.sandbox.enforcement,
        SandboxBackendEnforcement::Strict
    );
    assert!(!read_result.sandbox.local_process_fallback);
    assert!(
        !workspace.path().join("readback.txt").exists(),
        "the sandbox identity must remain denied after elevated ACL reconciliation"
    );
}

#[cfg(windows)]
#[test]
#[ignore = "requires first-run Windows UAC sandbox setup"]
fn windows_elevated_deny_read_is_held_for_overlapping_child_lifetimes() {
    let root = std::env::var_os("SINGULARITY_WINDOWS_SANDBOX_TEST_ROOT")
        .map(std::path::PathBuf::from)
        .expect("set SINGULARITY_WINDOWS_SANDBOX_TEST_ROOT outside TEMP");
    std::fs::create_dir_all(&root).expect("create live sandbox test root");
    let workspace_a = tempfile::Builder::new()
        .prefix("singularity-live-a-")
        .tempdir_in(&root)
        .expect("workspace A");
    let workspace_b = tempfile::Builder::new()
        .prefix("singularity-live-b-")
        .tempdir_in(&root)
        .expect("workspace B");
    std::fs::create_dir(workspace_a.path().join(".git")).expect("create protected metadata A");
    std::fs::create_dir(workspace_b.path().join(".git")).expect("create protected metadata B");
    std::fs::write(
        workspace_a.path().join(".git").join("secret.txt"),
        b"secret",
    )
    .expect("write protected secret A");
    std::fs::write(
        workspace_b.path().join(".git").join("secret.txt"),
        b"secret",
    )
    .expect("write protected secret B");

    let workspace_a_path = workspace_a.path().to_path_buf();
    let mut request_a = CommandRequest::project_verification(
        "command_overlap_a",
        vec![
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            "$ErrorActionPreference='Stop'; Set-Content -LiteralPath 'a-ready' -Value 'ready'; while (-not (Test-Path -LiteralPath 'probe')) { Start-Sleep -Milliseconds 50 }; $protected=Join-Path ([string]::Concat([char]46,'git')) ([string]::Concat('sec','ret.txt')); try { Get-Content -LiteralPath $protected -ErrorAction Stop | Out-Null; Set-Content -LiteralPath 'a-outcome' -Value 'readable'; exit 9 } catch { Set-Content -LiteralPath 'a-outcome' -Value 'denied'; exit 0 }".to_string(),
        ],
        path_str(&workspace_a_path),
        path_str(&workspace_a_path),
    );
    request_a.network.mode = SandboxNetworkMode::Denied;
    let (a_tx, a_rx) = std::sync::mpsc::channel();
    let a_handle = std::thread::spawn(move || {
        let result = WindowsSandboxBackend::new().execute(&request_a);
        a_tx.send(result).expect("send workspace A result");
    });
    let a_ready = workspace_a.path().join("a-ready");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !a_ready.exists() {
        if let Ok(result) = a_rx.try_recv() {
            panic!("workspace A exited before its ready marker: {result:#?}");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {}",
            a_ready.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let workspace_b_path = workspace_b.path().to_path_buf();
    let mut request_b = CommandRequest::project_verification(
        "command_overlap_b",
        vec![
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            "Set-Content -LiteralPath 'b-ready' -Value 'ready'".to_string(),
        ],
        path_str(&workspace_b_path),
        path_str(&workspace_b_path),
    );
    request_b.network.mode = SandboxNetworkMode::Denied;
    let b_handle = std::thread::spawn(move || WindowsSandboxBackend::new().execute(&request_b));

    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(
        !workspace_b.path().join("b-ready").exists(),
        "workspace B child started before workspace A released the shared read principal"
    );
    std::fs::write(workspace_a.path().join("probe"), b"probe").expect("release workspace A");

    let result_a = a_rx.recv().expect("receive workspace A result");
    a_handle.join().expect("join workspace A");
    let result_b = b_handle.join().expect("join workspace B");
    assert_eq!(
        result_a.execution_status,
        CommandExecutionStatus::Completed,
        "{result_a:#?}"
    );
    let outcome = std::fs::read_to_string(workspace_a.path().join("a-outcome"))
        .unwrap_or_else(|error| format!("missing:{error}"));
    assert_eq!(
        result_a.semantic_status,
        CommandSemanticStatus::Succeeded,
        "{result_a:#?}; outcome={outcome:?}"
    );
    assert_eq!(result_a.sandbox.backend, "windows_elevated");
    assert_eq!(
        result_a.sandbox.enforcement,
        SandboxBackendEnforcement::Strict
    );
    assert!(!result_a.sandbox.local_process_fallback);
    assert_eq!(outcome.trim(), "denied");
    assert_eq!(
        result_b.execution_status,
        CommandExecutionStatus::Completed,
        "{result_b:#?}"
    );
    assert_eq!(result_b.semantic_status, CommandSemanticStatus::Succeeded);
    assert_eq!(result_b.sandbox.backend, "windows_elevated");
    assert_eq!(
        result_b.sandbox.enforcement,
        SandboxBackendEnforcement::Strict
    );
    assert!(!result_b.sandbox.local_process_fallback);
    assert!(workspace_b.path().join("b-ready").exists());
}

#[cfg(windows)]
#[test]
#[ignore = "requires first-run Windows UAC sandbox setup"]
fn windows_elevated_runner_lease_survives_parent_crash() {
    if std::env::var_os(CRASH_CALLER_CHILD_ENV).is_some() {
        let workspace = std::path::PathBuf::from(
            std::env::var_os(CRASH_CALLER_WORKSPACE_ENV).expect("crash caller workspace"),
        );
        let mut request = CommandRequest::project_verification(
            "command_crash_parent_a",
            vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                "$ErrorActionPreference='Stop'; Set-Content -LiteralPath 'a-ready' -Value 'ready'; $protected=Join-Path ([string]::Concat([char]46,'git')) ([string]::Concat('sec','ret.txt')); while ($true) { try { Get-Content -LiteralPath $protected -ErrorAction Stop | Out-Null; Set-Content -LiteralPath 'escaped' -Value 'readable'; exit 9 } catch { Start-Sleep -Milliseconds 10 } }".to_string(),
            ],
            path_str(&workspace),
            path_str(&workspace),
        );
        request.network.mode = SandboxNetworkMode::Denied;
        let result = WindowsSandboxBackend::new().execute(&request);
        panic!("crash-caller child returned before its parent was terminated: {result:#?}");
    }

    let root = std::env::var_os("SINGULARITY_WINDOWS_SANDBOX_TEST_ROOT")
        .map(std::path::PathBuf::from)
        .expect("set SINGULARITY_WINDOWS_SANDBOX_TEST_ROOT outside TEMP");
    std::fs::create_dir_all(&root).expect("create live sandbox test root");
    let workspace_a = tempfile::Builder::new()
        .prefix("singularity-crash-a-")
        .tempdir_in(&root)
        .expect("workspace A");
    let workspace_b = tempfile::Builder::new()
        .prefix("singularity-crash-b-")
        .tempdir_in(&root)
        .expect("workspace B");
    for workspace in [workspace_a.path(), workspace_b.path()] {
        std::fs::create_dir(workspace.join(".git")).expect("create protected metadata");
        std::fs::write(workspace.join(".git").join("secret.txt"), b"secret")
            .expect("write protected secret");
    }

    let executable = std::env::current_exe().expect("current test executable");
    let mut caller = std::process::Command::new(executable)
        .args([
            "--exact",
            "windows_elevated_runner_lease_survives_parent_crash",
            "--ignored",
            "--nocapture",
        ])
        .env(CRASH_CALLER_CHILD_ENV, "1")
        .env(CRASH_CALLER_WORKSPACE_ENV, workspace_a.path())
        .spawn()
        .expect("spawn crash-caller test process");
    let ready = workspace_a.path().join("a-ready");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !ready.exists() {
        assert!(
            caller.try_wait().expect("poll crash caller").is_none(),
            "crash caller exited before the sandbox child became ready"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {}",
            ready.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    caller.kill().expect("terminate sandbox caller process");
    let caller_status = caller.wait().expect("wait terminated sandbox caller");
    assert!(
        !caller_status.success(),
        "crash caller must be terminated abruptly"
    );

    let mut request_b = CommandRequest::project_verification(
        "command_crash_parent_b",
        vec![
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            "Set-Content -LiteralPath 'b-ready' -Value 'ready'".to_string(),
        ],
        path_str(workspace_b.path()),
        path_str(workspace_b.path()),
    );
    request_b.network.mode = SandboxNetworkMode::Denied;
    let result_b = WindowsSandboxBackend::new().execute(&request_b);
    assert_eq!(
        result_b.execution_status,
        CommandExecutionStatus::Completed,
        "{result_b:#?}"
    );
    assert_eq!(result_b.semantic_status, CommandSemanticStatus::Succeeded);
    assert_eq!(result_b.sandbox.backend, "windows_elevated");
    assert_eq!(
        result_b.sandbox.enforcement,
        SandboxBackendEnforcement::Strict
    );
    assert!(!result_b.sandbox.local_process_fallback);
    assert!(workspace_b.path().join("b-ready").exists());
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        !workspace_a.path().join("escaped").exists(),
        "the crashed caller's child outlived Job cleanup and observed revoked protection"
    );
}

#[cfg(not(windows))]
#[test]
fn windows_backend_is_unavailable_off_windows() {
    let workspace = tempfile::tempdir().expect("workspace");
    let request = CommandRequest::project_verification(
        "command_echo",
        vec!["echo".to_string(), "ok".to_string()],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    let backend = WindowsSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(backend.name(), "windows");
    assert!(!backend.capabilities().supports_command_execution());
    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::BackendError
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

struct DirectArgvOnlyBackend;

impl SandboxBackend for DirectArgvOnlyBackend {
    fn name(&self) -> &'static str {
        "direct_argv_only"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, "direct argv")
    }
}
