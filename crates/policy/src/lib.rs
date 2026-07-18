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
}

impl PermissionProfileName {
    /// 返回 SQLite 使用的稳定文本值。
    pub const fn as_storage_text(&self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
        }
    }

    /// 从 SQLite 的稳定文本值恢复权限档位；未知值返回 `None`。
    pub fn from_storage_text(value: &str) -> Option<Self> {
        match value {
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            _ => None,
        }
    }
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
    OnRequest,
    Never,
}

impl ApprovalPolicy {
    /// 返回 SQLite 使用的稳定文本值。
    pub const fn as_storage_text(&self) -> &'static str {
        match self {
            Self::OnRequest => "on-request",
            Self::Never => "never",
        }
    }

    /// 从 SQLite 的稳定文本值恢复 approval 策略；未知值返回 `None`。
    pub fn from_storage_text(value: &str) -> Option<Self> {
        match value {
            "on-request" => Some(Self::OnRequest),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
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

/// 注册表中稳定、可持久化的工具标识。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct ToolId(String);

impl ToolId {
    /// 校验并创建工具标识；名称语法与模型工具名的可移植子集一致。
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '/' | '.')
            })
        {
            return Err("tool id is not portable".to_string());
        }
        Ok(Self(value))
    }

    /// 返回注册表使用的稳定名称。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ToolId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// 已由工作区文件系统边界解析出的规范相对路径。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct WorkspaceRelativePath(String);

impl WorkspaceRelativePath {
    /// 从工作区边界已经规范化的 `/` 分隔相对路径创建类型化资源。
    pub fn from_canonical(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value == "." {
            return Ok(Self(value));
        }
        if value.is_empty()
            || value.starts_with('/')
            || value.ends_with('/')
            || value.contains('\\')
            || value.contains(':')
            || value
                .split('/')
                .any(|component| component.is_empty() || component == "." || component == "..")
        {
            return Err("workspace path is not canonical and relative".to_string());
        }
        Ok(Self(value))
    }

    /// 返回工作区文件系统认可的规范相对路径。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_within(&self, root: &Self) -> bool {
        root.0 == "."
            || self.0 == root.0
            || self
                .0
                .strip_prefix(&root.0)
                .is_some_and(|suffix| suffix.starts_with('/'))
    }
}

impl<'de> Deserialize<'de> for WorkspaceRelativePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_canonical(value).map_err(serde::de::Error::custom)
    }
}

/// 对规范命令执行范围计算的稳定摘要。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct CommandScopeDigest(String);

impl CommandScopeDigest {
    /// 校验并创建 `sha256:<hex>` 命令范围摘要。
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let Some(hex) = value.strip_prefix("sha256:") else {
            return Err("command scope digest must use sha256".to_string());
        };
        if hex.len() != 64 || !hex.chars().all(|character| character.is_ascii_hexdigit()) {
            return Err("command scope digest is invalid".to_string());
        }
        Ok(Self(format!("sha256:{}", hex.to_ascii_lowercase())))
    }

    /// 返回稳定摘要文本。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for CommandScopeDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Policy 可以比较但不能自行解析或规范化的类型化权限资源。
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PermissionResource {
    WorkspacePath(WorkspaceRelativePath),
    CommandScope(CommandScopeDigest),
    Tool(ToolId),
}

/// 规则只允许精确资源或类型化工作区子树，不再解释字符串前缀。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PermissionResourceSelector {
    Exact(PermissionResource),
    WorkspaceSubtree(WorkspaceRelativePath),
}

impl PermissionResourceSelector {
    fn matches(&self, resource: &PermissionResource) -> bool {
        match (self, resource) {
            (Self::Exact(expected), actual) => expected == actual,
            (Self::WorkspaceSubtree(root), PermissionResource::WorkspacePath(candidate)) => {
                candidate.is_within(root)
            }
            (Self::WorkspaceSubtree(_), _) => false,
        }
    }
}

/// Agent/Policy 共同使用的文件系统、网络和 approval 范围。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionProfile {
    pub profile: PermissionProfileName,
    pub network_access: NetworkAccess,
    pub approval_policy: ApprovalPolicy,
    pub protected_paths_enforced: bool,
}

impl PermissionProfile {
    /// 创建只读权限档位。
    pub fn read_only() -> Self {
        Self {
            profile: PermissionProfileName::ReadOnly,
            network_access: NetworkAccess::Denied,
            approval_policy: ApprovalPolicy::OnRequest,
            protected_paths_enforced: true,
        }
    }

