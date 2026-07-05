use schemars::schema_for;
use singularity_policy::{
    ApprovalDecision, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, NetworkAccess,
    PermissionDecision, PermissionDecisionOutcome, PermissionOperation, PermissionProfile,
    PermissionProfileName, PermissionRequest, PermissionRule, PolicyEngine, PreToolUseHook,
    SettingsScope,
};

#[test]
fn permission_and_approval_objects_use_python_wire_names() {
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
    let profile = PermissionProfile::workspace_write("C:/repo");
    let request = PermissionRequest::new(
        "builtin.shell",
        PermissionOperation::Execute,
        "C:/repo/scripts/test.ps1",
    );
    let engine = PolicyEngine::new(profile)
        .with_rule(
            PermissionRule::new(
                "allow_execute",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Execute)
            .for_resource("C:/repo"),
        )
        .with_rule(
            PermissionRule::new(
                "deny_execute",
                SettingsScope::User,
                PermissionDecisionOutcome::Deny,
            )
            .for_operation(PermissionOperation::Execute)
            .for_resource("C:/repo/scripts"),
        );

    let decision = engine.evaluate(&request);

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Deny);
    assert_eq!(decision.rule_id.as_deref(), Some("deny_execute"));

    let hook_decision = PermissionDecision::new(
        PermissionDecisionOutcome::Ask,
        "pre tool-use hook requires approval",
    );
    let hooked = engine.with_hook(PreToolUseHook::new("hook_1", hook_decision.clone()));

    assert_eq!(hooked.evaluate(&request), hook_decision);
}

#[test]
fn managed_policy_precedence_wins_over_user_project_and_local_rules() {
    let request = PermissionRequest::new(
        "builtin.read",
        PermissionOperation::Read,
        "C:/repo/README.md",
    );
    let engine = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo"))
        .with_rule(
            PermissionRule::new(
                "local_deny",
                SettingsScope::Local,
                PermissionDecisionOutcome::Deny,
            )
            .for_operation(PermissionOperation::Read)
            .for_resource("C:/repo"),
        )
        .with_rule(
            PermissionRule::new(
                "managed_deny",
                SettingsScope::Managed,
                PermissionDecisionOutcome::Deny,
            )
            .for_operation(PermissionOperation::Read)
            .for_resource("C:/repo"),
        );

    let decision = engine.evaluate(&request);

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Deny);
    assert_eq!(decision.rule_id.as_deref(), Some("managed_deny"));
    assert_eq!(decision.scope, Some(SettingsScope::Managed));
}

#[test]
fn workspace_write_allows_scoped_writes_and_asks_outside_scope() {
    let engine = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo"));

    let inside = engine.evaluate(&PermissionRequest::new(
        "builtin.patch",
        PermissionOperation::Write,
        "C:/repo/src/lib.rs",
    ));
    let outside = engine.evaluate(&PermissionRequest::new(
        "builtin.patch",
        PermissionOperation::Write,
        "C:/outside/file.rs",
    ));

    assert_eq!(inside.outcome, PermissionDecisionOutcome::Allow);
    assert_eq!(outside.outcome, PermissionDecisionOutcome::Ask);
}

#[test]
fn protected_paths_are_denied_by_default_even_inside_workspace() {
    let engine = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo"));
    let request = PermissionRequest::new("builtin.read", PermissionOperation::Read, "C:/repo/.env");

    let decision = engine.evaluate(&request);

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Deny);
    assert_eq!(decision.reason, "protected path is denied by default");
}

