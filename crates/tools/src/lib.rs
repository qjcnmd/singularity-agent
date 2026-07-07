#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use singularity_core::contains_sensitive_text;

const TOOL_PROTOCOL_VERSION: &str = "1.0";
const DEFAULT_TOOL_VERSION: &str = "0.0.1";
const REDACTED_TOOL_OUTPUT: &str = "[redacted sensitive tool output]";
const UNKNOWN_TOOL_ERROR: &str = "unknown_tool";
const TOOL_DENIED_ERROR: &str = "tool_denied";
const TOOL_APPROVAL_REQUIRED_ERROR: &str = "approval_required";
const WORKSPACE_MUTATION_NOT_APPROVED: &str = "workspace mutation requires allowed tool decision";
const DEFAULT_READ_MAX_CHARS: usize = 8_192;
const DEFAULT_LIST_MAX_ENTRIES: usize = 200;
const DEFAULT_GREP_MAX_MATCHES: usize = 200;
const LARGE_OUTPUT_ARTIFACT_THRESHOLD: usize = 4_096;
const DEFAULT_OBSERVATION_PREVIEW_MAX_CHARS: usize = 4_096;
const BINARY_CONTENT_PREVIEW: &str = "[binary content omitted]";
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

    pub fn to_model_spec_payload(&self) -> Value {
        json!({
            "name": self.name,
            "description": redact_model_visible_text(&self.description),
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

    pub fn model_visible_specs(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(ToolSpec::to_model_spec_payload)
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

    pub fn allows_protected_path(&self) -> bool {
        matches!(
            self,
            Self::Approved {
                approval_grant_id
            } if !approval_grant_id.trim().is_empty()
        )
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

    pub fn model_visible_tools(&self) -> Vec<Value> {
        self.registry.model_visible_specs()
    }

    pub fn execute<F>(
        &self,
        envelope: &ToolCallEnvelope,
        decision: ToolBrokerDecision,
        executor: F,
    ) -> ToolObservation
    where
        F: FnOnce(&ToolCallEnvelope) -> ToolResult,
    {
        if self.registry.get(&envelope.tool_name).is_none() {
            return ToolObservation::failed(envelope, UNKNOWN_TOOL_ERROR, "tool is not registered");
        }
        if let ToolBrokerDecision::Deny { reason } = decision {
            return ToolObservation::failed(envelope, TOOL_DENIED_ERROR, reason);
        }
        if let ToolBrokerDecision::Ask {
            approval_request_id,
            reason,
        } = decision
        {
            return ToolObservation::approval_required(envelope, approval_request_id, reason);
        }
        ToolObservation::from_result(
            envelope,
            &executor(envelope),
            ToolObservationVisibility::Summary,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCallEnvelope {
    pub protocol_version: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub raw_arguments: String,
}

impl ToolCallEnvelope {
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
pub struct ToolResult {
    pub ok: bool,
    pub content: Value,
    pub error_code: Option<String>,
    pub truncated: bool,
    pub metadata: Value,
}

impl ToolResult {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolObservationVisibility {
    Summary,
    ReferenceOnly,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolObservation {
    pub tool_call_id: String,
    pub tool_name: String,
    pub ok: bool,
    pub status: String,
    pub visibility: ToolObservationVisibility,
    pub content_preview: String,
    pub content_digest: String,
    pub result_ref: Option<String>,
    pub error_code: Option<String>,
    pub reference_ids: Vec<String>,
    pub observation_id: Option<String>,
    pub approval_request_id: Option<String>,
    pub truncated: bool,
    pub redacted: bool,
    #[serde(skip)]
    policy_decision_id: Option<String>,
    #[serde(skip)]
    approval_grant_id: Option<String>,
    #[serde(skip)]
    internal_metadata: Option<Value>,
}

impl ToolObservation {
    pub fn summary(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        ok: bool,
        content_preview: impl Into<String>,
        content_digest: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            ok,
            status: if ok { "ok" } else { "error" }.to_string(),
            visibility: ToolObservationVisibility::Summary,
            content_preview: content_preview.into(),
            content_digest: content_digest.into(),
            result_ref: None,
            error_code: None,
            reference_ids: Vec::new(),
            observation_id: None,
            approval_request_id: None,
            truncated: false,
            redacted: true,
            policy_decision_id: None,
            approval_grant_id: None,
            internal_metadata: None,
        }
    }

    pub fn with_internal_metadata(
        mut self,
        policy_decision_id: impl Into<String>,
        approval_grant_id: impl Into<String>,
        metadata: Value,
    ) -> Self {
        self.policy_decision_id = Some(policy_decision_id.into());
        self.approval_grant_id = Some(approval_grant_id.into());
        self.internal_metadata = Some(metadata);
        self
    }

    pub fn from_result(
        envelope: &ToolCallEnvelope,
        result: &ToolResult,
        visibility: ToolObservationVisibility,
    ) -> Self {
        let result_content = result.content.to_string();
        let (content_preview, preview_truncated) =
            bounded_text(&result_content, DEFAULT_OBSERVATION_PREVIEW_MAX_CHARS);
        Self {
            visibility,
            error_code: result.error_code.clone(),
            truncated: result.truncated || preview_truncated,
            ..Self::summary(
                envelope.tool_call_id.clone(),
                envelope.tool_name.clone(),
                result.ok,
                content_preview,
                "",
            )
        }
    }

    pub fn failed(
        envelope: &ToolCallEnvelope,
        error_code: impl Into<String>,
        content_preview: impl Into<String>,
    ) -> Self {
        Self {
            error_code: Some(error_code.into()),
            ..Self::summary(
                envelope.tool_call_id.clone(),
                envelope.tool_name.clone(),
                false,
                content_preview,
                "",
            )
        }
    }

    pub fn approval_required(
        envelope: &ToolCallEnvelope,
        approval_request_id: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            approval_request_id: Some(approval_request_id.into()),
            ..Self::failed(envelope, TOOL_APPROVAL_REQUIRED_ERROR, reason)
        }
    }

    pub fn to_model_payload(&self) -> Value {
        let content_preview = redact_model_visible_text(&self.content_preview);
        let mut payload = json!({
            "ok": self.ok,
            "tool_name": self.tool_name,
            "tool_call_id": self.tool_call_id,
            "status": self.status,
            "content_digest": self.content_digest,
            "result_ref": self.result_ref,
            "error_code": self.error_code,
            "reference_ids": self.reference_ids,
            "observation_id": self.observation_id,
            "truncated": self.truncated,
            "redacted": self.redacted,
        });
        if let Some(approval_request_id) = &self.approval_request_id {
            payload["approval_request_id"] = json!(approval_request_id);
        }
        if self.visibility != ToolObservationVisibility::ReferenceOnly {
            payload["content"] = json!(content_preview);
            payload["content_preview"] = json!(content_preview);
        }
        payload
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceToolError {
    OutsideWorkspace(String),
    ProtectedPath(String),
    BinaryPattern,
    ReadFailed(String),
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
            Self::BinaryPattern => write!(formatter, "grep pattern must be valid utf-8 text"),
            Self::ReadFailed(message) => write!(formatter, "workspace tool read failed: {message}"),
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

#[derive(Debug, Clone)]
pub struct WorkspaceTools {
    workspace_root: PathBuf,
}

impl WorkspaceTools {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
        }
    }

    pub fn read(&self, input: ReadToolInput) -> Result<ToolResult, WorkspaceToolError> {
        let target = self.resolve_workspace_path(&input.path, false)?;
        let bytes = std::fs::read(&target).map_err(io_error)?;
        let relative = self.relative_path(&target);
        if is_binary(&bytes) {
            return Ok(ToolResult::success(json!({
                "path": relative,
                "binary": true,
                "content_preview": BINARY_CONTENT_PREVIEW,
                "truncated": true,
                "artifact_ref": artifact_ref(RESULT_ARTIFACT_PREFIX, &relative),
            })));
        }
        let content = String::from_utf8(bytes).map_err(|error| {
            WorkspaceToolError::ReadFailed(format!("invalid utf-8 after binary check: {error}"))
        })?;
        let max_chars = input.max_chars.unwrap_or(DEFAULT_READ_MAX_CHARS);
        let (preview, truncated) = bounded_text(&content, max_chars);
        Ok(ToolResult::success(json!({
            "path": relative,
            "binary": false,
            "content_preview": preview,
            "truncated": truncated,
            "artifact_ref": if truncated || content.len() > LARGE_OUTPUT_ARTIFACT_THRESHOLD {
                Value::String(artifact_ref(RESULT_ARTIFACT_PREFIX, &relative))
            } else {
                Value::Null
            },
        })))
    }

    pub fn list(&self, input: ListToolInput) -> Result<ToolResult, WorkspaceToolError> {
        let target = self.resolve_workspace_path(input.path.as_deref().unwrap_or("."), true)?;
        let mut entries = Vec::new();
        let mut redacted_entries = 0usize;
        let max_entries = input.max_entries.unwrap_or(DEFAULT_LIST_MAX_ENTRIES);
        for entry in std::fs::read_dir(&target).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let path = entry.path();
            let relative = self.relative_path(&path);
            if is_protected_relative(&relative) {
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
        Ok(ToolResult::success(json!({
            "entries": entries,
            "redacted_entries": redacted_entries,
            "truncated": entries.len() >= max_entries,
        })))
    }

    pub fn grep(&self, input: GrepToolInput) -> Result<ToolResult, WorkspaceToolError> {
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
        Ok(ToolResult::success(json!({
            "matches": matches,
            "truncated": truncated,
        })))
    }

    pub fn edit(
        &self,
        input: EditToolInput,
        decision: &ToolBrokerDecision,
    ) -> Result<ToolResult, WorkspaceToolError> {
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
    ) -> Result<ToolResult, WorkspaceToolError> {
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
        for change in &patch.changes {
            let target =
                self.resolve_workspace_path(&change.path, decision.allows_protected_path())?;
            let relative = self.relative_path(&target);
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
            if let Err(error) = atomic_write(path, updated) {
                rollback_originals(&originals);
                return Err(error);
            }
        }
        let changed_files = prepared
            .iter()
            .map(|(_path, relative, _original, _updated)| relative.clone())
            .collect::<Vec<_>>();
        Ok(ToolResult::success(json!({
            "changed_files": changed_files,
            "diff_ref": artifact_ref(DIFF_ARTIFACT_PREFIX, &changed_files.join(",")),
            "rolled_back": false,
        })))
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
        if is_protected_relative(&relative) {
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
            && (is_protected_relative(&relative) || is_protected_relative(&intended_relative))
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

fn validate_tool_name(name: &str) -> Result<(), String> {
    let parts = name.split('.').collect::<Vec<_>>();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(format!("tool name has an empty namespace segment: {name}"));
    }
    match parts.as_slice() {
        ["builtin", _tool] => Ok(()),
        ["mcp", _server, _tool] => Ok(()),
        ["python", _plugin, _tool] => Ok(()),
        _ => Err(format!(
            "tool name must use builtin.*, mcp.<server>.<tool>, or python.<plugin>.<tool>: {name}"
        )),
    }
}

fn redact_model_visible_text(text: &str) -> String {
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

fn is_protected_relative(path: &str) -> bool {
    path.replace('\\', "/")
        .split('/')
        .map(str::to_ascii_lowercase)
        .any(|component| is_protected_component(&component))
}

fn is_protected_component(component: &str) -> bool {
    PROTECTED_PATH_EXACT_MARKERS
        .iter()
        .any(|marker| component == *marker)
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
    let temp_path = path.with_extension("tmp-write");
    std::fs::write(&temp_path, content).map_err(io_error)?;
    std::fs::rename(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        io_error(error)
    })
}

fn existing_text_or_empty(path: &Path) -> Result<String, WorkspaceToolError> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(io_error(error)),
    }
}

fn rollback_originals(originals: &[(PathBuf, String, String, bool)]) {
    for (path, _relative, original, existed) in originals {
        if *existed {
            let _ = std::fs::write(path, original);
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn io_error(error: std::io::Error) -> WorkspaceToolError {
    WorkspaceToolError::ReadFailed(error.to_string())
}
