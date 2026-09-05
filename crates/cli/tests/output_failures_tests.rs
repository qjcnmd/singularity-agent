//! 终端输出通道写失败的边界处理测试。
//!
//! 输出通道故障属于投影层故障，不改变底层的执行事实与持久化 ledger 终态；
//! 但为防止下游机器解析方误判，summary 输出写失败时进程统一返回非零退出码，
//! 且绝不向输出流残留半行破损数据。

use std::sync::Arc;

use singularity_model::test_support::ScriptedProvider;
use singularity_runtime::objects::TurnStatus;

use crate::headless_support::{BufferedSink, FailOnSubstring, HeadlessFixture, session_records_at};
use crate::jsonl_mode::JsonlRenderer;
use crate::{HeadlessView, ProcessOutcome};

#[test]
fn summary_write_failure_never_looks_like_success() {
    let fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::ok("done".to_string())));
    let out = BufferedSink::default();
    let capture = out.clone();
    let view = HeadlessView::Json(JsonlRenderer::with_writer(
        Some(fixture.thread_id.clone()),
        FailOnSubstring::new(out, "{\"summary\""),
    ));
    let outcome =
        crate::execute_headless(Arc::clone(&fixture.conversation), "goal".to_string(), view);
    assert!(matches!(&outcome, ProcessOutcome::Output(_)), "{outcome:?}");
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

#[test]
fn event_write_failure_never_looks_like_success_even_when_summary_writes() {
    let fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::ok("done".to_string())));
    let out = BufferedSink::default();
    let capture = out.clone();
    let view = HeadlessView::Json(JsonlRenderer::with_writer(
        Some(fixture.thread_id.clone()),
        FailOnSubstring::new(out, "turn/started"),
    ));
    let outcome =
        crate::execute_headless(Arc::clone(&fixture.conversation), "goal".to_string(), view);
    assert!(matches!(&outcome, ProcessOutcome::Output(_)), "{outcome:?}");
    assert_ne!(outcome.finish().0, 0);
    assert!(capture.text().contains("summary"));
}
