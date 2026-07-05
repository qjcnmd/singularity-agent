#![forbid(unsafe_code)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const DEFAULT_PROTECTED_PATTERNS: [&str; 8] = [
    ".env",
    ".env.",
    ".ssh",
    "id_ed25519",
    "id_rsa",
    "credential",
    "secret",
    "token",
];
const REASON_APPROVAL_POLICY_NEVER: &str = "approval policy forbids approval requests";
const REASON_MATCHED_PERMISSION_RULE: &str = "matched permission rule";
const REASON_NETWORK_DENIED: &str = "network access is denied by profile";
const REASON_PERMISSION_MODE_ALLOWS: &str = "permission mode allows";
const REASON_PROTECTED_PATH_DENIED: &str = "protected path is denied by default";
const REASON_READ_ONLY_ALLOWS_READ: &str = "read-only mode allows read";
const REASON_READ_ONLY_REQUIRES_APPROVAL: &str = "read-only mode requires approval";
const REASON_SCOPED_WRITE_ALLOWED: &str = "workspace-write mode allows scoped write";
const REASON_UNSCOPED_WRITE_REQUIRES_APPROVAL: &str = "write outside workspace requires approval";
const REASON_PERMISSION_MODE_DEFERS: &str = "permission mode defers to rules or approval";
const REASON_NO_ALLOW_RULE: &str = "no allow rule matched; approval required";
const REASON_WORKSPACE_WRITE_ALLOWS_READ: &str = "workspace-write mode allows read";

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
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    ReadOnly,
    WorkspaceWrite,
    BypassPermissions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SettingsScope {
    Managed,
    User,
    Project,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOperation {
    Read,
    Write,
    Execute,
    Network,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecisionOutcome {
    Allow,
    Deny,
    Ask,
    Defer,
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

    pub fn permission_mode(&self) -> PermissionMode {
        match self.profile {
            PermissionProfileName::ReadOnly => PermissionMode::ReadOnly,
            PermissionProfileName::WorkspaceWrite => PermissionMode::WorkspaceWrite,
            PermissionProfileName::DangerFullAccess => PermissionMode::BypassPermissions,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub operation: PermissionOperation,
    pub resource: String,
}

impl PermissionRequest {
    pub fn new(
        tool_name: impl Into<String>,
        operation: PermissionOperation,
        resource: impl Into<String>,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            operation,
            resource: normalize_resource(resource.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionRule {
    pub rule_id: String,
    pub scope: SettingsScope,
    pub outcome: PermissionDecisionOutcome,
    pub operation: Option<PermissionOperation>,
    pub resource_pattern: Option<String>,
}

impl PermissionRule {
    pub fn new(
        rule_id: impl Into<String>,
        scope: SettingsScope,
        outcome: PermissionDecisionOutcome,
    ) -> Self {
        Self {
            rule_id: rule_id.into(),
            scope,
            outcome,
            operation: None,
            resource_pattern: None,
        }
    }

    pub fn for_operation(mut self, operation: PermissionOperation) -> Self {
        self.operation = Some(operation);
        self
    }

    pub fn for_resource(mut self, pattern: impl Into<String>) -> Self {
        self.resource_pattern = Some(normalize_resource(pattern.into()));
        self
    }

    pub fn matches(&self, request: &PermissionRequest) -> bool {
        self.operation
            .as_ref()
            .is_none_or(|operation| operation == &request.operation)
            && self
                .resource_pattern
                .as_ref()
                .is_none_or(|pattern| resource_matches(&request.resource, pattern))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionDecision {
    pub outcome: PermissionDecisionOutcome,
    pub reason: String,
    pub rule_id: Option<String>,
    pub scope: Option<SettingsScope>,
}

impl PermissionDecision {
    pub fn new(outcome: PermissionDecisionOutcome, reason: impl Into<String>) -> Self {
        Self {
            outcome,
            reason: reason.into(),
            rule_id: None,
            scope: None,
        }
    }

    fn from_rule(rule: &PermissionRule) -> Self {
        Self {
            outcome: rule.outcome.clone(),
            reason: REASON_MATCHED_PERMISSION_RULE.to_string(),
            rule_id: Some(rule.rule_id.clone()),
            scope: Some(rule.scope.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PreToolUseHook {
    pub hook_id: String,
    pub decision: PermissionDecision,
}

impl PreToolUseHook {
    pub fn new(hook_id: impl Into<String>, decision: PermissionDecision) -> Self {
        Self {
            hook_id: hook_id.into(),
            decision,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PolicyEngine {
    pub profile: PermissionProfile,
    pub rules: Vec<PermissionRule>,
    pub hooks: Vec<PreToolUseHook>,
}

impl PolicyEngine {
    pub fn new(profile: PermissionProfile) -> Self {
        Self {
            profile,
            rules: Vec::new(),
            hooks: Vec::new(),
        }
    }

    pub fn with_rule(mut self, rule: PermissionRule) -> Self {
        self.rules.push(rule);
        self
    }

    pub fn with_hook(mut self, hook: PreToolUseHook) -> Self {
        self.hooks.push(hook);
        self
    }

    pub fn evaluate(&self, request: &PermissionRequest) -> PermissionDecision {
        if let Some(hook) = self.hooks.iter().find(|hook| {
            !matches!(
                hook.decision.outcome,
                PermissionDecisionOutcome::Allow | PermissionDecisionOutcome::Defer
            )
        }) {
            return hook.decision.clone();
        }
        if let Some(decision) = self.first_matching_rule(request, PermissionDecisionOutcome::Deny) {
            return decision;
        }
        if self.profile.protected_paths_enforced
            && matches!(
                request.operation,
                PermissionOperation::Read | PermissionOperation::Write
            )
            && protected_resource(&request.resource)
        {
            return PermissionDecision::new(
                PermissionDecisionOutcome::Deny,
                REASON_PROTECTED_PATH_DENIED,
            );
        }
        if matches!(request.operation, PermissionOperation::Network)
            && matches!(self.profile.network_access, NetworkAccess::Denied)
        {
            return PermissionDecision::new(PermissionDecisionOutcome::Deny, REASON_NETWORK_DENIED);
        }
        if let Some(decision) = self.first_matching_rule(request, PermissionDecisionOutcome::Defer)
        {
            return decision;
        }
        if let Some(decision) = self.first_matching_rule(request, PermissionDecisionOutcome::Ask) {
            return self.apply_approval_policy(decision);
        }
        let mode_decision = self.evaluate_permission_mode(request);
        if !matches!(mode_decision.outcome, PermissionDecisionOutcome::Defer) {
            return self.apply_approval_policy(mode_decision);
        }
        if let Some(decision) = self.first_matching_rule(request, PermissionDecisionOutcome::Allow)
        {
            return decision;
        }
        self.approval_or_deny(REASON_NO_ALLOW_RULE)
    }

    fn first_matching_rule(
        &self,
        request: &PermissionRequest,
        outcome: PermissionDecisionOutcome,
    ) -> Option<PermissionDecision> {
        let mut rules = self
            .rules
            .iter()
            .filter(|rule| rule.outcome == outcome && rule.matches(request))
            .collect::<Vec<_>>();
        rules.sort_by_key(|rule| scope_precedence(&rule.scope));
        rules.into_iter().next().map(PermissionDecision::from_rule)
    }

    fn evaluate_permission_mode(&self, request: &PermissionRequest) -> PermissionDecision {
        match self.profile.permission_mode() {
            PermissionMode::BypassPermissions => PermissionDecision::new(
                PermissionDecisionOutcome::Allow,
                REASON_PERMISSION_MODE_ALLOWS,
            ),
            PermissionMode::ReadOnly => {
                if matches!(request.operation, PermissionOperation::Read) {
                    PermissionDecision::new(
                        PermissionDecisionOutcome::Allow,
                        REASON_READ_ONLY_ALLOWS_READ,
                    )
                } else {
                    self.approval_or_deny(REASON_READ_ONLY_REQUIRES_APPROVAL)
                }
            }
            PermissionMode::WorkspaceWrite => match request.operation {
                PermissionOperation::Read => PermissionDecision::new(
                    PermissionDecisionOutcome::Allow,
                    REASON_WORKSPACE_WRITE_ALLOWS_READ,
                ),
                PermissionOperation::Write => {
                    if self.resource_in_writable_scope(&request.resource) {
                        PermissionDecision::new(
                            PermissionDecisionOutcome::Allow,
                            REASON_SCOPED_WRITE_ALLOWED,
                        )
                    } else {
                        self.approval_or_deny(REASON_UNSCOPED_WRITE_REQUIRES_APPROVAL)
                    }
                }
                PermissionOperation::Execute | PermissionOperation::Network => {
                    PermissionDecision::new(
                        PermissionDecisionOutcome::Defer,
                        REASON_PERMISSION_MODE_DEFERS,
                    )
                }
            },
        }
    }

    fn apply_approval_policy(&self, decision: PermissionDecision) -> PermissionDecision {
        if !matches!(decision.outcome, PermissionDecisionOutcome::Ask)
            || matches!(self.profile.approval_policy, ApprovalPolicy::OnRequest)
        {
            return decision;
        }
        PermissionDecision {
            outcome: PermissionDecisionOutcome::Deny,
            reason: REASON_APPROVAL_POLICY_NEVER.to_string(),
            rule_id: decision.rule_id,
            scope: decision.scope,
        }
    }

    fn approval_or_deny(&self, reason: &'static str) -> PermissionDecision {
        self.apply_approval_policy(PermissionDecision::new(
            PermissionDecisionOutcome::Ask,
            reason,
        ))
    }

    fn resource_in_writable_scope(&self, resource: &str) -> bool {
        self.profile
            .workspace_roots
            .iter()
            .chain(self.profile.additional_writable_directories.iter())
            .map(normalize_resource)
            .any(|root| resource_matches(resource, &root))
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

fn scope_precedence(scope: &SettingsScope) -> u8 {
    match scope {
        SettingsScope::Managed => 0,
        SettingsScope::User => 1,
        SettingsScope::Project => 2,
        SettingsScope::Local => 3,
    }
}

fn normalize_resource(value: impl AsRef<str>) -> String {
    value.as_ref().replace('\\', "/").to_ascii_lowercase()
}

fn resource_matches(resource: &str, pattern: &str) -> bool {
    resource == pattern || resource.starts_with(&format!("{}/", pattern.trim_end_matches('/')))
}

fn protected_resource(resource: &str) -> bool {
    DEFAULT_PROTECTED_PATTERNS
        .iter()
        .any(|pattern| resource.contains(pattern))
}
