use schemars::schema_for;
use singularity_policy::{
    ApprovalDecision, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, PermissionDecision,
    PermissionDecisionOutcome, PermissionOperation, PermissionProfile, PermissionProfileName,
    PermissionRequest, PermissionRule, PolicyEngine, PreToolUseHook, SettingsScope,
};

fn rule(
    id: &str,
    scope: SettingsScope,
    outcome: PermissionDecisionOutcome,
    operation: PermissionOperation,
    resource: &str,
) -> PermissionRule {
    PermissionRule::new(id, scope, outcome)
        .for_operation(operation)
        .for_resource(resource)
}

#[test]
fn permission_profile_and_approval_objects_keep_wire_names() {
    let profile = PermissionProfile::workspace_write("C:/repo");
    let value = serde_json::to_value(&profile).expect("serialize permission profile");

    assert_eq!(value["profile"], "workspace-write");
    assert_eq!(value["workspace_roots"][0], "C:/repo");

    let request = ApprovalRequest::new("approval_1", "session_1", "task_1", "write_file");
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
fn policy_engine_evaluates_hooks_and_rules_in_fail_closed_order() {
    let request = PermissionRequest::new(
        "builtin.shell",
        PermissionOperation::Execute,
        "python -m pytest",
    );
    let engine = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo"))
        .with_rule(rule(
            "allow_test",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
            PermissionOperation::Execute,
            "python -m pytest",
        ))
        .with_rule(rule(
            "deny_test",
            SettingsScope::User,
            PermissionDecisionOutcome::Deny,
            PermissionOperation::Execute,
            "python -m pytest",
        ));

    let decision = engine.evaluate(&request);

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Deny);
    assert_eq!(decision.rule_id.as_deref(), Some("deny_test"));

    let hook_decision = PermissionDecision::new(
        PermissionDecisionOutcome::Ask,
        "pre tool-use hook requires approval",
    );
    let hooked = engine.with_hook(PreToolUseHook::new("hook_1", hook_decision.clone()));

    assert_eq!(hooked.evaluate(&request), hook_decision);
}

#[test]
fn managed_policy_precedence_wins_over_lower_scope_rules() {
    let request = PermissionRequest::new("builtin.read", PermissionOperation::Read, "README.md");
    let engine = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo"))
        .with_rule(rule(
            "local_deny",
            SettingsScope::Local,
            PermissionDecisionOutcome::Deny,
            PermissionOperation::Read,
            "README.md",
        ))
        .with_rule(rule(
            "managed_deny",
            SettingsScope::Managed,
            PermissionDecisionOutcome::Deny,
            PermissionOperation::Read,
            "README.md",
        ));

    let decision = engine.evaluate(&request);

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Deny);
    assert_eq!(decision.rule_id.as_deref(), Some("managed_deny"));
    assert_eq!(decision.scope, Some(SettingsScope::Managed));
}

#[test]
fn protected_resources_are_denied_by_default() {
    let request = PermissionRequest::new("builtin.read", PermissionOperation::Read, ".env");

    let decision =
        PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")).evaluate(&request);

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Deny);
    assert_eq!(decision.reason, "protected resource is denied by default");
}

#[test]
fn explicit_ask_rule_creates_approval_flow() {
    let request = PermissionRequest::new(
        "builtin.shell",
        PermissionOperation::Execute,
        "python -m pytest",
    );
    let decision = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo"))
        .with_rule(rule(
            "ask_tests",
            SettingsScope::Project,
            PermissionDecisionOutcome::Ask,
            PermissionOperation::Execute,
            "python -m pytest",
        ))
        .evaluate(&request);

    let approval = ApprovalRequest::new("approval_1", "session_1", "task_1", request.tool_name);
    let approved = ApprovalDecision::new("approval_1", ApprovalOutcome::Allow, "operator approved");
    let deferred = ApprovalDecision::new("approval_1", ApprovalOutcome::Defer, "decide later");

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Ask);
    assert_eq!(approval.action, "builtin.shell");
    assert_eq!(approved.outcome, ApprovalOutcome::Allow);
    assert_eq!(deferred.outcome, ApprovalOutcome::Defer);
}

#[test]
fn approval_policy_never_turns_approval_requests_into_deny() {
    let mut profile = PermissionProfile::workspace_write("C:/repo");
    profile.approval_policy = ApprovalPolicy::Never;

    let decision = PolicyEngine::new(profile)
        .with_rule(rule(
            "ask_tests",
            SettingsScope::Project,
            PermissionDecisionOutcome::Ask,
            PermissionOperation::Execute,
            "python -m pytest",
        ))
        .evaluate(&PermissionRequest::new(
            "builtin.shell",
            PermissionOperation::Execute,
            "python -m pytest",
        ));

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Deny);
    assert_eq!(decision.rule_id.as_deref(), Some("ask_tests"));
    assert_eq!(decision.reason, "approval policy forbids approval requests");
}

#[test]
fn unmatched_permission_requests_require_approval() {
    let decision = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")).evaluate(
        &PermissionRequest::new("builtin.git", PermissionOperation::Execute, "git status"),
    );

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Ask);
    assert_eq!(decision.rule_id, None);
}

#[test]
fn permission_schema_objects_round_trip() {
    let request = PermissionRequest::new("builtin.patch", PermissionOperation::Write, "src/lib.rs");
    let rule = rule(
        "allow_write",
        SettingsScope::Project,
        PermissionDecisionOutcome::Allow,
        PermissionOperation::Write,
        "src/lib.rs",
    );
    let decision = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo"))
        .with_rule(rule.clone())
        .evaluate(&request);
    let hook = PreToolUseHook::new("hook_allow", decision.clone());

    assert_eq!(
        serde_json::from_value::<PermissionRequest>(
            serde_json::to_value(&request).expect("serialize request")
        )
        .expect("deserialize request"),
        request
    );
    assert_eq!(
        serde_json::from_value::<PermissionRule>(
            serde_json::to_value(&rule).expect("serialize rule")
        )
        .expect("deserialize rule"),
        rule
    );
    assert_eq!(
        serde_json::from_value::<PermissionDecision>(
            serde_json::to_value(&decision).expect("serialize decision")
        )
        .expect("deserialize decision"),
        decision
    );
    assert_eq!(
        serde_json::from_value::<PreToolUseHook>(
            serde_json::to_value(&hook).expect("serialize hook")
        )
        .expect("deserialize hook"),
        hook
    );
    assert_eq!(
        schema_for!(PermissionRequest)
            .schema
            .metadata
            .expect("request schema metadata")
            .title
            .expect("request schema title"),
        "PermissionRequest"
    );
    assert_eq!(
        schema_for!(PermissionDecision)
            .schema
            .metadata
            .expect("decision schema metadata")
            .title
            .expect("decision schema title"),
        "PermissionDecision"
    );
}
