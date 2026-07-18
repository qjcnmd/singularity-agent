//! Policy rule、approval policy 和 permission decision 的行为测试。

use singularity_policy::{
    ApprovalDecision, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, CommandScopeDigest,
    NetworkAccess, PermissionDecisionCause, PermissionDecisionOutcome, PermissionOperation,
    PermissionProfile, PermissionProfileName, PermissionRequest, PermissionResource,
    PermissionRule, PolicyEngine, SettingsScope, ToolId, WorkspaceRelativePath,
};

fn tool(value: &str) -> ToolId {
    ToolId::new(value).expect("valid tool id")
}

fn workspace_path(value: &str) -> PermissionResource {
    PermissionResource::WorkspacePath(
        WorkspaceRelativePath::from_canonical(value).expect("canonical workspace path"),
    )
}

fn command_scope(hex: char) -> PermissionResource {
    PermissionResource::CommandScope(
        CommandScopeDigest::new(format!("sha256:{}", hex.to_string().repeat(64)))
            .expect("valid command digest"),
    )
}

fn tool_resource(value: &str) -> PermissionResource {
    PermissionResource::Tool(tool(value))
}

#[test]
fn typed_permission_resources_revalidate_untrusted_json() {
    assert!(serde_json::from_str::<ToolId>(r#""not a tool id""#).is_err());
    assert!(serde_json::from_str::<WorkspaceRelativePath>(r#""../secret""#).is_err());
    assert!(serde_json::from_str::<CommandScopeDigest>(r#""sha256:short""#).is_err());
    assert!(
        serde_json::from_value::<PermissionResource>(serde_json::json!({
            "kind": "workspace_path",
            "value": "src/../secret"
        }))
        .is_err()
    );

    let digest = format!("sha256:{}", "A".repeat(64));
    let parsed = serde_json::from_value::<PermissionResource>(serde_json::json!({
        "kind": "command_scope",
        "value": digest
    }))
    .expect("valid command scope");
    assert_eq!(parsed, command_scope('a'));
}

fn request(
    tool_id: &str,
    operation: PermissionOperation,
    resource: PermissionResource,
) -> PermissionRequest {
    PermissionRequest::new(tool(tool_id), operation, resource)
}

fn rule(
    id: &str,
    scope: SettingsScope,
    outcome: PermissionDecisionOutcome,
    operation: PermissionOperation,
    resource: PermissionResource,
) -> PermissionRule {
    PermissionRule::new(id, scope, outcome)
        .for_operation(operation)
        .for_resource(resource)
}

#[test]
fn permission_profile_and_approval_objects_keep_wire_names() {
    let profile = PermissionProfile::workspace_write();
    let value = serde_json::to_value(&profile).expect("serialize permission profile");

    assert_eq!(value["profile"], "workspace-write");
    assert!(value.get("workspace_roots").is_none());

    let request = ApprovalRequest::new("approval_1", "thread_1", "turn_1", tool("write_file"));
    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Deny,
        "protected path",
    );

    assert_eq!(profile.profile, PermissionProfileName::WorkspaceWrite);
    assert_eq!(decision.outcome, ApprovalOutcome::Deny);
    assert_eq!(decision.reason, "protected path");
}

#[test]
fn storage_enum_text_is_stable_and_rejects_unknown_values() {
    assert_eq!(
        PermissionProfileName::WorkspaceWrite.as_storage_text(),
        "workspace-write"
    );
    assert_eq!(ApprovalPolicy::OnRequest.as_storage_text(), "on-request");
    assert_eq!(ApprovalOutcome::Allow.as_storage_text(), "allow");
    assert_eq!(
        PermissionProfileName::from_storage_text("read-only"),
        Some(PermissionProfileName::ReadOnly)
    );
    assert_eq!(ApprovalPolicy::from_storage_text("unknown"), None);
    assert_eq!(ApprovalOutcome::from_storage_text("deferred"), None);
}

#[test]
fn workspace_subtree_rules_match_only_the_named_path_tree() {
    let rule = PermissionRule::new(
        "allow_src_tree",
        SettingsScope::Project,
        PermissionDecisionOutcome::Allow,
    )
    .for_operation(PermissionOperation::Write)
    .for_workspace_subtree(
        WorkspaceRelativePath::from_canonical("src").expect("canonical subtree"),
    );

    for resource in ["src", "src/lib.rs"] {
        assert!(rule.matches(&request(
            "edit",
            PermissionOperation::Write,
            workspace_path(resource),
        )));
    }
    assert!(!rule.matches(&request(
        "edit",
        PermissionOperation::Write,
        workspace_path("src2/lib.rs"),
    )));
    assert!(WorkspaceRelativePath::from_canonical("src\\lib.rs").is_err());
}

#[test]
fn denied_profile_network_cannot_be_enabled_by_permission_rule() {
    let profile = PermissionProfile::workspace_write();
    let decision = PolicyEngine::new(profile)
        .with_rule(rule(
            "allow_network",
            SettingsScope::Managed,
            PermissionDecisionOutcome::Allow,
            PermissionOperation::Network,
            command_scope('a'),
        ))
        .evaluate(&request(
            "command",
            PermissionOperation::Network,
            command_scope('a'),
        ));

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Deny);
    assert_eq!(decision.cause, PermissionDecisionCause::NetworkProfile);
    assert_eq!(decision.rule_id, None);
    assert_eq!(
        decision.reason,
        "network access is denied by the permission profile"
    );
}

#[test]
fn allowed_profile_network_still_requires_a_matching_rule() {
    let mut profile = PermissionProfile::workspace_write();
    profile.network_access = NetworkAccess::Allowed;
    let engine = PolicyEngine::new(profile).with_rule(rule(
        "allow_network",
        SettingsScope::Project,
        PermissionDecisionOutcome::Allow,
        PermissionOperation::Network,
        command_scope('a'),
    ));

    let allowed = engine.evaluate(&request(
        "command",
        PermissionOperation::Network,
        command_scope('a'),
    ));
    let unmatched = engine.evaluate(&request(
        "command",
        PermissionOperation::Network,
        command_scope('b'),
    ));

    assert_eq!(allowed.outcome, PermissionDecisionOutcome::Allow);
    assert_eq!(unmatched.outcome, PermissionDecisionOutcome::Ask);
}

#[test]
fn policy_engine_evaluates_rules_in_fail_closed_order() {
    let request = request("shell", PermissionOperation::Execute, command_scope('a'));
    let engine = PolicyEngine::new(PermissionProfile::workspace_write())
        .with_rule(rule(
            "allow_test",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
            PermissionOperation::Execute,
            command_scope('a'),
        ))
        .with_rule(rule(
            "deny_test",
            SettingsScope::User,
            PermissionDecisionOutcome::Deny,
            PermissionOperation::Execute,
            command_scope('a'),
        ));

    let decision = engine.evaluate(&request);

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Deny);
    assert_eq!(decision.cause, PermissionDecisionCause::Rule);
    assert_eq!(decision.rule_id.as_deref(), Some("deny_test"));
}

#[test]
fn managed_policy_precedence_wins_over_lower_scope_rules() {
    let request = request(
        "read",
        PermissionOperation::Read,
        workspace_path("README.md"),
    );
    let engine = PolicyEngine::new(PermissionProfile::workspace_write())
        .with_rule(rule(
            "local_deny",
            SettingsScope::Local,
            PermissionDecisionOutcome::Deny,
            PermissionOperation::Read,
            workspace_path("README.md"),
        ))
        .with_rule(rule(
            "managed_deny",
            SettingsScope::Managed,
            PermissionDecisionOutcome::Deny,
            PermissionOperation::Read,
            workspace_path("README.md"),
        ));

    let decision = engine.evaluate(&request);

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Deny);
    assert_eq!(decision.cause, PermissionDecisionCause::Rule);
    assert_eq!(decision.rule_id.as_deref(), Some("managed_deny"));
    assert_eq!(decision.scope, Some(SettingsScope::Managed));
}

#[test]
fn sensitive_resources_are_denied_when_marked_by_caller() {
    let request = request("read", PermissionOperation::Read, workspace_path(".env"))
        .with_sensitive_resource();

    let decision = PolicyEngine::new(PermissionProfile::workspace_write()).evaluate(&request);

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Deny);
    assert_eq!(decision.cause, PermissionDecisionCause::ProtectedResource);
    assert_eq!(decision.reason, "protected resource is denied by default");
}

#[test]
fn explicit_ask_rule_creates_approval_flow() {
    let request = request("shell", PermissionOperation::Execute, command_scope('a'));
    let decision = PolicyEngine::new(PermissionProfile::workspace_write())
        .with_rule(rule(
            "ask_tests",
            SettingsScope::Project,
            PermissionDecisionOutcome::Ask,
            PermissionOperation::Execute,
            command_scope('a'),
        ))
        .evaluate(&request);

    let approval =
        ApprovalRequest::new("approval_1", "thread_1", "turn_1", request.tool_id.clone());
    let approved = ApprovalDecision::new("approval_1", ApprovalOutcome::Allow, "operator approved");

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Ask);
    assert_eq!(approval.action.as_str(), "shell");
    assert_eq!(approved.outcome, ApprovalOutcome::Allow);
}

