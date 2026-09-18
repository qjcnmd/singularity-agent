//! 终端输出通道写失败的边界处理测试。
//!
//! 输出通道故障属于投影层故障，不改变底层的执行事实与持久化 ledger 终态；
//! 但一旦 stdout 写入失败，该通道不再能确认行边界，渲染器停止后续写入
//! （包括 summary），进程统一返回非零退出码并报告输出故障。

use std::sync::Arc;

use singularity_model::test_support::ScriptedProvider;
use singularity_protocol::TurnStatus;

use super::support::{
    BufferedSink, FailOnSubstring, HeadlessFixture, PartialWriteThenFail, session_records_at,
};
use crate::ProcessOutcome;
use crate::jsonl_mode::JsonlRenderer;

#[test]
fn summary_write_failure_never_looks_like_success() {
    let fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::ok("done".to_string())));
    let out = BufferedSink::default();
    let capture = out.clone();
    let renderer = JsonlRenderer::with_writer(
        Some(fixture.thread_id.clone()),
        FailOnSubstring::new(out, "{\"summary\""),
    );
    let outcome = crate::execute_headless(&fixture.conversation, "goal", renderer);
    assert!(
        matches!(&outcome, ProcessOutcome::Failed(message)
        if message.contains("simulated stdout failure")),
        "{outcome:?}"
    );
    assert_ne!(
        outcome.finish().0,
        0,
        "an output failure is never a success exit"
    );
    let written = capture.text();
    assert!(
        written.contains("\"method\":\"turn/completed\""),
        "the event projection itself completed normally: {written}"
    );
    assert!(
        !written.contains("\"summary\""),
        "the failed summary line was not partially written"
    );
    assert!(
        session_records_at(&fixture.session_path())
            .iter()
            .any(|record| {
                matches!(
                    record,
                    singularity_agent::session::LedgerRecord::OperationFinished {
                        outcome: TurnStatus::Completed,
                        ..
                    }
                )
            }),
        "execution facts are untouched by projection failure"
    );
}

/// 事件行写失败（未写出字节）：同一输出通道不再写任何后续行，包括 summary。
#[test]
fn event_write_failure_stops_the_channel_before_the_summary() {
    let fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::ok("done".to_string())));
    let out = BufferedSink::default();
    let capture = out.clone();
    let renderer = JsonlRenderer::with_writer(
        Some(fixture.thread_id.clone()),
        FailOnSubstring::new(out, "turn/started"),
    );
    let outcome = crate::execute_headless(&fixture.conversation, "goal", renderer);
    assert!(
        matches!(&outcome, ProcessOutcome::Failed(message)
        if message.contains("simulated stdout failure")),
        "{outcome:?}"
    );
    assert_ne!(outcome.finish().0, 0);
    let written = capture.text();
    assert!(
        written.contains("\"method\":\"turn/controlChanged\""),
        "events before the failure were written: {written}"
    );
    assert!(
        !written.contains("turn/started") && !written.contains("\"summary\""),
        "no line is appended to the failed output channel: {written}"
    );
}

/// 事件行先写出部分字节再失败：流上已可能残留半行，后续 summary 不能再追加，
/// 否则会把第二个 JSON 接在半行之后。通道随后恢复可写也不改变这一点。
#[test]
fn partial_event_write_failure_never_appends_a_second_json_line() {
    /// 半行长度：足够短以确认没有整行写出，又足够长以确认真实短写形状。
    const PARTIAL_BYTES: usize = 24;

    let fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::ok("done".to_string())));
    let out = BufferedSink::default();
    let capture = out.clone();
    let renderer = JsonlRenderer::with_writer(
        Some(fixture.thread_id.clone()),
        PartialWriteThenFail::new(out, PARTIAL_BYTES),
    );
    let outcome = crate::execute_headless(&fixture.conversation, "goal", renderer);
    assert!(
        matches!(&outcome, ProcessOutcome::Failed(message)
        if message.contains("simulated stdout failure")),
        "{outcome:?}"
    );
    assert_ne!(outcome.finish().0, 0);
    let written = capture.text();
    assert_eq!(
        written.len(),
        PARTIAL_BYTES,
        "nothing is appended after the failed write: {written}"
    );
    assert!(
        !written.contains('\n') && !written.contains("\"summary\""),
        "the partial line is not completed by a summary: {written}"
    );
    assert!(
        session_records_at(&fixture.session_path())
            .iter()
            .any(|record| {
                matches!(
                    record,
                    singularity_agent::session::LedgerRecord::OperationFinished {
                        outcome: TurnStatus::Completed,
                        ..
                    }
                )
            }),
        "execution facts are untouched by projection failure"
    );
}
