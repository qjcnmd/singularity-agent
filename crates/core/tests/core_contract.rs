use singularity_core::{ClientInfo, ErrorCode, RequestId, Timestamp};

#[test]
fn client_metadata_and_ids_round_trip_as_json() {
    let client = ClientInfo::new("rust_cli", "Rust CLI", "0.1.0");
    let value = serde_json::to_value(&client).expect("serialize client info");

    assert_eq!(value["name"], "rust_cli");
    assert_eq!(value["title"], "Rust CLI");
    assert_eq!(value["version"], "0.1.0");

    let request_id = RequestId::from("request_1");
    assert_eq!(request_id.as_str(), "request_1");

    let timestamp = Timestamp::parse("2026-01-01T00:00:00Z").expect("parse timestamp");
    assert_eq!(timestamp.to_string(), "2026-01-01T00:00:00Z");
    assert_eq!(ErrorCode::not_initialized().message(), "Not initialized");
}