#[test]
fn approval_policy_never_turns_approval_requests_into_deny() {
    let mut profile = PermissionProfile::workspace_write();
    profile.approval_policy = ApprovalPolicy::Never;

    let decision = PolicyEngine::new(profile)
        .with_rule(rule(
            "ask_tests",
            SettingsScope::Project,
            PermissionDecisionOutcome::Ask,
            PermissionOperation::Execute,
            command_scope('a'),
        ))
        .evaluate(&request(
            "shell",
            PermissionOperation::Execute,
            command_scope('a'),
        ));

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Deny);
    assert_eq!(decision.cause, PermissionDecisionCause::ApprovalPolicy);
    assert_eq!(decision.rule_id.as_deref(), Some("ask_tests"));
    assert_eq!(decision.reason, "approval policy forbids approval requests");
}

#[test]
fn read_only_profile_hard_denies_write_even_with_an_allow_rule() {
    let profile = PermissionProfile::read_only();

    let decision = PolicyEngine::new(profile).evaluate(&request(
        "edit",
        PermissionOperation::Write,
        workspace_path("README.md"),
    ));

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Deny);
    assert_eq!(decision.cause, PermissionDecisionCause::FilesystemProfile);
    assert_eq!(
        decision.reason,
        "write access is denied by the read-only profile"
    );
}