    /// 创建 workspace-write 权限档位。
    pub fn workspace_write() -> Self {
        Self {
            profile: PermissionProfileName::WorkspaceWrite,
            network_access: NetworkAccess::Denied,
            approval_policy: ApprovalPolicy::OnRequest,
            protected_paths_enforced: true,
        }
    }
}

/// 待评估的工具、操作和资源请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionRequest {
    pub tool_id: ToolId,
    pub operation: PermissionOperation,
    pub resource: PermissionResource,
    pub resource_sensitive: bool,
}

impl PermissionRequest {
    /// 创建待评估的 permission 请求。
    pub fn new(
        tool_id: ToolId,
        operation: PermissionOperation,
        resource: PermissionResource,
    ) -> Self {
        Self {
            tool_id,
            operation,
            resource,
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
    pub resource: Option<PermissionResourceSelector>,
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
        }
    }

    /// 限制规则适用的操作类型。
    pub fn for_operation(mut self, operation: PermissionOperation) -> Self {
        self.operation = Some(operation);
        self
    }

    /// 限制规则适用的精确资源。
    pub fn for_resource(mut self, resource: PermissionResource) -> Self {
        self.resource = Some(PermissionResourceSelector::Exact(resource));
        self
    }

    /// 限制规则适用于一个已经规范化的工作区子树。
    pub fn for_workspace_subtree(mut self, root: WorkspaceRelativePath) -> Self {
        self.resource = Some(PermissionResourceSelector::WorkspaceSubtree(root));
        self
    }

    /// 判断规则是否匹配请求。
    pub fn matches(&self, request: &PermissionRequest) -> bool {
        self.operation
            .is_none_or(|operation| operation == request.operation)
            && self
                .resource
                .as_ref()
                .is_none_or(|resource| resource.matches(&request.resource))
    }
}

/// 说明权限决定来源的稳定分类。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecisionCause {
    Explicit,
    Rule,
    FilesystemProfile,
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

/// 聚合 profile 与规则的 Policy evaluator。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PolicyEngine {
    pub profile: PermissionProfile,
    pub rules: Vec<PermissionRule>,
}

impl PolicyEngine {
    /// 创建使用指定 profile 的 PolicyEngine。
    pub fn new(profile: PermissionProfile) -> Self {
        Self {
            profile,
            rules: Vec::new(),
        }
    }

    /// 添加 permission 规则。
    pub fn with_rule(mut self, rule: PermissionRule) -> Self {
        self.rules.push(rule);
        self
    }

    /// 对请求执行 profile、规则和 approval 策略评估。
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

        if request.operation == PermissionOperation::Write {
            if self.profile.profile == PermissionProfileName::ReadOnly {
                return PermissionDecision::new(
                    PermissionDecisionOutcome::Deny,
                    "write access is denied by the read-only profile",
                )
                .with_cause(PermissionDecisionCause::FilesystemProfile);
            }
            if !matches!(request.resource, PermissionResource::WorkspacePath(_)) {
                return PermissionDecision::new(
                    PermissionDecisionOutcome::Deny,
                    "write access requires a workspace path resource",
                )
                .with_cause(PermissionDecisionCause::FilesystemProfile);
            }
        }

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
            ApprovalPolicy::OnRequest => decision,
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

impl ApprovalOutcome {
    /// 返回 SQLite 使用的稳定文本值。
    pub const fn as_storage_text(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Defer => "defer",
        }
    }

    /// 从 SQLite 的稳定文本值恢复 approval 结果；未知值返回 `None`。
    pub fn from_storage_text(value: &str) -> Option<Self> {
        match value {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            "defer" => Some(Self::Defer),
            _ => None,
        }
    }
}

/// 绑定 thread、turn 和资源的 approval 请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub tool_call_id: Option<String>,
    pub action: ToolId,
    #[serde(default)]
    pub resources: Vec<PermissionResource>,
    pub reason: String,
}

impl ApprovalRequest {
    /// 创建绑定 thread/turn 的 approval 请求。
    pub fn new(
        request_id: impl Into<String>,
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        action: ToolId,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            tool_call_id: None,
            action,
            resources: Vec::new(),
            reason: String::new(),
        }
    }

    /// 设置 approval 关联资源。
    pub fn with_resources<I>(mut self, resources: I) -> Self
    where
        I: IntoIterator<Item = PermissionResource>,
    {
        self.resources = resources.into_iter().collect();
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
