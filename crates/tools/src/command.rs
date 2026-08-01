//! command tool 输入合同、范围摘要与安全结果映射辅助函数。

use super::*;

/// 面向模型的命令输入；执行策略由受信任的 sandbox 路径固定提供。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandToolInput {
    pub command: String,
    pub cwd: Option<String>,
    pub timeout_seconds: Option<u64>,
}

impl CommandToolInput {
    pub(crate) fn validate(&self) -> Result<(), WorkspaceToolError> {
        if self.command.trim().is_empty() {
            return Err(WorkspaceToolError::InvalidInput(
                "command must not be empty".to_string(),
            ));
        }
        if self.command.contains('\0') {
            return Err(WorkspaceToolError::InvalidInput(
                "command must not contain NUL".to_string(),
            ));
        }
        if self.command.chars().count() > MAX_COMMAND_SCRIPT_CHARS {
            return Err(WorkspaceToolError::InvalidInput(format!(
                "command must not exceed {MAX_COMMAND_SCRIPT_CHARS} characters"
            )));
        }
        if self.cwd.as_deref().is_some_and(|cwd| cwd.trim().is_empty()) {
            return Err(WorkspaceToolError::InvalidInput(
                "cwd must not be empty".to_string(),
            ));
        }
        if self.timeout_seconds == Some(0) {
            return Err(WorkspaceToolError::InvalidInput(
                "timeout_seconds must be greater than zero".to_string(),
            ));
        }
        if self
            .timeout_seconds
            .is_some_and(|timeout| timeout > MAX_COMMAND_TIMEOUT_SECONDS)
        {
            return Err(WorkspaceToolError::InvalidInput(format!(
                "timeout_seconds must not exceed {MAX_COMMAND_TIMEOUT_SECONDS}"
            )));
        }
        Ok(())
    }

    pub fn effective_cwd(&self) -> &str {
        self.cwd.as_deref().unwrap_or(".")
    }

    pub fn effective_timeout_seconds(&self) -> u64 {
        self.timeout_seconds
            .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECONDS)
    }
}

#[derive(Serialize)]
struct CommandScope<'a> {
    argv: &'a [String],
    cwd: &'a str,
    timeout_seconds: u64,
    sandbox_mode: &'a SandboxFilesystemMode,
    network_access: &'a SandboxNetworkMode,
}

impl<'a> CommandScope<'a> {
    fn new(
        argv: &'a [String],
        cwd: &'a str,
        timeout_seconds: u64,
        sandbox_mode: &'a SandboxFilesystemMode,
        network_access: &'a SandboxNetworkMode,
    ) -> Self {
        Self {
            argv,
            cwd,
            timeout_seconds,
            sandbox_mode,
            network_access,
        }
    }

    fn encoded(&self) -> String {
        serde_json::to_string(self).expect("command scope is serializable")
    }

    fn digest(&self) -> String {
        format!("sha256:{:x}", Sha256::digest(self.encoded().as_bytes()))
    }
}

/// 对命令、`cwd`、超时、文件系统模式和网络模式计算哈希，用于校验绑定。
pub fn command_scope_digest(
    argv: &[String],
    cwd: &str,
    timeout_seconds: u64,
    sandbox_mode: &SandboxFilesystemMode,
    network_access: &SandboxNetworkMode,
) -> String {
    CommandScope::new(argv, cwd, timeout_seconds, sandbox_mode, network_access).digest()
}

#[derive(Serialize)]
struct CommandScriptScope<'a> {
    command: &'a str,
    cwd: &'a str,
    timeout_seconds: u64,
    sandbox_mode: SandboxFilesystemMode,
    network_access: SandboxNetworkMode,
}

impl<'a> CommandScriptScope<'a> {
    fn new(
        command: &'a str,
        cwd: &'a str,
        timeout_seconds: u64,
        sandbox_mode: SandboxFilesystemMode,
        network_access: SandboxNetworkMode,
    ) -> Self {
        Self {
            command,
            cwd,
            timeout_seconds,
            sandbox_mode,
            network_access,
        }
    }

    fn digest(&self) -> String {
        let encoded = serde_json::to_vec(self).expect("command script scope is serializable");
        format!("sha256:{:x}", Sha256::digest(encoded))
    }
}

/// 对模型 command string 及其固定的只读、离线执行范围计算哈希。
pub fn command_script_scope_digest(command: &str, cwd: &str, timeout_seconds: u64) -> String {
    command_script_scope_digest_with_policy(
        command,
        cwd,
        timeout_seconds,
        SandboxFilesystemMode::ReadOnly,
        SandboxNetworkMode::Denied,
    )
}

/// 对模型 command 及其经 Policy 绑定的执行范围计算哈希。
pub fn command_script_scope_digest_with_policy(
    command: &str,
    cwd: &str,
    timeout_seconds: u64,
    sandbox_mode: SandboxFilesystemMode,
    network_access: SandboxNetworkMode,
) -> String {
    CommandScriptScope::new(command, cwd, timeout_seconds, sandbox_mode, network_access).digest()
}