#[test]
fn ask_rule_creates_approval_request_and_decision_uses_expected_outcomes() {
    let request = PermissionRequest::new(
        "builtin.shell",
        PermissionOperation::Execute,
        "C:/repo/scripts/build.ps1",
    );
    let decision = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo"))
        .with_rule(
            PermissionRule::new(
                "ask_shell",
                SettingsScope::Project,
                PermissionDecisionOutcome::Ask,
            )
            .for_operation(PermissionOperation::Execute)
            .for_resource("C:/repo/scripts"),
        )
        .evaluate(&request);

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Ask);

    let approval = ApprovalRequest::new("approval_1", "session_1", "task_1", request.tool_name);
    let approved = ApprovalDecision::new("approval_1", ApprovalOutcome::Allow, "operator approved");
    let deferred = ApprovalDecision::new("approval_1", ApprovalOutcome::Defer, "decide later");

    assert_eq!(approval.action, "builtin.shell");
    assert_eq!(approved.outcome, ApprovalOutcome::Allow);
    assert_eq!(deferred.outcome, ApprovalOutcome::Defer);
}

#[test]
fn network_access_is_separate_and_denied_by_profile_default() {
    let denied = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")).evaluate(
        &PermissionRequest::new("builtin.shell", PermissionOperation::Network, "example.com"),
    );

    assert_eq!(denied.outcome, PermissionDecisionOutcome::Deny);
    assert_eq!(denied.reason, "network access is denied by profile");

    let mut profile = PermissionProfile::workspace_write("C:/repo");
    profile.network_access = NetworkAccess::Allowed;
    let allowed = PolicyEngine::new(profile)
        .with_rule(
            PermissionRule::new(
                "allow_network",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Network)
            .for_resource("example.com"),
        )
        .evaluate(&PermissionRequest::new(
            "builtin.shell",
            PermissionOperation::Network,
            "example.com",
        ));

    assert_eq!(allowed.outcome, PermissionDecisionOutcome::Allow);
    assert_eq!(allowed.rule_id.as_deref(), Some("allow_network"));
}

#[test]
fn approval_policy_never_turns_approval_requests_into_deny() {
    let mut profile = PermissionProfile::workspace_write("C:/repo");
    profile.approval_policy = ApprovalPolicy::Never;
    let decision = PolicyEngine::new(profile)
        .with_rule(
            PermissionRule::new(
                "ask_shell",
                SettingsScope::Project,
                PermissionDecisionOutcome::Ask,
            )
            .for_operation(PermissionOperation::Execute)
            .for_resource("C:/repo/scripts/build.ps1"),
        )
        .evaluate(&PermissionRequest::new(
            "builtin.shell",
            PermissionOperation::Execute,
            "C:/repo/scripts/build.ps1",
        ));

    assert_eq!(decision.outcome, PermissionDecisionOutcome::Deny);
    assert_eq!(decision.rule_id.as_deref(), Some("ask_shell"));
    assert_eq!(decision.reason, "approval policy forbids approval requests");
}

#[test]
fn permission_decision_objects_are_schema_backed_and_round_trip() {
    let request = PermissionRequest::new(
        "builtin.patch",
        PermissionOperation::Write,
        "C:/repo/src/lib.rs",
    );
    let rule = PermissionRule::new(
        "allow_write",
        SettingsScope::Project,
        PermissionDecisionOutcome::Allow,
    )
    .for_operation(PermissionOperation::Write)
    .for_resource("C:/repo");
    let decision = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo"))
        .with_rule(rule.clone())
        .evaluate(&request);
    let hook = PreToolUseHook::new("hook_allow", decision.clone());

    let restored_request: PermissionRequest =
        serde_json::from_value(serde_json::to_value(&request).expect("serialize request"))
            .expect("deserialize request");
    let restored_rule: PermissionRule =
        serde_json::from_value(serde_json::to_value(&rule).expect("serialize rule"))
            .expect("deserialize rule");
    let restored_decision: PermissionDecision =
        serde_json::from_value(serde_json::to_value(&decision).expect("serialize decision"))
            .expect("deserialize decision");
    let restored_hook: PreToolUseHook =
        serde_json::from_value(serde_json::to_value(&hook).expect("serialize hook"))
            .expect("deserialize hook");

    assert_eq!(restored_request, request);
    assert_eq!(restored_rule, rule);
    assert_eq!(restored_decision, decision);
    assert_eq!(restored_hook, hook);
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
