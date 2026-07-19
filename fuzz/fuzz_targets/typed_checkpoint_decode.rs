#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    let Some(request_value) = value.get("request") else {
        return;
    };
    let Some(payload) = value.get("checkpoint") else {
        return;
    };
    let Ok(request) = serde_json::from_value::<singularity_policy::ApprovalRequest>(
        request_value.clone(),
    ) else {
        return;
    };
    let _ = singularity_agent::PendingApprovalOccurrence::from_checkpoint_payload(request, payload);
});