pub(crate) fn validate_tool_name(name: &str) -> Result<(), String> {
    if name.starts_with("builtin_")
        || name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        Err(format!("tool name is not provider-portable: {name}"))
    } else {
        Ok(())
    }
}

pub(crate) fn redact_public_text(text: &str) -> String {
    let lowered = text.to_ascii_lowercase();
    if contains_sensitive_text(text)
        || PROMPT_INJECTION_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker))
    {
        REDACTED_TOOL_OUTPUT.to_string()
    } else if contains_artifact_reference(text) {
        ARTIFACT_REFERENCE_OMITTED.to_string()
    } else {
        text.to_string()
    }
}

pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

pub(crate) fn bounded_text(content: &str, max_chars: usize) -> (String, bool) {
    let preview = content.chars().take(max_chars).collect::<String>();
    let truncated = content.chars().count() > preview.chars().count();
    (preview, truncated)
}

pub(crate) fn io_error(error: std::io::Error) -> WorkspaceToolError {
    WorkspaceToolError::ReadFailed(error.to_string())
}

pub(crate) fn next_command_id() -> String {
    let sequence = COMMAND_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("command_{sequence}")
}

pub(crate) fn command_tool_output(result: CommandResult) -> ToolOutput {
    let ok = result.execution_status == CommandExecutionStatus::Completed
        && result.semantic_status == CommandSemanticStatus::Succeeded;
    // 模型只接收命令语义和有界输出，backend/enforcement 仅保留在 audit metadata。
    let content = json!({
        "execution_status": result.execution_status,
        "semantic_status": result.semantic_status,
        "exit_code": result.exit_code,
        "duration_ms": result.duration_ms,
        "timed_out": result.timed_out,
        "stdout_preview": result.stdout_preview,
        "stderr_preview": result.stderr_preview,
        "output_truncated": result.output_truncated,
        "redacted": result.redacted,
    });
    if ok {
        ToolOutput::success(content)
    } else {
        ToolOutput::failure_with_kind(
            command_failure_kind(&result),
            command_error_code(&result),
            content,
        )
    }
}

pub(crate) fn command_failure_kind(result: &CommandResult) -> ToolFailureKind {
    match result.execution_status {
        CommandExecutionStatus::PolicyDenied => ToolFailureKind::Policy,
        CommandExecutionStatus::ReviewRequired => ToolFailureKind::Approval,
        CommandExecutionStatus::Unsupported => ToolFailureKind::Capability,
        CommandExecutionStatus::ExecutableUnavailable => ToolFailureKind::Capability,
        CommandExecutionStatus::TimedOut => ToolFailureKind::Timeout,
        CommandExecutionStatus::Cancelled => ToolFailureKind::Cancelled,
        CommandExecutionStatus::BackendError => ToolFailureKind::Backend,
        CommandExecutionStatus::Completed => match result.semantic_status {
            CommandSemanticStatus::Succeeded
            | CommandSemanticStatus::ExitNonzero
            | CommandSemanticStatus::TestsFailed
            | CommandSemanticStatus::BuildFailed => ToolFailureKind::Execution,
            CommandSemanticStatus::PolicyBlocked => ToolFailureKind::Policy,
            CommandSemanticStatus::Unsupported => ToolFailureKind::Capability,
            CommandSemanticStatus::TimedOut => ToolFailureKind::Timeout,
            CommandSemanticStatus::Cancelled => ToolFailureKind::Cancelled,
        },
    }
}

pub(crate) fn command_error_code(result: &CommandResult) -> &'static str {
    match result.execution_status {
        CommandExecutionStatus::PolicyDenied => "command_policy_denied",
        CommandExecutionStatus::ReviewRequired => "command_review_required",
        CommandExecutionStatus::Unsupported => "command_unsupported",
        CommandExecutionStatus::ExecutableUnavailable => "command_executable_unavailable",
        CommandExecutionStatus::TimedOut => "command_timed_out",
        CommandExecutionStatus::Cancelled => "command_cancelled",
        CommandExecutionStatus::BackendError => TOOL_SANDBOX_UNAVAILABLE_ERROR,
        CommandExecutionStatus::Completed => match result.semantic_status {
            CommandSemanticStatus::Succeeded => "command_succeeded",
            CommandSemanticStatus::ExitNonzero => "command_exit_nonzero",
            CommandSemanticStatus::TestsFailed => "command_tests_failed",
            CommandSemanticStatus::BuildFailed => "command_build_failed",
            CommandSemanticStatus::PolicyBlocked => "command_policy_blocked",
            CommandSemanticStatus::Unsupported => "command_unsupported",
            CommandSemanticStatus::TimedOut => "command_timed_out",
            CommandSemanticStatus::Cancelled => "command_cancelled",
        },
    }
}
