#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::session::{SessionEntry, SessionManager};

#[test]
fn request_content_limit_stops_transport_without_retry_or_usage() {
    use singularity_model::{ModelMessage, ModelRole, test_support::ScriptedProvider};
    let dir = tempfile::tempdir().unwrap();
    let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
    let writer = Arc::new(std::sync::Mutex::new(session));
    let scripted = Arc::new(ScriptedProvider::ok("must not send"));
    let provider: Arc<dyn Provider + Send + Sync> = scripted.clone();
    // The content record itself exceeds the real session line budget.
    let mut request = ModelTurnRequest::new(
        "",
        vec![ModelMessage::text(
            ModelRole::User,
            "x".repeat(16 * 1024 * 1024),
        )],
    );
    let cancellation = CancellationToken::new();
    let mut accounting = RequestAccounting::default();
    let mut ledger = AttemptLedger::new(&writer, &mut accounting);
    let result = send_with_retry(
        |ledger, events| {
            stream_completion_once(
                &provider,
                &mut request,
                ledger,
                events,
                &cancellation,
                1,
                singularity_protocol::RequestPurpose::Generation,
            )
        },
        &mut ledger,
        TurnRetryPolicy {
            max_retries: 3,
            base_delay_ms: 0,
        },
        &mut AgentEvents::default(),
        &cancellation,
    );
    assert!(matches!(
        result,
        Err(RequestExecutionError::Session(
            SessionError::AppendLimitExceeded { .. }
        ))
    ));
    assert!(scripted.requests().is_empty());
    assert_eq!(accounting.attempts, 1);
    assert_eq!(accounting.usage, ModelUsage::default());
}

#[test]
fn recording_failure_stops_retries_and_preserves_storage_error_and_measured_usage() {
    use singularity_model::{
        ModelMessage, ModelRole,
        test_support::{ScriptedAttempt, ScriptedProvider},
    };
    use singularity_protocol::RequestPurpose;

    for purpose in [RequestPurpose::Generation, RequestPurpose::Compaction] {
        for fail_before_start in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
            let path = session.path().to_path_buf();
            let writer = Arc::new(std::sync::Mutex::new(session));
            let usage = ModelUsage {
                input_tokens: 5,
                output_tokens: 2,
                total_tokens: 7,
                usage_present: true,
                ..ModelUsage::default()
            };
            let scripted = Arc::new(ScriptedProvider::new([
                ScriptedAttempt::success_with_usage("response", usage.clone()),
            ]));
            let provider: Arc<dyn Provider + Send + Sync> = scripted.clone();
            let cancellation = CancellationToken::new();
            let mut request =
                ModelTurnRequest::new("", vec![ModelMessage::text(ModelRole::User, "hello")]);
            let mut accounting = RequestAccounting::default();
            let mut ledger = AttemptLedger::new(&writer, &mut accounting);
            let mut observed = Vec::new();
            if fail_before_start {
                std::fs::remove_file(&path).unwrap();
            }
            let mut sink = |event| {
                if let AgentEvent::ProviderAttempt { observation, .. } = &event
                    && observation.status == singularity_protocol::ProviderAttemptStatus::Started
                {
                    // Start is durable, but the finish record will fail.
                    std::fs::remove_file(&path).unwrap();
                }
                observed.push(event);
            };
            let mut events = AgentEvents {
                on_event: Some(&mut sink),
            };
            let result = send_with_retry(
                |ledger, events| {
                    stream_completion_once(
                        &provider,
                        &mut request,
                        ledger,
                        events,
                        &cancellation,
                        1,
                        purpose,
                    )
                },
                &mut ledger,
                TurnRetryPolicy {
                    max_retries: 3,
                    base_delay_ms: 0,
                },
                &mut events,
                &cancellation,
            );
            assert!(
                matches!(result, Err(RequestExecutionError::Session(SessionError::Io(error))) if error.kind() == std::io::ErrorKind::NotFound)
            );
            assert!(!cancellation.is_cancelled());
            assert_eq!(scripted.requests().len(), usize::from(!fail_before_start));
            assert_eq!(accounting.attempts, 1);
            assert_eq!(
                accounting.usage,
                if fail_before_start {
                    ModelUsage::default()
                } else {
                    usage
                }
            );
            assert!(accounting.complete);
            assert!(!observed.iter().any(|event| matches!(event, AgentEvent::ProviderAttempt { observation, .. } if observation.status != singularity_protocol::ProviderAttemptStatus::Started)));
            let streamed = observed
                .iter()
                .any(|event| matches!(event, AgentEvent::MessageUpdate { .. }));
            assert_eq!(
                streamed,
                !fail_before_start && purpose == RequestPurpose::Generation
            );
        }
    }
}

#[test]
fn failed_attempt_preserves_public_thinking_and_text_under_its_result_id_once() {
    let dir = tempfile::tempdir().expect("temp");
    let session =
        SessionManager::create(dir.path(), &dir.path().join("sessions")).expect("session");
    let writer = Arc::new(std::sync::Mutex::new(session));
    let mut attempts = RequestAccounting::default();
    let mut ledger = AttemptLedger::new(&writer, &mut attempts);
    ledger.begin();
    let id = ledger.result_entry_id().to_string();
    ledger
        .persist_visible_assistant("visible text", "visible thinking")
        .unwrap();
    ledger
        .persist_visible_assistant("duplicate", "duplicate")
        .unwrap();
    let writer = lock_writer(&writer);
    let records: Vec<_> = writer
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message {
                id: actual,
                message,
                ..
            } if actual == &id => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].content_text(), "visible text");
    assert!(
        matches!(&records[0].content()[0], ContentBlock::Thinking { thinking, .. } if thinking == "visible thinking")
    );
    assert!(records[0].provider_reasoning_replay().is_none());
}
