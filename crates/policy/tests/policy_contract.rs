use singularity_policy::{
    ApprovalDecision, ApprovalOutcome, ApprovalRequest, PermissionProfile, PermissionProfileName,
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
