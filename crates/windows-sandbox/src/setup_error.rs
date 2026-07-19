use crate::acl::AclOperation;
use crate::acl::WindowsAclError;
use crate::read_acl_mutex::ReadAclMutexError;
use crate::string_utils::sanitize_metric_tag_value;
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;

/// These represent the most common failures for the elevated sandbox setup.
///
/// Codes are used as metric tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupErrorCode {
    // Orchestrator (run in CLI) failures.
    /// Failed to create `sandbox_home/.sandbox` in the orchestrator.
    OrchestratorSandboxDirCreateFailed,
    /// Failed to determine whether the current process is elevated.
    OrchestratorElevationCheckFailed,
    /// The setup command requires an already elevated process.
    OrchestratorElevationRequired,
    /// Failed to serialize the elevation payload before launching the helper.
    OrchestratorPayloadSerializeFailed,
    /// Failed to launch the setup helper process (spawn or ShellExecuteExW).
    OrchestratorHelperLaunchFailed,
    /// User canceled the UAC prompt while launching the helper.
    OrchestratorHelperLaunchCanceled,
    /// Helper exited non-zero and no structured report was available.
    OrchestratorHelperExitNonzero,
    /// Helper exited non-zero and reading `setup_error.json` failed.
    OrchestratorHelperReportReadFailed,
    /// Helper exited successfully before setup completed.
    OrchestratorHelperIncomplete,
    // Helper (elevated process) failures.
    /// Helper failed while validating or decoding the request payload.
    HelperRequestArgsFailed,
    /// Helper failed to create `sandbox_home/.sandbox`.
    HelperSandboxDirCreateFailed,
    /// Helper failed to open or write the setup log.
    HelperLogFailed,
    /// Helper failed in the provisioning phase (fallback bucket).
    HelperUserProvisionFailed,
    /// Helper failed to create the sandbox users local group.
    HelperUsersGroupCreateFailed,
    /// Helper failed to create or update a sandbox user account.
    HelperUserCreateOrUpdateFailed,
    /// Helper failed to protect user passwords with DPAPI.
    HelperDpapiProtectFailed,
    /// Helper failed to write the sandbox users secrets file.
    HelperUsersFileWriteFailed,
    /// Helper failed to write or protect the setup marker file.
    HelperSetupMarkerWriteFailed,
    /// Helper failed to resolve a SID or convert it to a PSID.
    HelperSidResolveFailed,
    /// Helper failed to load or convert capability SIDs.
    HelperCapabilitySidFailed,
    /// Helper failed to initialize COM for firewall configuration.
    HelperFirewallComInitFailed,
    /// Helper failed to access firewall policy or rule collections.
    HelperFirewallPolicyAccessFailed,
    /// Helper detected that local firewall policy changes will not fully take effect.
    HelperFirewallPolicyIneffective,
    /// Helper failed to create, update, or add the firewall rule.
    HelperFirewallRuleCreateOrAddFailed,
    /// Helper failed to verify the configured firewall rule scope.
    HelperFirewallRuleVerifyFailed,
    /// Helper failed to spawn the read-ACL helper process.
    HelperReadAclHelperSpawnFailed,
    /// Helper could not determine the read-ACL helper coordination state.
    HelperReadAclMutexProbeFailed,
    /// Helper failed to reconcile deny-read ACLs.
    HelperDenyReadAclFailed,
    /// Helper failed to lock down sandbox directories via ACLs.
    HelperSandboxLockFailed,
    /// Helper failed for an unmapped or unexpected reason.
    HelperUnknownError,
}

