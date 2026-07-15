#![forbid(unsafe_code)]

//! Policy、approval、permission profile 和资源边界的统一决策模型。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const REASON_APPROVAL_POLICY_NEVER: &str = "approval policy forbids approval requests";
const REASON_MATCHED_PERMISSION_RULE: &str = "matched permission rule";
const REASON_NO_RULE: &str = "no permission rule matched; approval required";
const REASON_NETWORK_ACCESS_DENIED: &str = "network access is denied by the permission profile";
const REASON_PROTECTED_RESOURCE_DENIED: &str = "protected resource is denied by default";

/// 文件系统和 approval 的基础权限档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionProfileName {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

/// 当前会话是否允许网络访问。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NetworkAccess {
    Denied,
    Allowed,
}

/// 无匹配规则时对工具操作的 approval 策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    Untrusted,
    OnRequest,
    Never,
}

/// 配置规则的来源层级。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
/// 配置规则的来源优先级。
pub enum SettingsScope {
    Managed,
    User,
    Project,
    Local,
}

/// 受 Policy 控制的操作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOperation {
    Read,
    Write,
    Execute,
    Network,
}

/// Policy 对一次操作给出的最终结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecisionOutcome {
    Allow,
    Deny,
    Ask,
}

/// Agent/Policy 共同使用的文件系统、网络和 approval 范围。
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
    /// 创建 workspace-write 权限档位。
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

/// 待评估的工具、操作和资源请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub operation: PermissionOperation,
    pub resource: String,
    pub resource_sensitive: bool,
}

impl PermissionRequest {
    /// 创建待评估的 permission 请求。
    pub fn new(
        tool_name: impl Into<String>,
        operation: PermissionOperation,
        resource: impl Into<String>,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            operation,
            resource: resource.into(),
            resource_sensitive: false,
        }
    }

    /// 标记请求涉及敏感资源。
    pub fn with_sensitive_resource(mut self) -> Self {
        self.resource_sensitive = true;
        self
    }
}

/// 按 scope、操作和资源匹配的 Policy 规则。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionRule {
    pub rule_id: String,
    pub scope: SettingsScope,
    pub outcome: PermissionDecisionOutcome,
    pub operation: Option<PermissionOperation>,
    pub resource: Option<String>,
    pub resource_prefix: Option<String>,
}

impl PermissionRule {
    /// 创建匹配指定操作的规则。
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
            resource: None,
            resource_prefix: None,
        }
    }

    /// 限制规则适用的操作类型。
    pub fn for_operation(mut self, operation: PermissionOperation) -> Self {
        self.operation = Some(operation);
        self
    }

    /// 限制规则适用的精确资源。
    pub fn for_resource(mut self, resource: impl Into<String>) -> Self {
        self.resource = Some(resource.into());
        self.resource_prefix = None;
        self
    }

    /// 限制规则适用的资源前缀。
    pub fn for_resource_prefix(mut self, resource_prefix: impl Into<String>) -> Self {
        self.resource = None;
        self.resource_prefix = Some(resource_prefix.into());
        self
    }

    /// 判断规则是否匹配请求。
    pub fn matches(&self, request: &PermissionRequest) -> bool {
        self.operation
            .is_none_or(|operation| operation == request.operation)
            && self
                .resource
                .as_ref()
                .is_none_or(|resource| request.resource == *resource)
            && self
                .resource_prefix
                .as_ref()
                .is_none_or(|prefix| resource_matches_prefix(&request.resource, prefix))
    }
}

fn resource_matches_prefix(resource: &str, prefix: &str) -> bool {
    !prefix.is_empty()
        && (resource == prefix
            || resource
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with(['/', '\\'])))
}

/// 说明权限决定来源的稳定分类。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecisionCause {
    Explicit,
    Rule,
    Hook,
    NetworkProfile,
    ProtectedResource,
    NoMatchingRule,
    ApprovalPolicy,
}

/// Policy 对一次请求作出的结果及其原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionDecision {
    pub outcome: PermissionDecisionOutcome,
    pub cause: PermissionDecisionCause,
    pub reason: String,
    pub rule_id: Option<String>,
    pub scope: Option<SettingsScope>,
}

impl PermissionDecision {
    /// 创建最终 permission 决策。
    pub fn new(outcome: PermissionDecisionOutcome, reason: impl Into<String>) -> Self {
        Self {
            outcome,
            cause: PermissionDecisionCause::Explicit,
            reason: reason.into(),
            rule_id: None,
            scope: None,
        }
    }

    fn with_cause(mut self, cause: PermissionDecisionCause) -> Self {
        self.cause = cause;
        self
    }

    fn from_rule(rule: &PermissionRule) -> Self {
        Self {
            outcome: rule.outcome,
            cause: PermissionDecisionCause::Rule,
            reason: REASON_MATCHED_PERMISSION_RULE.to_string(),
            rule_id: Some(rule.rule_id.clone()),
            scope: Some(rule.scope),
        }
    }
}

/// 在 Policy 评估前可介入的工具 hook。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PreToolUseHook {
    pub hook_id: String,
    pub decision: PermissionDecision,
}

