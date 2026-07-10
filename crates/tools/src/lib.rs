#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use singularity_core::{CancellationToken, contains_sensitive_text};
pub use singularity_sandbox::{
    CommandExecutionStatus, CommandRequest, CommandResult, CommandSemanticStatus,
    DEFAULT_COMMAND_TIMEOUT_SECONDS, SandboxBackend, SandboxBackendEnforcement,
    SandboxCapabilities, SandboxFilesystemMode, SandboxNetworkMode, WindowsSandboxBackend,
    command_permission_resource,
};

const TOOL_PROTOCOL_VERSION: &str = "1.0";
const DEFAULT_TOOL_VERSION: &str = "0.0.1";
const REDACTED_TOOL_OUTPUT: &str = "[redacted sensitive tool output]";
const UNKNOWN_TOOL_ERROR: &str = "unknown_tool";
const TOOL_DENIED_ERROR: &str = "tool_denied";
const TOOL_APPROVAL_REQUIRED_ERROR: &str = "approval_required";
const TOOL_SANDBOX_UNAVAILABLE_ERROR: &str = "sandbox_unavailable";
const WORKSPACE_MUTATION_NOT_APPROVED: &str = "workspace mutation requires allowed tool decision";
const DUPLICATE_PATCH_TARGET: &str = "patch contains duplicate canonical target";
const MUTATION_TEMP_FILE_ATTEMPTS: usize = 64;
const DEFAULT_READ_MAX_CHARS: usize = 8_192;
const DEFAULT_LIST_MAX_ENTRIES: usize = 200;
const DEFAULT_GREP_MAX_MATCHES: usize = 200;
const LARGE_OUTPUT_ARTIFACT_THRESHOLD: usize = 4_096;
const DEFAULT_RESULT_PREVIEW_MAX_CHARS: usize = 4_096;
const BINARY_CONTENT_PREVIEW: &str = "[binary content omitted]";
const DIGEST_PREFIX: &str = "hash:";
const FNV64_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV64_PRIME: u64 = 0x100000001b3;
const DIFF_ARTIFACT_PREFIX: &str = "artifact://diff/";
const RESULT_ARTIFACT_PREFIX: &str = "artifact://result/";
const PROTECTED_PATH_EXACT_MARKERS: [&str; 13] = [
    ".aws",
    ".azure",
    ".git",
    ".gnupg",
    ".ssh",
    "credentials",
    "credentials.json",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "id_rsa",
    "secret",
    "secrets",
];
const PROTECTED_PATH_PREFIXES: [&str; 3] = [".env", "credential", "private-key"];
const PROTECTED_PATH_SUFFIXES: [&str; 4] = [".key", ".pem", ".p12", ".pfx"];
const PROMPT_INJECTION_MARKERS: [&str; 4] = [
    "developer message",
    "ignore previous",
    "reveal hidden",
    "system prompt",
];
static COMMAND_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
static MUTATION_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionLevel {
    ReadOnly,
    Write,
    Shell,
    Git,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolSpec {
    pub name: String,
    pub version: String,
    pub description: String,
    pub input_schema: Value,
    pub permission_level: PermissionLevel,
    pub risk_tags: Vec<String>,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            version: DEFAULT_TOOL_VERSION.to_string(),
            description: description.into(),
            input_schema,
            permission_level: PermissionLevel::ReadOnly,
            risk_tags: Vec::new(),
        }
    }

    pub fn to_schema_payload(&self) -> Value {
        json!({
            "name": self.name,
            "description": redact_public_text(&self.description),
            "input_schema": self.input_schema,
        })
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolSpec>,
}

impl ToolRegistry {
    pub fn register(&mut self, spec: ToolSpec) -> Result<(), String> {
        validate_tool_name(&spec.name)?;
        if self.tools.contains_key(&spec.name) {
            return Err(format!("tool already registered: {}", spec.name));
        }
        self.tools.insert(spec.name.clone(), spec);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    pub fn schema_payloads(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(ToolSpec::to_schema_payload)
            .collect::<Vec<_>>()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolBrokerDecision {
    Allow,
    Approved {
        approval_grant_id: String,
    },
    Deny {
        reason: String,
    },
    Ask {
        approval_request_id: String,
        reason: String,
    },
}

impl ToolBrokerDecision {
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }

    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow | Self::Approved { .. })
    }