#[test]
fn absolute_resources_are_rejected_before_policy_evaluation() {
    assert!(WorkspaceRelativePath::from_canonical("C:/other/README.md").is_err());
    assert!(WorkspaceRelativePath::from_canonical("/other/README.md").is_err());
}

#[test]
fn unmatched_permission_requests_require_approval() {
    let decision = PolicyEngine::new(PermissionProfile::workspace_write()).evaluate(&request(
        "git",
        PermissionOperation::Execute,
        tool_resource("git"),
    ));

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Ask);
    assert_eq!(decision.rule_id, None);
}

#[test]
fn distinct_typed_command_scopes_are_not_a_policy_special_case() {
    let engine = PolicyEngine::new(PermissionProfile::workspace_write()).with_rule(rule(
        "deny_cargo_test",
        SettingsScope::Managed,
        PermissionDecisionOutcome::Deny,
        PermissionOperation::Execute,
        command_scope('a'),
    ));

    let other_scope = request("shell", PermissionOperation::Execute, command_scope('b'));
    let exact_scope = request("shell", PermissionOperation::Execute, command_scope('a'));

    assert_eq!(
        engine.evaluate(&other_scope).outcome,
        PermissionDecisionOutcome::Ask
    );
    assert_eq!(
        engine.evaluate(&exact_scope).outcome,
        PermissionDecisionOutcome::Deny
    );
}
