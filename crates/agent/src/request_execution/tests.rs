#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::session::{SessionEntry, SessionManager};

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