    pub fn approved(approval_grant_id: impl Into<String>) -> Self {
        Self::Approved {
            approval_grant_id: approval_grant_id.into(),
        }
    }

    pub fn ask(approval_request_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Ask {
            approval_request_id: approval_request_id.into(),
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolBroker {
    registry: ToolRegistry,
}

impl ToolBroker {
    pub fn new(registry: ToolRegistry) -> Self {
        Self { registry }
    }

    pub fn register(&mut self, spec: ToolSpec) -> Result<(), String> {
        self.registry.register(spec)
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.registry.get(name)
    }

    pub fn tool_schema_payloads(&self) -> Vec<Value> {
        self.registry.schema_payloads()
    }

    pub fn execute<F>(
        &self,
        envelope: &ToolCallRequest,
        decision: ToolBrokerDecision,
        executor: F,
    ) -> ToolResult
    where
        F: FnOnce(&ToolCallRequest) -> ToolOutput,
    {
        if self.registry.get(&envelope.tool_name).is_none() {
            return ToolResult::failed(envelope, UNKNOWN_TOOL_ERROR, "tool is not registered");
        }
        if let ToolBrokerDecision::Deny { reason } = decision {
            return ToolResult::failed(envelope, TOOL_DENIED_ERROR, reason);
        }
        if let ToolBrokerDecision::Ask {
            approval_request_id,
            reason,
        } = decision
        {
            return ToolResult::approval_required(envelope, approval_request_id, reason);
        }
        ToolResult::from_result(envelope, &executor(envelope))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCallRequest {
    pub protocol_version: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub raw_arguments: String,
}

impl ToolCallRequest {
    pub fn new(
        run_id: impl Into<String>,
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        raw_arguments: impl Into<String>,
    ) -> Self {
        Self {
            protocol_version: TOOL_PROTOCOL_VERSION.to_string(),
            run_id: run_id.into(),
            session_id: session_id.into(),
            task_id: task_id.into(),
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            raw_arguments: raw_arguments.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolOutput {
    pub ok: bool,
    pub content: Value,
    pub error_code: Option<String>,
    pub truncated: bool,
    pub metadata: Value,
}

impl ToolOutput {
    pub fn success(content: Value) -> Self {
        Self {
            ok: true,
            content,
            error_code: None,
            truncated: false,
            metadata: json!({}),
        }
    }

    pub fn failure(error_code: impl Into<String>, content: Value) -> Self {
        Self {
            ok: false,
            content,
            error_code: Some(error_code.into()),
            truncated: false,
            metadata: json!({}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub ok: bool,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    pub digest: String,
    pub artifact_ref: Option<String>,
    pub error_code: Option<String>,
    pub artifact_refs: Vec<String>,
    pub result_id: Option<String>,
    pub approval_request_id: Option<String>,
    pub truncated: bool,
    pub redacted: bool,
    #[serde(skip)]
    policy_decision_id: Option<String>,
    #[serde(skip)]
    approval_grant_id: Option<String>,
    #[serde(skip)]
    audit_metadata: Option<Value>,
}

impl ToolResult {
    pub fn summary(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        ok: bool,
        preview: impl Into<String>,
        digest: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            ok,
            status: if ok { "ok" } else { "error" }.to_string(),
            preview: Some(redact_public_text(&preview.into())),
            digest: digest.into(),
            artifact_ref: None,
            error_code: None,
            artifact_refs: Vec::new(),
            result_id: None,
            approval_request_id: None,
            truncated: false,
            redacted: true,
            policy_decision_id: None,
            approval_grant_id: None,
            audit_metadata: None,
        }
    }

    pub fn with_audit_metadata(
        mut self,
        policy_decision_id: impl Into<String>,
        approval_grant_id: impl Into<String>,
        metadata: Value,
    ) -> Self {
        self.policy_decision_id = Some(policy_decision_id.into());
        self.approval_grant_id = Some(approval_grant_id.into());
        self.audit_metadata = Some(metadata);
        self
    }

    pub fn with_audit(mut self, metadata: Value) -> Self {
        self.audit_metadata = Some(metadata);
        self
    }

    pub fn audit_metadata(&self) -> Option<&Value> {
        self.audit_metadata.as_ref()
    }

    pub fn from_result(envelope: &ToolCallRequest, result: &ToolOutput) -> Self {
        let result_content = result.content.to_string();
        let (preview, preview_truncated) =
            bounded_text(&result_content, DEFAULT_RESULT_PREVIEW_MAX_CHARS);
        let truncated = result.truncated || preview_truncated;
        let artifact_ref = result_artifact_ref(&result.content, &result.metadata);
        let artifact_refs = result_artifact_refs(&result.content, &result.metadata);
        let result_id = result_id(&result.content, &result.metadata);
        let mut tool_result = Self {
            error_code: result.error_code.clone(),
            truncated,
            ..Self::summary(
                envelope.tool_call_id.clone(),
                envelope.tool_name.clone(),
                result.ok,
                preview,
                stable_digest(&result.content),
            )
        };
        tool_result.artifact_ref = artifact_ref;
        tool_result.artifact_refs = artifact_refs;
        tool_result.result_id = result_id;
        if truncated
            && (tool_result.artifact_ref.is_some()
                || !tool_result.artifact_refs.is_empty()
                || tool_result.result_id.is_some())
        {
            tool_result.preview = None;
        }
        tool_result.audit_metadata = result.metadata.get("audit").cloned();
        tool_result
    }

    pub fn failed(
        envelope: &ToolCallRequest,
        error_code: impl Into<String>,
        preview: impl Into<String>,
    ) -> Self {
        Self {
            error_code: Some(error_code.into()),
            ..Self::summary(
                envelope.tool_call_id.clone(),
                envelope.tool_name.clone(),
                false,
                preview,
                "",
            )
        }
    }

    pub fn approval_required(
        envelope: &ToolCallRequest,
        approval_request_id: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            approval_request_id: Some(approval_request_id.into()),
            ..Self::failed(envelope, TOOL_APPROVAL_REQUIRED_ERROR, reason)
        }
    }

    pub fn to_message_payload(&self) -> Value {
        let artifact_ref = self.artifact_ref.as_deref().and_then(safe_reference);
        let artifact_refs = self
            .artifact_refs
            .iter()
            .filter_map(|value| safe_reference(value))
            .collect::<Vec<_>>();
        let result_id = self.result_id.as_deref().and_then(safe_reference);
        let mut payload = json!({
            "ok": self.ok,
            "tool_name": self.tool_name,
            "tool_call_id": self.tool_call_id,
            "status": self.status,
            "digest": self.digest,
            "artifact_ref": artifact_ref,
            "error_code": self.error_code,
            "artifact_refs": artifact_refs,
            "result_id": result_id,
            "truncated": self.truncated,
            "redacted": self.redacted,
        });
        if let Some(preview) = self.preview.as_deref() {
            let preview = redact_public_text(preview);
            payload["content"] = json!(preview);
            payload["preview"] = json!(preview);
        }
        payload
    }
}

fn result_artifact_ref(content: &Value, metadata: &Value) -> Option<String> {
    value_string(content.get("artifact_ref"))
        .or_else(|| value_string(content.get("diff_ref")))
        .or_else(|| value_string(metadata.get("artifact_ref")))
        .or_else(|| value_string(metadata.get("diff_ref")))
}

fn result_artifact_refs(content: &Value, metadata: &Value) -> Vec<String> {
    let mut refs = Vec::new();
    refs.extend(value_string(content.get("artifact_ref")));
    refs.extend(value_string(content.get("diff_ref")));
    refs.extend(value_string(metadata.get("artifact_ref")));
    refs.extend(value_string(metadata.get("diff_ref")));
    refs.extend(value_string_array(content.get("artifact_refs")));
    refs.extend(value_string_array(metadata.get("artifact_refs")));
    refs.sort();
    refs.dedup();
    refs
}

fn result_id(content: &Value, metadata: &Value) -> Option<String> {
    value_string(metadata.get("result_id"))
        .or_else(|| value_string(content.get("result_id")))
        .or_else(|| value_string(metadata.get("output_digest")))
        .or_else(|| value_string(content.get("output_digest")))
}

fn value_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

fn safe_reference(value: &str) -> Option<String> {
    let lowered = value.to_ascii_lowercase();
    if contains_sensitive_text(value)
        || PROMPT_INJECTION_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker))
    {
        None
    } else {
        Some(value.to_string())
    }
}

fn value_string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceToolError {
    OutsideWorkspace(String),
    ProtectedPath(String),
    SandboxUnavailable,
    BinaryPattern,
    ReadFailed(String),
    RollbackFailed(String),
    ExpectedContentMissing(String),
    InvalidInput(String),
}

impl fmt::Display for WorkspaceToolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideWorkspace(path) => write!(formatter, "path outside workspace: {path}"),
            Self::ProtectedPath(path) => {
                write!(formatter, "protected path requires approval: {path}")
            }
            Self::SandboxUnavailable => write!(formatter, "strict sandbox backend unavailable"),
            Self::BinaryPattern => write!(formatter, "grep pattern must be valid utf-8 text"),
            Self::ReadFailed(message) => write!(formatter, "workspace tool read failed: {message}"),
            Self::RollbackFailed(message) => {
                write!(formatter, "workspace mutation rollback failed: {message}")
            }
            Self::ExpectedContentMissing(path) => {
                write!(formatter, "expected content not found in {path}")
            }
            Self::InvalidInput(message) => {
                write!(formatter, "invalid workspace tool input: {message}")
            }
        }
    }
}

impl std::error::Error for WorkspaceToolError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReadToolInput {
    pub path: String,
    pub max_chars: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ListToolInput {
    pub path: Option<String>,
    pub max_entries: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GrepToolInput {
    pub path: Option<String>,
    pub pattern: String,
    pub max_matches: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EditToolInput {
    pub path: String,
    pub expected: String,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkspacePatch {
    pub changes: Vec<WorkspacePatchChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkspacePatchChange {
    pub path: String,
    pub expected: Option<String>,
    pub replacement: String,
}

#[derive(Clone)]
pub struct WorkspaceTools {
    workspace_root: PathBuf,
    sandbox_backend: Option<Arc<dyn SandboxBackend + Send + Sync>>,
}

impl fmt::Debug for WorkspaceTools {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceTools")
            .field("workspace_root", &self.workspace_root)
            .field(
                "sandbox_backend",
                &self.sandbox_backend.as_ref().map(|backend| backend.name()),
            )
            .finish()
    }
}

impl WorkspaceTools {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            sandbox_backend: None,
        }
    }

    pub fn with_sandbox_backend(
        self,
        sandbox_backend: impl SandboxBackend + Send + Sync + 'static,
    ) -> Self {
        self.with_shared_sandbox_backend(Arc::new(sandbox_backend))
    }

    pub fn with_shared_sandbox_backend(
        mut self,
        sandbox_backend: Arc<dyn SandboxBackend + Send + Sync>,
    ) -> Self {
        self.sandbox_backend = Some(sandbox_backend);
        self
    }

    pub fn read(&self, input: ReadToolInput) -> Result<ToolOutput, WorkspaceToolError> {
        let target = self.resolve_workspace_path(&input.path, false)?;
        let bytes = std::fs::read(&target).map_err(io_error)?;
        let relative = self.relative_path(&target);
        if is_binary(&bytes) {
            return Ok(ToolOutput::success(json!({
                "path": relative,
                "binary": true,
                "preview": BINARY_CONTENT_PREVIEW,
                "truncated": true,
                "artifact_ref": artifact_ref(RESULT_ARTIFACT_PREFIX, &relative),
            })));
        }
        let content = String::from_utf8(bytes).map_err(|error| {
            WorkspaceToolError::ReadFailed(format!("invalid utf-8 after binary check: {error}"))
        })?;
        let max_chars = input.max_chars.unwrap_or(DEFAULT_READ_MAX_CHARS);
        let (preview, truncated) = bounded_text(&content, max_chars);
        Ok(ToolOutput::success(json!({
            "path": relative,
            "binary": false,
            "preview": preview,
            "truncated": truncated,
            "artifact_ref": if truncated || content.len() > LARGE_OUTPUT_ARTIFACT_THRESHOLD {
                Value::String(artifact_ref(RESULT_ARTIFACT_PREFIX, &relative))
            } else {
                Value::Null
            },
        })))
    }

    pub fn list(&self, input: ListToolInput) -> Result<ToolOutput, WorkspaceToolError> {
        let target = self.resolve_workspace_path(input.path.as_deref().unwrap_or("."), true)?;
        let mut entries = Vec::new();
        let mut redacted_entries = 0usize;
        let max_entries = input.max_entries.unwrap_or(DEFAULT_LIST_MAX_ENTRIES);
        for entry in std::fs::read_dir(&target).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let path = entry.path();
            let relative = self.relative_path(&path);
            if is_protected_path(&relative) {
                redacted_entries += 1;
                continue;
            }
            entries.push(json!({
                "path": relative,
                "kind": if path.is_dir() { "directory" } else { "file" },
            }));
            if entries.len() >= max_entries {
                break;
            }
        }
        Ok(ToolOutput::success(json!({
            "entries": entries,
            "redacted_entries": redacted_entries,
            "truncated": entries.len() >= max_entries,
        })))
    }

    pub fn grep(&self, input: GrepToolInput) -> Result<ToolOutput, WorkspaceToolError> {
        if input.pattern.is_empty() {
            return Err(WorkspaceToolError::InvalidInput(
                "pattern must not be empty".to_string(),
            ));
        }
        let root = self
            .resolve_workspace_path(input.path.as_deref().unwrap_or("."), input.path.is_none())?;
        let max_matches = input.max_matches.unwrap_or(DEFAULT_GREP_MAX_MATCHES);
        let mut matches = Vec::new();
        self.grep_path(&root, &input.pattern, max_matches, &mut matches)?;
        let truncated = matches.len() >= max_matches;
        Ok(ToolOutput::success(json!({
            "matches": matches,
            "truncated": truncated,
        })))
    }

    pub fn edit(
        &self,
        input: EditToolInput,
        decision: &ToolBrokerDecision,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        self.patch(
            WorkspacePatch {
                changes: vec![WorkspacePatchChange {
                    path: input.path,
                    expected: Some(input.expected),
                    replacement: input.replacement,
                }],
            },
            decision,
        )
    }

    pub fn patch(
        &self,
        patch: WorkspacePatch,
        decision: &ToolBrokerDecision,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        if !decision.is_allowed() {
            return Err(WorkspaceToolError::InvalidInput(
                WORKSPACE_MUTATION_NOT_APPROVED.to_string(),
            ));
        }
        if patch.changes.is_empty() {
            return Err(WorkspaceToolError::InvalidInput(
                "patch must contain at least one change".to_string(),
            ));
        }
        let mut prepared = Vec::new();
        let mut targets = BTreeSet::new();
        for change in &patch.changes {
            let target = self.resolve_workspace_path(&change.path, false)?;
            let relative = self.relative_path(&target);
            if !targets.insert(target.clone()) {
                return Err(WorkspaceToolError::InvalidInput(format!(
                    "{DUPLICATE_PATCH_TARGET}: {relative}"
                )));
            }
            let original = existing_text_or_empty(&target)?;
            let updated = if let Some(expected) = &change.expected {
                if !original.contains(expected) {
                    return Err(WorkspaceToolError::ExpectedContentMissing(relative));
                }
                original.replacen(expected, &change.replacement, 1)
            } else {
                change.replacement.clone()
            };
            prepared.push((target, relative, original, updated));
        }
        let originals = prepared
            .iter()
            .map(|(path, relative, original, _updated)| {
                (
                    path.clone(),
                    relative.clone(),
                    original.clone(),
                    path.exists(),
                )
            })
            .collect::<Vec<_>>();
        for (path, _relative, _original, updated) in &prepared {
            if let Err(write_error) = atomic_write(path, updated) {
                if let Err(rollback_error) = rollback_originals(&originals) {
                    return Err(WorkspaceToolError::RollbackFailed(format!(
                        "write error: {write_error}; rollback error: {rollback_error}"
                    )));
                }
                return Err(write_error);
            }
        }
        let changed_files = prepared
            .iter()
            .map(|(_path, relative, _original, _updated)| relative.clone())
            .collect::<Vec<_>>();
        Ok(ToolOutput::success(json!({
            "changed_files": changed_files,
            "diff_ref": artifact_ref(DIFF_ARTIFACT_PREFIX, &changed_files.join(",")),
            "rolled_back": false,
        })))
    }

    pub fn command(&self, input: CommandToolInput) -> Result<ToolOutput, WorkspaceToolError> {
        self.command_cancellable(input, &CancellationToken::new())
    }

    pub fn command_cancellable(
        &self,
        input: CommandToolInput,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        let Some(backend) = &self.sandbox_backend else {
            return Err(WorkspaceToolError::SandboxUnavailable);
        };
        let capabilities = backend.capabilities();
        if !capabilities.supports_command_execution() {
            return Err(WorkspaceToolError::SandboxUnavailable);
        }
        let filesystem_mode = input.sandbox_mode();
        let network_mode = input.network_access();
        let mut request = CommandRequest::project_verification(
            next_command_id(),
            input.argv,
            input.cwd.unwrap_or_else(|| ".".to_string()),
            self.workspace_root.to_string_lossy().into_owned(),
        );
        request.filesystem.mode = filesystem_mode.clone();
        request.network.mode = network_mode.clone();
        if let Some(timeout_seconds) = input.timeout_seconds {
            request.timeout_seconds = timeout_seconds;
        }
        let scope_digest = command_scope_digest(
            &request.argv,
            &request.cwd,
            request.timeout_seconds,
            &request.filesystem.mode,
            &request.network.mode,
        );
        let result = backend.execute_cancellable(&request, cancellation);
        let execution = result.sandbox.clone();
        let mut output = command_tool_output(result);
        output.metadata["result_id"] = json!(scope_digest);
        output.metadata["audit"] = json!({
            "cwd": request.cwd,
            "timeout_seconds": request.timeout_seconds,
            "sandbox_mode": filesystem_mode,
            "network_access": network_mode,
            "sandbox_backend": execution.backend,
            "sandbox_enforcement": execution.enforcement,
            "local_process_fallback": execution.local_process_fallback,
            "command_scope_digest": scope_digest,
            "command_provenance": "agent_requested",
        });
        Ok(output)
    }

    fn grep_path(
        &self,
        root: &Path,
        pattern: &str,
        max_matches: usize,
        matches: &mut Vec<Value>,
    ) -> Result<(), WorkspaceToolError> {
        if matches.len() >= max_matches {
            return Ok(());
        }
        let workspace = std::fs::canonicalize(&self.workspace_root).map_err(io_error)?;
        let resolved = std::fs::canonicalize(root).map_err(io_error)?;
        if !resolved.starts_with(&workspace) {
            return Ok(());
        }
        let root = resolved.as_path();
        let relative = self.relative_path(root);
        if is_protected_path(&relative) {
            return Ok(());
        }
        if root.is_dir() {
            for entry in std::fs::read_dir(root).map_err(io_error)? {
                let entry = entry.map_err(io_error)?;
                if entry.file_type().map_err(io_error)?.is_symlink() {
                    continue;
                }
                self.grep_path(&entry.path(), pattern, max_matches, matches)?;
                if matches.len() >= max_matches {
                    break;
                }
            }
            return Ok(());
        }
        let bytes = std::fs::read(root).map_err(io_error)?;
        if is_binary(&bytes) {
            return Ok(());
        }
        let content =
            String::from_utf8(bytes).map_err(|_error| WorkspaceToolError::BinaryPattern)?;
        for (line_index, line) in content.lines().enumerate() {
            if line.contains(pattern) {
                let line_number = line_index + 1;
                matches.push(json!({
                    "path": relative,
                    "line": line_number,
                    "preview": line,
                }));
                if matches.len() >= max_matches {
                    break;
                }
            }
        }
        Ok(())
    }

    fn resolve_workspace_path(
        &self,
        path: &str,
        allow_protected: bool,
    ) -> Result<PathBuf, WorkspaceToolError> {
        let candidate = Path::new(path);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.workspace_root.join(candidate)
        };
        let normalized = normalize_path(&joined);
        let workspace = std::fs::canonicalize(&self.workspace_root).map_err(io_error)?;
        let resolved = canonicalize_existing_or_parent(&normalized)?;
        if !resolved.starts_with(&workspace) {
            return Err(WorkspaceToolError::OutsideWorkspace(path.to_string()));
        }
        let relative = resolved
            .strip_prefix(&workspace)
            .unwrap_or(resolved.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        let intended_relative = normalized
            .strip_prefix(&workspace)
            .unwrap_or(normalized.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        if !allow_protected
            && (is_protected_path(&relative) || is_protected_path(&intended_relative))
        {
            return Err(WorkspaceToolError::ProtectedPath(intended_relative));
        }
        Ok(resolved)
    }

    fn relative_path(&self, path: &Path) -> String {
        let workspace = std::fs::canonicalize(&self.workspace_root)
            .unwrap_or_else(|_| normalize_path(&self.workspace_root));
        normalize_path(path)
            .strip_prefix(&workspace)
            .unwrap_or(path)
            .to_string_lossy()
            .trim_start_matches(['/', '\\'])
            .replace('\\', "/")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommandToolInput {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub sandbox_mode: Option<SandboxFilesystemMode>,
    pub network_access: Option<SandboxNetworkMode>,
}

impl CommandToolInput {
    pub fn effective_cwd(&self) -> &str {
        self.cwd.as_deref().unwrap_or(".")
    }

    pub fn effective_timeout_seconds(&self) -> u64 {
        self.timeout_seconds
            .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECONDS)
    }

    pub fn sandbox_mode(&self) -> SandboxFilesystemMode {
        self.sandbox_mode
            .clone()
            .unwrap_or(SandboxFilesystemMode::ReadOnly)
    }

    pub fn network_access(&self) -> SandboxNetworkMode {
        self.network_access
            .clone()
            .unwrap_or(SandboxNetworkMode::Denied)
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

pub fn command_scope_resource(
    argv: &[String],
    cwd: &str,
    timeout_seconds: u64,
    sandbox_mode: &SandboxFilesystemMode,
    network_access: &SandboxNetworkMode,
) -> String {
    let command = command_permission_resource(argv);
    if command.is_empty() {
        String::new()
    } else {
        let scope = CommandScope::new(argv, cwd, timeout_seconds, sandbox_mode, network_access);
        format!(
            "command:{command};scope:{};digest:{}",
            scope.encoded(),
            scope.digest()
        )
    }
}

pub fn command_scope_digest(
    argv: &[String],
    cwd: &str,
    timeout_seconds: u64,
    sandbox_mode: &SandboxFilesystemMode,
    network_access: &SandboxNetworkMode,
) -> String {
    CommandScope::new(argv, cwd, timeout_seconds, sandbox_mode, network_access).digest()
}

fn validate_tool_name(name: &str) -> Result<(), String> {
    let parts = name.split('.').collect::<Vec<_>>();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(format!("tool name has an empty namespace segment: {name}"));
    }
    match parts.as_slice() {
        ["builtin", _tool] => Ok(()),
        ["mcp", _server, _tool] => Ok(()),
        _ => Err(format!(
            "tool name must use builtin.* or mcp.<server>.<tool>: {name}"
        )),
    }
}

fn redact_public_text(text: &str) -> String {
    let lowered = text.to_ascii_lowercase();
    if contains_sensitive_text(text)
        || PROMPT_INJECTION_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker))
    {
        REDACTED_TOOL_OUTPUT.to_string()
    } else {
        text.to_string()
    }
}

fn normalize_path(path: &Path) -> PathBuf {
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

fn canonicalize_existing_or_parent(path: &Path) -> Result<PathBuf, WorkspaceToolError> {
    if path.exists() {
        return std::fs::canonicalize(path).map_err(io_error);
    }
    let mut missing_components = Vec::new();
    let mut ancestor = path;
    while !ancestor.exists() {
        let name = ancestor.file_name().ok_or_else(|| {
            WorkspaceToolError::ReadFailed(format!("path does not exist: {}", path.display()))
        })?;
        missing_components.push(name.to_owned());
        ancestor = ancestor.parent().ok_or_else(|| {
            WorkspaceToolError::ReadFailed(format!("path has no parent: {}", path.display()))
        })?;
    }
    let mut resolved = std::fs::canonicalize(ancestor).map_err(io_error)?;
    for component in missing_components.iter().rev() {
        resolved.push(component);
    }
    Ok(normalize_path(&resolved))
}

pub fn is_protected_path(path: &str) -> bool {
    path.replace('\\', "/")
        .split('/')
        .map(str::to_ascii_lowercase)
        .any(|component| is_protected_component(&component))
}

fn is_protected_component(component: &str) -> bool {
    PROTECTED_PATH_EXACT_MARKERS.contains(&component)
        || PROTECTED_PATH_PREFIXES.iter().any(|prefix| {
            component == *prefix
                || component
                    .strip_prefix(*prefix)
                    .is_some_and(|suffix| suffix.starts_with('.'))
        })
        || PROTECTED_PATH_SUFFIXES
            .iter()
            .any(|suffix| component.ends_with(suffix))
        || component.contains("secret")
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

fn bounded_text(content: &str, max_chars: usize) -> (String, bool) {
    let preview = content.chars().take(max_chars).collect::<String>();
    let truncated = content.chars().count() > preview.chars().count();
    (preview, truncated)
}

fn stable_digest(value: &Value) -> String {
    let mut digest = FNV64_OFFSET_BASIS;
    for byte in value.to_string().as_bytes() {
        digest ^= u64::from(*byte);
        digest = digest.wrapping_mul(FNV64_PRIME);
    }
    format!("{DIGEST_PREFIX}{digest:016x}")
}

fn artifact_ref(prefix: &str, path: &str) -> String {
    let sanitized = path
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    format!("{prefix}{sanitized}")
}

fn atomic_write(path: &Path, content: &str) -> Result<(), WorkspaceToolError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_error)?;
    }
    let (temp_path, mut temp_file) = create_unique_temp_file(path)?;
    if let Err(error) = temp_file
        .write_all(content.as_bytes())
        .and_then(|()| temp_file.sync_all())
    {
        drop(temp_file);
        let _ = std::fs::remove_file(&temp_path);
        return Err(io_error(error));
    }
    drop(temp_file);
    std::fs::rename(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        io_error(error)
    })
}

fn create_unique_temp_file(path: &Path) -> Result<(PathBuf, File), WorkspaceToolError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace-file");
    for _ in 0..MUTATION_TEMP_FILE_ATTEMPTS {
        let sequence = MUTATION_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".{file_name}.singularity-tmp-{}-{sequence}",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(error)),
        }
    }
    Err(WorkspaceToolError::ReadFailed(format!(
        "failed to allocate unique temporary file for {}",
        path.display()
    )))
}

fn existing_text_or_empty(path: &Path) -> Result<String, WorkspaceToolError> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(io_error(error)),
    }
}

