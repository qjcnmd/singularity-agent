use singularity_core::{
    CancellationToken, ClientInfo, ErrorCode, RequestId, Timestamp, contains_sensitive_text,
};

#[test]
fn client_metadata_and_ids_round_trip_as_json() {
    let client = ClientInfo::new("singularity_cli", "Singularity CLI", "0.1.0");
    let value = serde_json::to_value(&client).expect("serialize client info");

    assert_eq!(value["name"], "singularity_cli");
    assert_eq!(value["title"], "Singularity CLI");
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
    assert!(!contains_sensitive_text(
        "at async onImport.tracePromise.__proto__ (node:internal/modules/esm/loader:661:26)"
    ));
}

#[test]
fn cloned_cancellation_tokens_share_one_monotonic_state() {
    let token = CancellationToken::new();
    let clone = token.clone();

    assert!(!token.is_cancelled());
    clone.cancel();
    assert!(token.is_cancelled());
}