impl SetupErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OrchestratorSandboxDirCreateFailed => "orchestrator_sandbox_dir_create_failed",
            Self::OrchestratorElevationCheckFailed => "orchestrator_elevation_check_failed",
            Self::OrchestratorElevationRequired => "orchestrator_elevation_required",
            Self::OrchestratorPayloadSerializeFailed => "orchestrator_payload_serialize_failed",
            Self::OrchestratorHelperLaunchFailed => "orchestrator_helper_launch_failed",
            Self::OrchestratorHelperLaunchCanceled => "orchestrator_helper_launch_canceled",
            Self::OrchestratorHelperExitNonzero => "orchestrator_helper_exit_nonzero",
            Self::OrchestratorHelperReportReadFailed => "orchestrator_helper_report_read_failed",
            Self::OrchestratorHelperIncomplete => "orchestrator_helper_incomplete",
            Self::HelperRequestArgsFailed => "helper_request_args_failed",
            Self::HelperSandboxDirCreateFailed => "helper_sandbox_dir_create_failed",
            Self::HelperLogFailed => "helper_log_failed",
            Self::HelperUserProvisionFailed => "helper_user_provision_failed",
            Self::HelperUsersGroupCreateFailed => "helper_users_group_create_failed",
            Self::HelperUserCreateOrUpdateFailed => "helper_user_create_or_update_failed",
            Self::HelperDpapiProtectFailed => "helper_dpapi_protect_failed",
            Self::HelperUsersFileWriteFailed => "helper_users_file_write_failed",
            Self::HelperSetupMarkerWriteFailed => "helper_setup_marker_write_failed",
            Self::HelperSidResolveFailed => "helper_sid_resolve_failed",
            Self::HelperCapabilitySidFailed => "helper_capability_sid_failed",
            Self::HelperFirewallComInitFailed => "helper_firewall_com_init_failed",
            Self::HelperFirewallPolicyAccessFailed => "helper_firewall_policy_access_failed",
            Self::HelperFirewallPolicyIneffective => "helper_firewall_policy_ineffective",
            Self::HelperFirewallRuleCreateOrAddFailed => {
                "helper_firewall_rule_create_or_add_failed"
            }
            Self::HelperFirewallRuleVerifyFailed => "helper_firewall_rule_verify_failed",
            Self::HelperReadAclHelperSpawnFailed => "helper_read_acl_helper_spawn_failed",
            Self::HelperReadAclMutexProbeFailed => "helper_read_acl_mutex_probe_failed",
            Self::HelperDenyReadAclFailed => "helper_deny_read_acl_failed",
            Self::HelperSandboxLockFailed => "helper_sandbox_lock_failed",
            Self::HelperUnknownError => "helper_unknown_error",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupErrorReport {
    pub code: SetupErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acl_operation: Option<AclOperation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_error_code: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupFailure {
    pub code: SetupErrorCode,
    pub message: String,
    pub acl_operation: Option<AclOperation>,
    pub windows_error_code: Option<u32>,
}

impl SetupFailure {
    pub fn new(code: SetupErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            acl_operation: None,
            windows_error_code: None,
        }
    }

    pub fn from_report(report: SetupErrorReport) -> Self {
        Self {
            code: report.code,
            message: report.message,
            acl_operation: report.acl_operation,
            windows_error_code: report.windows_error_code,
        }
    }

    /// Adds a safe, typed ACL cause without retaining the failing path or raw context.
    pub fn with_acl_error(mut self, error: WindowsAclError) -> Self {
        self.acl_operation = Some(error.operation);
        self.windows_error_code = Some(error.code);
        self.message = format!(
            "{}: acl_operation={:?}, windows_error_code={}",
            self.message, error.operation, error.code
        );
        self
    }

    /// Adds a safe, typed mutex cause without retaining paths or raw context.
    pub fn with_mutex_error(mut self, error: ReadAclMutexError) -> Self {
        self.message = format!(
            "{}: mutex_operation={:?}, windows_error_code={}",
            self.message, error.operation, error.code
        );
        self
    }

    pub fn metric_message(&self) -> String {
        sanitize_setup_metric_tag_value(&self.message)
    }
}

impl std::fmt::Display for SetupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for SetupFailure {}

pub fn failure(code: SetupErrorCode, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(SetupFailure::new(code, message))
}

pub fn extract_failure(err: &anyhow::Error) -> Option<&SetupFailure> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<SetupFailure>())
}

/// Returns only stable setup/Win32 cause fields suitable for a protected-path diagnostic.
pub fn safe_windows_error_summary(error: &anyhow::Error) -> String {
    if let Some(failure) = extract_failure(error) {
        if let (Some(operation), Some(code)) = (failure.acl_operation, failure.windows_error_code) {
            return format!(
                "setup_code={}, acl_operation={operation:?}, windows_error_code={code}",
                failure.code.as_str()
            );
        }
        if failure.code == SetupErrorCode::HelperReadAclMutexProbeFailed {
            return format!("setup_code={}, {}", failure.code.as_str(), failure.message);
        }
        return format!("setup_code={}", failure.code.as_str());
    }
    if let Some(error) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<WindowsAclError>())
    {
        return format!(
            "acl_operation={:?}, windows_error_code={}",
            error.operation, error.code
        );
    }
    if let Some(error) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ReadAclMutexError>())
    {
        return format!(
            "mutex_operation={:?}, windows_error_code={}",
            error.operation, error.code
        );
    }
    "windows sandbox operation failed".to_string()
}

pub fn setup_error_path(sandbox_home: &Path) -> PathBuf {
    sandbox_home.join(".sandbox").join("setup_error.json")
}