fn rollback_originals(
    originals: &[(PathBuf, String, String, bool)],
) -> Result<(), WorkspaceToolError> {
    let mut failures = Vec::new();
    for (path, relative, original, existed) in originals {
        let result = if *existed {
            atomic_write(path, original)
        } else if path.exists() {
            std::fs::remove_file(path).map_err(io_error)
        } else {
            Ok(())
        };
        if let Err(error) = result {
            failures.push(format!("{relative}: {error}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(WorkspaceToolError::RollbackFailed(failures.join("; ")))
    }
}

fn io_error(error: std::io::Error) -> WorkspaceToolError {
    WorkspaceToolError::ReadFailed(error.to_string())
}

fn next_command_id() -> String {
    let sequence = COMMAND_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("command_{sequence}")
}

fn command_tool_output(result: CommandResult) -> ToolOutput {
    let ok = result.execution_status == CommandExecutionStatus::Completed
        && result.semantic_status == CommandSemanticStatus::Succeeded;
    let content = serde_json::to_value(&result).expect("command result serializes");
    if ok {
        ToolOutput::success(content)
    } else {
        ToolOutput::failure(command_error_code(&result), content)
    }
}

fn command_error_code(result: &CommandResult) -> &'static str {
    match result.execution_status {
        CommandExecutionStatus::PolicyDenied => "command_policy_denied",
        CommandExecutionStatus::ReviewRequired => "command_review_required",
        CommandExecutionStatus::Unsupported => "command_unsupported",
        CommandExecutionStatus::SpawnFailed => "command_spawn_failed",
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
