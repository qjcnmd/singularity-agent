use singularity_core::{ClientInfo, ErrorCode, RequestId, Timestamp, contains_sensitive_text};

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

#[test]
fn sensitive_text_detects_common_secret_label_formats() {
    for text in [
        "X-API-Key: abcdefgh",
        "api-key=abcdefgh",
        "apikey=abcdefgh",
        "token: abcdefgh",
        "--api-key abcdefgh",
        "--token abcdefgh",
    ] {
        assert!(contains_sensitive_text(text), "{text} should be sensitive");
    }

    assert!(!contains_sensitive_text(
        "token count is 42 and token budget is 100"
    ));
}
