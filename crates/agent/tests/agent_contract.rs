use singularity_agent::{AgentHostStatus, AgentLoopBridge};

#[test]
fn agent_boundary_reports_not_migrated_without_claiming_completion() {
    let bridge = AgentLoopBridge::not_migrated();

    assert_eq!(bridge.status, AgentHostStatus::NotMigrated);
    assert!(!bridge.completed);
    assert_eq!(bridge.status.as_str(), "not_migrated");
}
