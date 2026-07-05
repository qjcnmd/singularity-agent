#![forbid(unsafe_code)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionProfileName {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NetworkAccess {
    Denied,
    Allowed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    OnRequest,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionProfile {
    pub profile: PermissionProfileName,
    pub workspace_roots: Vec<String>,
    pub additional_writable_directories: Vec<String>,
    pub network_access: NetworkAccess,
    pub approval_policy: ApprovalPolicy,
    pub protected_paths_enforced: bool,
}

impl PermissionProfile {
    pub fn workspace_write(workspace_root: impl Into<String>) -> Self {
        Self {
            profile: PermissionProfileName::WorkspaceWrite,
            workspace_roots: vec![workspace_root.into()],
            additional_writable_directories: Vec::new(),
            network_access: NetworkAccess::Denied,
            approval_policy: ApprovalPolicy::OnRequest,
            protected_paths_enforced: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    Allow,
    Deny,
    Defer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub session_id: String,
    pub task_id: String,
    pub action: String,
    pub reason: String,
}

impl ApprovalRequest {
    pub fn new(
        request_id: impl Into<String>,
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            session_id: session_id.into(),
            task_id: task_id.into(),
            action: action.into(),
            reason: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalDecision {
    pub request_id: String,
    pub decision_id: String,
    pub outcome: ApprovalOutcome,
    pub reason: String,
}

impl ApprovalDecision {
    pub fn new(
        request_id: impl Into<String>,
        outcome: ApprovalOutcome,
        reason: impl Into<String>,
    ) -> Self {
        let request_id = request_id.into();
        Self {
            decision_id: format!("{request_id}_decision"),
            request_id,
            outcome,
            reason: reason.into(),
        }
    }
}
