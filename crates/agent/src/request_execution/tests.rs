#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::events::AgentEvent;
use crate::message::ContentBlock;
use crate::session::{SessionEntry, SessionManager};
use singularity_core::CancellationToken;
use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};
use singularity_model::{ModelTurnRequest, ModelUsage};
use std::sync::Arc;

#[test]
fn failed_attempt_preserves_public_thinking_and_text_under_its_result_id_once() {
    let dir = tempfile::tempdir().expect("temp");
    let session =
        SessionManager::create(dir.path(), &dir.path().join("sessions")).expect("session");
    let writer = Arc::new(std::sync::Mutex::new(session));
    let mut attempts = RequestAccounting::default();
    let mut ledger = AttemptLedger::new(&writer, &mut attempts);
    // 构造即有效：不再需要 begin，身份与本次 attempt 计数已就位。
    let id = ledger.result_entry_id().to_string();
    ledger
        .persist_visible_assistant("visible text", "visible thinking")
        .unwrap();
    ledger
        .persist_visible_assistant("duplicate", "duplicate")
        .unwrap();
    drop(ledger);
    // 相邻 attempt 身份不同，累计计数随构造递增。
    let next_id = AttemptLedger::new(&writer, &mut attempts)
        .result_entry_id()
        .to_string();
    assert_eq!(attempts.attempts, 2);
    assert_ne!(next_id, id);
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

/// Started 是唯一携带 request head 的观测；一次 Finished 只记账一次，其已知用量
/// 进入本次 accounting。（失败响应保留已知用量的用例见 agent::request::tests。）
#[test]
fn attempt_observations_put_the_request_head_on_started_and_account_each_finish_once() {
    use singularity_protocol::{ProviderAttemptStatus, RequestPurpose};

    let dir = tempfile::tempdir().expect("temp");
    let session =
        SessionManager::create(dir.path(), &dir.path().join("sessions")).expect("session");
    let writer = Arc::new(std::sync::Mutex::new(session));
    let usage = ModelUsage {
        input_tokens: 5,
        output_tokens: 2,
        total_tokens: 7,
        usage_present: true,
        ..ModelUsage::default()
    };
    let provider =
        ScriptedProvider::new([ScriptedAttempt::success_with_usage("answer", usage.clone())]);
    let mut request = ModelTurnRequest::new(String::new(), Vec::new());
    let mut accounting = RequestAccounting::default();
    let mut observed = Vec::new();
    let mut sink = |event| {
        if let AgentEvent::ProviderAttempt { observation, .. } = event {
            observed.push(observation);
        }
    };
    execute_request(
        &provider,
        &writer,
        &mut accounting,
        &mut request,
        &mut sink,
        &CancellationToken::new(),
        1,
        RequestPurpose::Generation,
    )
    .expect("scripted success");

    let statuses: Vec<_> = observed
        .iter()
        .map(|observation| observation.status)
        .collect();
    assert_eq!(
        statuses,
        [ProviderAttemptStatus::Started, ProviderAttemptStatus::Ok]
    );
    let heads: Vec<_> = observed
        .iter()
        .map(|observation| observation.request_head.is_some())
        .collect();
    assert_eq!(
        heads,
        [true, false],
        "the request head belongs to the Started observation only"
    );
    assert_eq!(accounting.attempts, 1);
    assert_eq!(
        accounting.usage, usage,
        "one Finished accounts its usage once"
    );
    assert!(accounting.complete);
}