impl PreToolUseHook {
    /// 创建一个 pre-tool hook。
    pub fn new(hook_id: impl Into<String>, decision: PermissionDecision) -> Self {
        Self {
            hook_id: hook_id.into(),
            decision,
        }
    }
}

/// 聚合 profile、规则和 hooks 的 Policy evaluator。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PolicyEngine {
    pub profile: PermissionProfile,
    pub rules: Vec<PermissionRule>,
    pub hooks: Vec<PreToolUseHook>,
}

impl PolicyEngine {
    /// 创建使用指定 profile 的 PolicyEngine。
    pub fn new(profile: PermissionProfile) -> Self {
        Self {
            profile,
            rules: Vec::new(),
            hooks: Vec::new(),
        }
    }

    /// 添加 permission 规则。
    pub fn with_rule(mut self, rule: PermissionRule) -> Self {
        self.rules.push(rule);
        self
    }

    /// 添加 pre-tool hook。
    pub fn with_hook(mut self, mut hook: PreToolUseHook) -> Self {
        hook.decision.cause = PermissionDecisionCause::Hook;
        self.hooks.push(hook);
        self
    }

    /// 对请求执行规则、hook 和 approval 策略评估。
    pub fn evaluate(&self, request: &PermissionRequest) -> PermissionDecision {
        if request.operation == PermissionOperation::Network
            && self.profile.network_access == NetworkAccess::Denied
        {
            return PermissionDecision::new(
                PermissionDecisionOutcome::Deny,
                REASON_NETWORK_ACCESS_DENIED,
            )
            .with_cause(PermissionDecisionCause::NetworkProfile);
        }
        let hook_decision = self
            .hooks
            .iter()
            .find(|hook| !matches!(hook.decision.outcome, PermissionDecisionOutcome::Allow))
            .map(|hook| hook.decision.clone());

        if let Some(decision) = self.first_matching_rule(request, PermissionDecisionOutcome::Deny) {
            return decision;
        }
        if self.profile.protected_paths_enforced && request.resource_sensitive {
            return PermissionDecision::new(
                PermissionDecisionOutcome::Deny,
                REASON_PROTECTED_RESOURCE_DENIED,
            )
            .with_cause(PermissionDecisionCause::ProtectedResource);
        }
        if let Some(decision) = hook_decision {
            return self.apply_approval_policy(decision);
        }
        if let Some(decision) = self.first_matching_rule(request, PermissionDecisionOutcome::Ask) {
            return self.apply_approval_policy(decision);
        }
        if let Some(decision) = self.first_matching_rule(request, PermissionDecisionOutcome::Allow)
        {
            return decision;
        }
        self.apply_approval_policy(
            PermissionDecision::new(PermissionDecisionOutcome::Ask, REASON_NO_RULE)
                .with_cause(PermissionDecisionCause::NoMatchingRule),
        )
    }

    fn first_matching_rule(
        &self,
        request: &PermissionRequest,
        outcome: PermissionDecisionOutcome,
    ) -> Option<PermissionDecision> {
        self.rules
            .iter()
            .filter(|rule| rule.outcome == outcome && rule.matches(request))
            .min_by_key(|rule| rule.scope)
            .map(PermissionDecision::from_rule)
    }

    fn apply_approval_policy(&self, decision: PermissionDecision) -> PermissionDecision {
        if decision.outcome != PermissionDecisionOutcome::Ask {
            return decision;
        }
        match self.profile.approval_policy {
            ApprovalPolicy::Untrusted | ApprovalPolicy::OnRequest => decision,
            ApprovalPolicy::Never => PermissionDecision {
                outcome: PermissionDecisionOutcome::Deny,
                cause: PermissionDecisionCause::ApprovalPolicy,
                reason: REASON_APPROVAL_POLICY_NEVER.to_string(),
                rule_id: decision.rule_id,
                scope: decision.scope,
            },
        }
    }
}

/// approval 请求的持久化决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    Allow,
    Deny,
    Defer,
}

/// 绑定 thread、turn 和资源的 approval 请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub tool_call_id: Option<String>,
    pub action: String,
    #[serde(default)]
    pub resources: Vec<String>,
    pub reason: String,
}

impl ApprovalRequest {
    /// 创建绑定 thread/turn 的 approval 请求。
    pub fn new(
        request_id: impl Into<String>,
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            tool_call_id: None,
            action: action.into(),
            resources: Vec::new(),
            reason: String::new(),
        }
    }

    /// 设置 approval 关联资源。
    pub fn with_resources<I, S>(mut self, resources: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.resources = resources.into_iter().map(Into::into).collect();
        self
    }

    /// 绑定 tool call id。
    pub fn with_tool_call_id(mut self, tool_call_id: impl Into<String>) -> Self {
        self.tool_call_id = Some(tool_call_id.into());
        self
    }
}

/// approval 决定及其可审计原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalDecision {
    pub request_id: String,
    pub decision_id: String,
    pub outcome: ApprovalOutcome,
    pub reason: String,
}

impl ApprovalDecision {
    /// 创建 approval 决策记录。
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
