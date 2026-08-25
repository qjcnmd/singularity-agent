//! core 公共类型和 JSON-RPC 基础合同测试。

use singularity_core::{CancellationToken, ClientInfo, ErrorCode};

#[test]
fn client_metadata_round_trips_as_json() {
    let client = ClientInfo::new("singularity_cli", "Singularity CLI", "0.1.0");
    let value = serde_json::to_value(&client).expect("serialize client info");

    assert_eq!(value["name"], "singularity_cli");
    assert_eq!(value["title"], "Singularity CLI");
    assert_eq!(value["version"], "0.1.0");

    assert_eq!(ErrorCode::not_initialized().message(), "Not initialized");
}

#[test]
fn cloned_cancellation_tokens_share_one_monotonic_state() {
    let token = CancellationToken::new();
    let clone = token.clone();

    assert!(!token.is_cancelled());
    clone.cancel();
    assert!(token.is_cancelled());
}
