//! Command request、sandbox result、取消和 backend capability 测试。

use schemars::schema_for;
use singularity_core::CancellationToken;
#[cfg(windows)]
use singularity_sandbox::SandboxNetworkMode;
#[cfg(windows)]
use singularity_sandbox::WorkspaceMutation;
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
    assert!(backend.capabilities().change_detection);
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
    assert_eq!(result.workspace_mutation, WorkspaceMutation::Unchanged);
    assert!(!result.sandbox.local_process_fallback);

    let changed_request = CommandRequest::project_verification(
        "command_elevated_changed",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo changed>changed.txt".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    let changed_result = WindowsSandboxBackend::new().execute(&changed_request);
    assert_eq!(
        changed_result.execution_status,
        CommandExecutionStatus::Completed,
        "{changed_result:#?}"
    );
    assert_eq!(
        changed_result.workspace_mutation,
        WorkspaceMutation::Changed
    );
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
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            // Construct the protected suffix at runtime so this fault injection reaches the
            // post-execution ACL reconciliation below instead of stopping at lexical admission.
            "$protected=[string]::Concat('generated',[char]46,'pem'); Set-Content -LiteralPath $protected -Value 'generated'".to_string(),
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
    assert_eq!(create_result.workspace_mutation, WorkspaceMutation::Changed);
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
            // Only the expected ACL denial passes. A successful read or any unrelated failure
            // exits non-zero, so marker creation cannot introduce another false positive.
            "$ErrorActionPreference='Stop'; $protected=[string]::Concat('generated',[char]46,'pem'); try { $null=Get-Content -LiteralPath $protected -Raw } catch [System.UnauthorizedAccessException] { exit 0 }; exit 9".to_string(),
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
    assert_eq!(read_result.exit_code, Some(0), "{read_result:#?}");
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

#[cfg(target_os = "linux")]
mod linux_tests {
    use super::*;
    use singularity_core::CancellationToken;
    use singularity_sandbox::{
        LinuxCapability, LinuxSandboxBackend, LinuxSandboxProbe, SandboxFilesystemMode,
        SandboxNetworkMode, WorkspaceMutation, probe_linux_capabilities,
    };
    use std::ffi::OsString;
    use std::fs::{self, hard_link};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use std::sync::{Mutex, MutexGuard};
    use std::thread;
    use std::time::{Duration, Instant};

    static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());

    fn path_str(path: &Path) -> &str {
        path.to_str().expect("utf8 path")
    }

    fn make_executable(path: &Path) {
        let mut permissions = fs::metadata(path)
            .expect("executable metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("mark executable");
    }

    fn strict_backend() -> LinuxSandboxBackend {
        let backend = LinuxSandboxBackend::new();
        assert!(
            backend.probe().strict_ready(),
            "strict Linux capability probe failed: {:?}",
            backend.probe()
        );
        backend
    }

    fn request(
        id: &str,
        argv: &[&str],
        workspace: &Path,
        filesystem: SandboxFilesystemMode,
        network: SandboxNetworkMode,
    ) -> CommandRequest {
        let mut request = CommandRequest::project_verification(
            id,
            argv.iter().map(|value| (*value).to_string()).collect(),
            path_str(workspace),
            path_str(workspace),
        );
        request.filesystem.mode = filesystem;
        request.network.mode = network;
        request
    }

    struct EnvironmentGuard {
        name: &'static str,
        previous: Option<OsString>,
    }

    fn lock_environment() -> MutexGuard<'static, ()> {
        ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    impl EnvironmentGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(name);
            unsafe { std::env::set_var(name, value) };
            Self { name, previous }
        }

        fn remove(name: &'static str) -> Self {
            let previous = std::env::var_os(name);
            unsafe { std::env::remove_var(name) };
            Self { name, previous }
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(value) = self.previous.take() {
                    std::env::set_var(self.name, value);
                } else {
                    std::env::remove_var(self.name);
                }
            }
        }
    }

    fn delayed_orphan_script(delay_seconds: u64) -> String {
        format!(
            r#"
import os
import time

child = os.fork()
if child == 0:
    try:
        os.setpgid(0, 0)
    except OSError:
        with open("orphan-setup-failed", "w") as marker:
            marker.write("setpgid failed")
        os._exit(91)
    with open("orphan-ready", "w") as marker:
        marker.write("ready")
    time.sleep({delay_seconds})
    with open("orphan-marker", "w") as marker:
        marker.write("late")
    os._exit(0)

while not os.path.exists("orphan-ready"):
    time.sleep(0.01)
time.sleep(30)
"#
        )
    }

    fn wait_for_file(path: &Path, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if path.is_file() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        path.is_file()
    }

    fn assert_no_delayed_side_effect(workspace: &Path) {
        let marker = workspace.join("orphan-marker");
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            assert!(!marker.exists(), "orphan child produced a delayed marker");
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!marker.exists(), "orphan child produced a delayed marker");
        assert!(
            !workspace.join("orphan-setup-failed").exists(),
            "the orphan process did not establish its independent process group"
        );
    }

    #[test]
    fn linux_probe_reports_kernel_controls_without_os_handles() {
        let probe: LinuxSandboxProbe = probe_linux_capabilities();
        assert!(probe.user_namespace);
        assert!(probe.pid_namespace);
        assert!(probe.mount_namespace);
        assert!(probe.network_namespace);
        assert!(probe.no_new_privs);
        assert!(probe.seccomp);
        assert!(probe.landlock_abi.is_some_and(|abi| abi >= 3));
        assert!(probe.process_tree_cleanup);
        assert!(probe.cgroup_v2);
        assert!(!probe.cgroup_delegated);
    }

    #[test]
    fn linux_strict_mode_requires_pid_namespace_capability() {
        let mut probe = probe_linux_capabilities();
        probe.pid_namespace = false;
        assert!(!probe.strict_ready());
        assert_eq!(
            probe.missing_capability(),
            Some(LinuxCapability::PidNamespace)
        );
    }

    #[test]
    fn linux_nonstandard_executable_does_not_authorize_its_sibling_secret() {
        let workspace = tempfile::tempdir().expect("workspace");
        let runtime = tempfile::tempdir().expect("runtime");
        let bin = runtime.path().join("bin");
        fs::create_dir(&bin).expect("runtime bin");
        let secret = runtime.path().join("secret.txt");
        fs::write(&secret, "outside-secret").expect("runtime sibling secret");
        let executable = bin.join("runner");
        assert!(!path_str(&secret).contains('\''));
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nif /bin/cat '{}' >/dev/null 2>&1; then exit 41; fi\nprintf runtime-sibling-denied\n",
                path_str(&secret)
            ),
        )
        .expect("runtime executable");
        make_executable(&executable);

        let result = strict_backend().execute(&request(
            "linux_nonstandard_executable_scope",
            &[path_str(&executable)],
            workspace.path(),
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
        ));

        assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout_preview.contains("runtime-sibling-denied"));
        assert!(!result.stdout_preview.contains("outside-secret"));
    }

    #[test]
    fn linux_python_venv_preserves_invocation_identity() {
        let workspace = tempfile::tempdir().expect("workspace");
        let venv = tempfile::tempdir().expect("venv parent");
        let venv_root = venv.path().join("environment");
        let setup = Command::new("/usr/bin/python3")
            .args(["-m", "venv", path_str(&venv_root)])
            .status()
            .expect("create Python venv");
        assert!(setup.success(), "Python venv setup failed: {setup}");
        let python = venv_root.join("bin/python");
        let expected = format!("{}|{}", path_str(&venv_root), path_str(&python));

        let result = strict_backend().execute(&request(
            "linux_python_venv_identity",
            &[
                path_str(&python),
                "-c",
                "import sys; print(f'{sys.prefix}|{sys.executable}')",
            ],
            workspace.path(),
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
        ));

        assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
        assert_eq!(result.exit_code, Some(0), "{}", result.stderr_preview);
        assert_eq!(result.stdout_preview.trim(), expected);
    }

    #[test]
    fn linux_env_shebang_resolves_nonstandard_interpreter_from_path() {
        let _environment = lock_environment();
        let workspace = tempfile::tempdir().expect("workspace");
        let interpreter_home = tempfile::tempdir().expect("interpreter home");
        let interpreter_bin = interpreter_home.path().join("bin");
        fs::create_dir(&interpreter_bin).expect("interpreter bin");
        let interpreter = interpreter_bin.join("sandbox-python");
        fs::copy("/usr/bin/python3", &interpreter).expect("copy Python interpreter");
        make_executable(&interpreter);
        let script_home = tempfile::tempdir().expect("script home");
        let script = script_home.path().join("entrypoint");
        fs::write(
            &script,
            "#!/usr/bin/env sandbox-python\nprint('env-shebang-ok')\n",
        )
        .expect("env script");
        make_executable(&script);
        let host_path = std::env::var("PATH").unwrap_or_default();
        let _path = EnvironmentGuard::set(
            "PATH",
            &format!("{}:{host_path}", path_str(&interpreter_bin)),
        );

        let result = strict_backend().execute(&request(
            "linux_env_shebang",
            &[path_str(&script)],
            workspace.path(),
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
        ));

        assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
        assert_eq!(result.exit_code, Some(0), "{}", result.stderr_preview);
        assert!(result.stdout_preview.contains("env-shebang-ok"));
    }

    #[test]
    fn linux_rustup_proxy_runs_real_cargo_toolchain() {
        let _environment = lock_environment();
        let workspace = tempfile::tempdir().expect("workspace");
        let _target_dir = EnvironmentGuard::remove("CARGO_TARGET_DIR");
        fs::create_dir(workspace.path().join("src")).expect("src");
        fs::write(
            workspace.path().join("Cargo.toml"),
            "[package]\nname = \"sandbox-smoke\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .expect("Cargo.toml");
        fs::write(workspace.path().join("src/main.rs"), "fn main() {}\n").expect("main.rs");

        let result = strict_backend().execute(&request(
            "linux_rustup_cargo",
            &["cargo", "check", "--offline", "--quiet"],
            workspace.path(),
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Denied,
        ));

        assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
        assert_eq!(result.exit_code, Some(0), "{}", result.stderr_preview);
        assert!(workspace.path().join("target/debug").is_dir());
    }

    #[test]
    fn linux_rustup_proxy_honors_custom_toolchain_homes() {
        use std::os::unix::fs::symlink;
        use std::path::PathBuf;

        let _environment = lock_environment();
        let workspace = tempfile::tempdir().expect("workspace");
        let toolchain_layout = tempfile::tempdir().expect("toolchain layout");
        let cargo_home = toolchain_layout.path().join("cargo-home");
        let cargo_bin = cargo_home.join("bin");
        let rustup_home = toolchain_layout.path().join("rustup-home");
        fs::create_dir_all(&cargo_bin).expect("custom cargo bin");

        let host_cargo = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join("cargo"))
            .find(|candidate| candidate.is_file())
            .expect("host cargo proxy");
        let host_rustup = fs::canonicalize(&host_cargo).expect("canonical rustup proxy");
        assert_eq!(
            host_rustup.file_name().and_then(|name| name.to_str()),
            Some("rustup")
        );
        let host_rustup_home = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".rustup"))
            })
            .expect("host rustup home");
        symlink(&host_rustup, cargo_bin.join("cargo")).expect("custom cargo proxy");
        symlink(&host_rustup_home, &rustup_home).expect("custom rustup home");

        fs::create_dir(workspace.path().join("src")).expect("src");
        fs::write(
            workspace.path().join("Cargo.toml"),
            "[package]\nname = \"custom-toolchain-home-smoke\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .expect("Cargo.toml");
        fs::write(workspace.path().join("src/main.rs"), "fn main() {}\n").expect("main.rs");

        let host_path = std::env::var("PATH").unwrap_or_default();
        let _path = EnvironmentGuard::set("PATH", &format!("{}:{host_path}", path_str(&cargo_bin)));
        let _cargo_home = EnvironmentGuard::set("CARGO_HOME", path_str(&cargo_home));
        let _rustup_home = EnvironmentGuard::set("RUSTUP_HOME", path_str(&rustup_home));
        let _target_dir = EnvironmentGuard::remove("CARGO_TARGET_DIR");

        let result = strict_backend().execute(&request(
            "linux_custom_rustup_homes",
            &["cargo", "check", "--offline", "--quiet"],
            workspace.path(),
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Denied,
        ));

        assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
        assert_eq!(result.exit_code, Some(0), "{}", result.stderr_preview);
        assert!(workspace.path().join("target/debug").is_dir());
    }

    #[test]
    fn linux_nonstandard_node_binary_runs_without_authorizing_siblings() {
        let workspace = tempfile::tempdir().expect("workspace");
        let runtime = tempfile::tempdir().expect("runtime");
        let bin = runtime.path().join("bin");
        fs::create_dir(&bin).expect("runtime bin");
        let node = bin.join("node");
        fs::copy("/usr/bin/node", &node).expect("copy node");
        make_executable(&node);
        let secret = runtime.path().join("secret.txt");
        fs::write(&secret, "outside-secret").expect("runtime sibling secret");
        assert!(!path_str(&secret).contains('\''));
        let script = format!(
            "const fs=require('fs');try{{fs.readFileSync('{}');process.exit(41)}}catch(_){{console.log('node-sibling-denied')}}",
            path_str(&secret)
        );

        let result = strict_backend().execute(&request(
            "linux_nonstandard_node",
            &[path_str(&node), "-e", &script],
            workspace.path(),
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
        ));

        assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
        assert_eq!(result.exit_code, Some(0), "{}", result.stderr_preview);
        assert!(result.stdout_preview.contains("node-sibling-denied"));
    }

    #[test]
    fn linux_system_node_and_npm_toolchain_execute() {
        let workspace = tempfile::tempdir().expect("workspace");
        let backend = strict_backend();
        let node = backend.execute(&request(
            "linux_system_node",
            &["node", "-e", "console.log('node-ok')"],
            workspace.path(),
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
        ));
        assert_eq!(node.execution_status, CommandExecutionStatus::Completed);
        assert_eq!(node.exit_code, Some(0), "{}", node.stderr_preview);
        assert!(node.stdout_preview.contains("node-ok"));

        let npm = backend.execute(&request(
            "linux_system_npm",
            &["npm", "--version"],
            workspace.path(),
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
        ));
        assert_eq!(npm.execution_status, CommandExecutionStatus::Completed);
        assert_eq!(npm.exit_code, Some(0), "{}", npm.stderr_preview);
        assert!(!npm.stdout_preview.trim().is_empty());
    }

    #[test]
    fn linux_workspace_write_is_enforced_and_observed() {
        let workspace = tempfile::tempdir().expect("workspace");
        let backend = strict_backend();
        let request = request(
            "linux_workspace_write",
            &["/bin/sh", "-c", "printf changed > output.txt"],
            workspace.path(),
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Denied,
        );

        let result = backend.execute(&request);
        assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.workspace_mutation, WorkspaceMutation::Changed);
        assert_eq!(
            fs::read_to_string(workspace.path().join("output.txt")).unwrap(),
            "changed"
        );
    }

    #[test]
    fn linux_external_hardlink_is_rejected_before_execution() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        fs::write(outside.path().join("outside.txt"), "outside").expect("outside file");
        hard_link(
            outside.path().join("outside.txt"),
            workspace.path().join("linked.txt"),
        )
        .expect("external hardlink");
        let backend = strict_backend();
        let request = request(
            "linux_external_hardlink",
            &[
                "/bin/sh",
                "-c",
                "printf changed > linked.txt; printf ran > executed.txt",
            ],
            workspace.path(),
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Denied,
        );

        let result = backend.execute(&request);
        assert_eq!(
            result.execution_status,
            CommandExecutionStatus::PolicyDenied
        );
        assert_eq!(result.exit_code, None);
        assert_eq!(result.workspace_mutation, WorkspaceMutation::Unknown);
        assert!(
            result
                .stderr_preview
                .contains("workspace hardlink safety check failed")
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("outside.txt")).unwrap(),
            "outside"
        );
        assert!(!workspace.path().join("executed.txt").exists());
    }

    #[test]
    fn linux_internal_hardlinks_are_allowed() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::write(workspace.path().join("first.txt"), "original").expect("first file");
        hard_link(
            workspace.path().join("first.txt"),
            workspace.path().join("second.txt"),
        )
        .expect("internal hardlink");
        let backend = strict_backend();
        let request = request(
            "linux_internal_hardlink",
            &["/bin/sh", "-c", "printf changed > second.txt"],
            workspace.path(),
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Denied,
        );

        let result = backend.execute(&request);
        assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.workspace_mutation, WorkspaceMutation::Changed);
        assert_eq!(
            fs::read_to_string(workspace.path().join("first.txt")).unwrap(),
            "changed"
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("second.txt")).unwrap(),
            "changed"
        );
    }

    #[test]
    fn linux_read_only_and_protected_mounts_reject_writes() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::write(workspace.path().join("existing.txt"), "original").expect("existing file");
        fs::write(workspace.path().join(".env"), "opaque").expect("protected file");
        let backend = strict_backend();

        let readonly = request(
            "linux_read_only",
            &["/bin/sh", "-c", "printf changed > existing.txt"],
            workspace.path(),
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
        );
        let readonly_result = backend.execute(&readonly);
        assert_eq!(
            readonly_result.execution_status,
            CommandExecutionStatus::Completed
        );
        assert_ne!(readonly_result.exit_code, Some(0));
        assert_eq!(
            fs::read_to_string(workspace.path().join("existing.txt")).unwrap(),
            "original"
        );

        let protected_read = request(
            "linux_protected_read",
            &["/bin/cat", ".env"],
            workspace.path(),
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
        );
        let protected_read_result = backend.execute(&protected_read);
        assert_eq!(
            protected_read_result.execution_status,
            CommandExecutionStatus::Completed
        );
        assert_ne!(protected_read_result.exit_code, Some(0));

        let protected = request(
            "linux_protected_rename",
            &["/bin/sh", "-c", "mv .env renamed.env"],
            workspace.path(),
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Denied,
        );
        let protected_result = backend.execute(&protected);
        assert_eq!(
            protected_result.execution_status,
            CommandExecutionStatus::Completed
        );
        assert_ne!(protected_result.exit_code, Some(0));
        assert!(workspace.path().join(".env").exists());
        assert!(!workspace.path().join("renamed.env").exists());
    }

    #[test]
    fn linux_path_traversal_and_symlink_escape_are_denied_by_landlock() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        fs::write(outside.path().join("outside.txt"), "outside").expect("outside file");
        symlink(outside.path(), workspace.path().join("outside-link")).expect("symlink");
        let backend = strict_backend();

        for (id, path) in [
            (
                "linux_path_traversal",
                format!("{}/../outside.txt", path_str(workspace.path())),
            ),
            (
                "linux_symlink_escape",
                format!("{}/outside-link/outside.txt", path_str(workspace.path())),
            ),
        ] {
            let request = request(
                id,
                &["/bin/cat", &path],
                workspace.path(),
                SandboxFilesystemMode::ReadOnly,
                SandboxNetworkMode::Denied,
            );
            let result = backend.execute(&request);
            assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
            assert_ne!(
                result.exit_code,
                Some(0),
                "escape unexpectedly succeeded: {id}"
            );
        }
    }

    #[test]
    fn linux_network_seccomp_denies_and_allows_socket_creation() {
        let python = Path::new("/usr/bin/python3");
        if !python.is_file() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let backend = strict_backend();
        let script = "import socket; socket.socket().close()";

        let denied = request(
            "linux_network_denied",
            &[path_str(python), "-c", script],
            workspace.path(),
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
        );
        let denied_result = backend.execute(&denied);
        assert_eq!(
            denied_result.execution_status,
            CommandExecutionStatus::Completed
        );
        assert_ne!(denied_result.exit_code, Some(0));

        let allowed = request(
            "linux_network_allowed",
            &[path_str(python), "-c", script],
            workspace.path(),
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Allowed,
        );
        let allowed_result = backend.execute(&allowed);
        assert_eq!(
            allowed_result.execution_status,
            CommandExecutionStatus::Completed
        );
        assert_eq!(allowed_result.exit_code, Some(0));
    }

    #[test]
    fn linux_child_inherits_secret_isolation_and_kernel_restrictions() {
        let _environment = lock_environment();
        let python = Path::new("/usr/bin/python3");
        assert!(python.is_file(), "WSL test requires /usr/bin/python3");
        assert!(
            Path::new("/bin/sh").is_file(),
            "strict shell is unavailable"
        );
        assert!(Path::new("/bin/cat").is_file(), "strict cat is unavailable");

        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let outside_file = outside.path().join("outside.txt");
        fs::write(&outside_file, "outside").expect("outside file");
        let outside_file = path_str(&outside_file);
        assert!(
            !outside_file.contains('\''),
            "temporary path unexpectedly contains a shell quote"
        );
        let _secret = EnvironmentGuard::set(
            "SINGULARITY_LINUX_SANDBOX_SECRET_TOKEN",
            "synthetic-linux-secret-sentinel",
        );
        let backend = strict_backend();
        let script = format!(
            r#"
set -eu
test "$$" -eq 1
test -z "${{SINGULARITY_LINUX_SANDBOX_SECRET_TOKEN-}}"
/bin/sh -c 'test "$$" -gt 1 && test -z "${{SINGULARITY_LINUX_SANDBOX_SECRET_TOKEN-}}"'
if /usr/bin/python3 -c 'import socket; socket.socket().close()'; then exit 41; fi
if /bin/sh -c '/usr/bin/python3 -c "import socket; socket.socket().close()"'; then exit 42; fi
if /bin/cat '{outside_file}' >/dev/null 2>&1; then exit 43; fi
if /bin/sh -c '/bin/cat "{outside_file}" >/dev/null 2>&1'; then exit 44; fi
printf child-contract-ok
"#
        );
        let result = backend.execute(&request(
            "linux_child_contract",
            &["/bin/sh", "-c", &script],
            workspace.path(),
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Denied,
        ));

        assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.sandbox.backend, "linux");
        assert_eq!(
            result.sandbox.enforcement,
            SandboxBackendEnforcement::Strict
        );
        assert!(result.stdout_preview.contains("child-contract-ok"));
        assert!(
            !result
                .stdout_preview
                .contains("synthetic-linux-secret-sentinel")
        );
        assert!(
            !result
                .stderr_preview
                .contains("synthetic-linux-secret-sentinel")
        );
    }

    #[test]
    fn linux_timeout_and_cancellation_remove_orphaned_process_tree_side_effects() {
        let python = Path::new("/usr/bin/python3");
        assert!(python.is_file(), "WSL test requires /usr/bin/python3");
        let backend = strict_backend();

        let timeout_workspace = tempfile::tempdir().expect("timeout workspace");
        let timeout_script = delayed_orphan_script(5);
        let mut timeout_request = request(
            "linux_orphan_timeout",
            &["/usr/bin/python3", "-c", &timeout_script],
            timeout_workspace.path(),
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Denied,
        );
        timeout_request.timeout_seconds = 3;
        let timeout_result = backend.execute(&timeout_request);
        assert_eq!(
            timeout_result.execution_status,
            CommandExecutionStatus::TimedOut
        );
        assert!(wait_for_file(
            &timeout_workspace.path().join("orphan-ready"),
            Duration::from_secs(3)
        ));
        assert_no_delayed_side_effect(timeout_workspace.path());

        let cancel_workspace = tempfile::tempdir().expect("cancel workspace");
        let cancel_script = delayed_orphan_script(2);
        let cancellation = CancellationToken::new();
        let cancel_request = request(
            "linux_orphan_cancel",
            &["/usr/bin/python3", "-c", &cancel_script],
            cancel_workspace.path(),
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Denied,
        );
        let worker_backend = backend.clone();
        let worker_cancellation = cancellation.clone();
        let worker = thread::spawn(move || {
            worker_backend.execute_cancellable(&cancel_request, &worker_cancellation)
        });
        let ready = wait_for_file(
            &cancel_workspace.path().join("orphan-ready"),
            Duration::from_secs(1),
        );
        cancellation.cancel();
        let cancelled = worker.join().expect("cancelled orphan command worker");
        assert!(ready, "cancelled command did not start its derived child");
        assert_eq!(
            cancelled.execution_status,
            CommandExecutionStatus::Cancelled
        );
        assert_no_delayed_side_effect(cancel_workspace.path());
    }

    #[test]
    fn linux_normal_exit_removes_orphaned_process_tree_side_effects() {
        let workspace = tempfile::tempdir().expect("workspace");
        let backend = strict_backend();
        let result = backend.execute(&request(
            "linux_orphan_normal_exit",
            &[
                "/bin/sh",
                "-c",
                "printf parent-ok; (printf ready > orphan-ready; sleep 2; printf late > orphan-marker) & while [ ! -f orphan-ready ]; do sleep 0.01; done; exit 0",
            ],
            workspace.path(),
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Denied,
        ));

        assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout_preview.contains("parent-ok"));
        assert!(workspace.path().join("orphan-ready").is_file());
        assert_no_delayed_side_effect(workspace.path());
    }

    #[test]
    fn linux_timeout_and_cancellation_kill_the_process_group() {
        let workspace = tempfile::tempdir().expect("workspace");
        let backend = strict_backend();
        let mut timeout_request = request(
            "linux_timeout",
            &["/bin/sh", "-c", "sleep 30 & wait"],
            workspace.path(),
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
        );
        timeout_request.timeout_seconds = 1;
        let started = Instant::now();
        let timeout_result = backend.execute(&timeout_request);
        assert_eq!(
            timeout_result.execution_status,
            CommandExecutionStatus::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(5));

        let cancellation = CancellationToken::new();
        let cancel_request = request(
            "linux_cancel",
            &["/bin/sh", "-c", "sleep 30 & wait"],
            workspace.path(),
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
        );
        let worker_backend = backend.clone();
        let worker_cancellation = cancellation.clone();
        let worker = thread::spawn(move || {
            worker_backend.execute_cancellable(&cancel_request, &worker_cancellation)
        });
        thread::sleep(Duration::from_millis(100));
        cancellation.cancel();
        let cancelled = worker.join().expect("cancelled command worker");
        assert_eq!(
            cancelled.execution_status,
            CommandExecutionStatus::Cancelled
        );
    }
}
