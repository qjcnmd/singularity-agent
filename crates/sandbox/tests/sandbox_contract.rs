use schemars::schema_for;
use singularity_sandbox::{
    CommandRequest, CommandResult, SandboxBackend, SandboxBackendDescriptor, SandboxCapabilities,
    SandboxPolicy,
};

struct TestBackend;

impl SandboxBackend for TestBackend {
    fn name(&self) -> &'static str {
        "test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict()
    }
}

#[test]
fn sandbox_policy_and_backend_contract_are_serializable() {
    let policy = SandboxPolicy::isolated_verification("C:/repo");
    let value = serde_json::to_value(&policy).expect("serialize sandbox policy");

    assert_eq!(value["profile"], "isolated_verification");
    assert_eq!(value["network"]["mode"], "denied");
    assert_eq!(TestBackend.name(), "test");
    assert!(TestBackend.capabilities().network_isolation);
}

#[test]
fn command_request_and_result_are_schema_backed_boundaries() {
    let request = CommandRequest::project_verification(
        "command_1",
        vec!["python".to_string(), "-m".to_string(), "pytest".to_string()],
        ".",
        "C:/repo",
    );
    let result = CommandResult::completed("command_1", "passed");

    let request_value = serde_json::to_value(&request).expect("serialize command request");
    let result_value = serde_json::to_value(&result).expect("serialize command result");

    assert_eq!(request_value["purpose"], "project_verification");
    assert_eq!(request_value["network"]["mode"], "denied");
    assert_eq!(result_value["semantic_status"], "succeeded");
    assert_eq!(
        schema_for!(CommandRequest)
            .schema
            .metadata
            .unwrap()
            .title
            .unwrap(),
        "CommandRequest"
    );
    assert_eq!(
        schema_for!(CommandResult)
            .schema
            .metadata
            .unwrap()
            .title
            .unwrap(),
        "CommandResult"
    );
}

#[test]
fn sandbox_backend_descriptor_is_a_serializable_contract() {
    let descriptor = SandboxBackendDescriptor::strict("windows_elevated");
    let value = serde_json::to_value(&descriptor).expect("serialize backend descriptor");

    assert_eq!(value["backend"], "windows_elevated");
    assert_eq!(value["enforcement"], "strict");
    assert!(
        value["capabilities"]["filesystem_isolation"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(
        schema_for!(SandboxBackendDescriptor)
            .schema
            .metadata
            .unwrap()
            .title
            .unwrap(),
        "SandboxBackendDescriptor"
    );
}