pub fn clear_setup_error_report(sandbox_home: &Path) -> Result<()> {
    let path = setup_error_path(sandbox_home);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

pub fn write_setup_error_report(sandbox_home: &Path, report: &SetupErrorReport) -> Result<()> {
    let sandbox_dir = sandbox_home.join(".sandbox");
    fs::create_dir_all(&sandbox_dir)
        .with_context(|| format!("create sandbox dir {}", sandbox_dir.display()))?;
    let path = setup_error_path(sandbox_home);
    let json = serde_json::to_vec_pretty(report)?;
    fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn read_setup_error_report(sandbox_home: &Path) -> Result<Option<SetupErrorReport>> {
    let path = setup_error_path(sandbox_home);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let report = serde_json::from_slice::<SetupErrorReport>(&bytes)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(report))
}

/// Sanitize a setup error message for use as a metric tag.
pub fn sanitize_setup_metric_tag_value(value: &str) -> String {
    sanitize_metric_tag_value(redact_home_paths(value).as_str())
}

fn redact_home_paths(value: &str) -> String {
    let mut usernames: Vec<String> = Vec::new();
    if let Ok(username) = std::env::var("USERNAME")
        && !username.trim().is_empty()
    {
        usernames.push(username);
    }
    if let Ok(user) = std::env::var("USER")
        && !user.trim().is_empty()
        && !usernames.iter().any(|v| v.eq_ignore_ascii_case(&user))
    {
        usernames.push(user);
    }

    redact_username_segments(value, &usernames)
}

fn redact_username_segments(value: &str, usernames: &[String]) -> String {
    if usernames.is_empty() {
        return value.to_string();
    }

    let mut segments: Vec<String> = Vec::new();
    let mut separators: Vec<char> = Vec::new();
    let mut current = String::new();

    for ch in value.chars() {
        if ch == '\\' || ch == '/' {
            segments.push(std::mem::take(&mut current));
            separators.push(ch);
        } else {
            current.push(ch);
        }
    }
    segments.push(current);

    for segment in &mut segments {
        let matches = if cfg!(windows) {
            usernames
                .iter()
                .any(|name| segment.eq_ignore_ascii_case(name))
        } else {
            usernames.iter().any(|name| segment == name)
        };
        if matches {
            *segment = "<user>".to_string();
        }
    }

    let mut out = String::new();
    for (idx, segment) in segments.iter().enumerate() {
        out.push_str(segment);
        if let Some(sep) = separators.get(idx) {
            out.push(*sep);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::AclOperation;
    use super::SetupErrorCode;
    use super::SetupFailure;
    use super::WindowsAclError;
    use super::redact_username_segments;
    use super::safe_windows_error_summary;
    use anyhow::Error;
    use pretty_assertions::assert_eq;

    #[test]
    fn sanitize_tag_value_redacts_username_segments() {
        let usernames = vec!["Alice".to_string(), "Bob".to_string()];
        let msg = "failed to write C:\\Users\\Alice\\file.txt; fallback D:\\Profiles\\Bob\\x";
        let redacted = redact_username_segments(msg, &usernames);
        assert_eq!(
            redacted,
            "failed to write C:\\Users\\<user>\\file.txt; fallback D:\\Profiles\\<user>\\x"
        );
    }

    #[test]
    fn sanitize_tag_value_leaves_unknown_segments() {
        let usernames = vec!["Alice".to_string()];
        let msg = "failed to write E:\\data\\file.txt";
        let redacted = redact_username_segments(msg, &usernames);
        assert_eq!(redacted, msg);
    }

    #[test]
    fn sanitize_tag_value_redacts_multiple_occurrences() {
        let usernames = vec!["Alice".to_string()];
        let msg = "C:\\Users\\Alice\\a and C:\\Users\\Alice\\b";
        let redacted = redact_username_segments(msg, &usernames);
        assert_eq!(redacted, "C:\\Users\\<user>\\a and C:\\Users\\<user>\\b");
    }

    #[test]
    fn acl_failure_keeps_only_typed_operation_and_code() {
        let failure = SetupFailure::new(
            SetupErrorCode::HelperDenyReadAclFailed,
            "deny-read ACL reconciliation failed",
        )
        .with_acl_error(WindowsAclError {
            operation: AclOperation::OpenTarget,
            code: 5,
        });
        let error = Error::new(failure.clone());

        assert_eq!(failure.acl_operation, Some(AclOperation::OpenTarget));
        assert_eq!(failure.windows_error_code, Some(5));
        let summary = safe_windows_error_summary(&error);
        assert!(summary.contains("acl_operation=OpenTarget"));
        assert!(summary.contains("windows_error_code=5"));
        assert!(!summary.contains("protected"));
    }
}
