#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! T028 [US2]：TUI、print 与 JSON 适配器共享同一执行路径。
//!
//! 同一目标、同一脚本 provider、同一 workspace 初始状态下运行三种入口，
//! 比较工具动作顺序、ledger 终态事实与 usage/失败原因：差异只允许存在于
//! 渲染面。JSON 面逐行解析 `{"method","params"}` envelope 并以恰好一条
//! summary 收尾；失败面（回合失败与准备失败）同样三面收敛（contracts/
//! entrypoints.md、contracts/turn-events.md）。中断收敛归 conversation_tests
//! 所有，此处不重复。

use std::sync::Arc;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};
use singularity_runtime::TurnOutcome;
use singularity_runtime::events::TurnEvent;
use singularity_runtime::objects::{TurnModelUsage, TurnStatus};

use super::app::{Phase, TuiApp};
use super::commands::Action;
use crate::headless_support::{BufferedSink, HeadlessFixture, session_records};
use crate::jsonl_mode::JsonlRenderer;
use crate::print_mode::PrintRenderer;
use crate::{HeadlessView, ProcessOutcome};

/// 与 `tui.rs` 事件循环同构的整帧渲染观察面。
fn render_text(app: &mut TuiApp) -> String {
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| app.draw(frame))
        .expect("frame renders from events alone");
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<Vec<_>>()
        .join("")
}

fn journey_script() -> Vec<ScriptedAttempt> {
    let usage = singularity_model::ModelUsage {
        input_tokens: 10,
        output_tokens: 5,
        total_tokens: 15,
        ..Default::default()
    };
    vec![
        ScriptedAttempt::tool_call("c1", "read", json!({"path": "notes.txt"})),
        ScriptedAttempt::tool_call(
            "c2",
            "edit",
            json!({"path": "notes.txt", "oldString": "alpha", "newString": "beta"}),
        ),
        ScriptedAttempt::tool_call("c3", "read", json!({"path": "notes.txt"})),
        ScriptedAttempt::success_with_usage("task complete", usage),
    ]
}

/// 一次 JSON 面运行的解析结果。
struct JsonRunOutput {
    pub outcome: ProcessOutcome,
    pub events: Vec<(String, Value)>,
    pub summaries: Vec<Value>,
}

fn run_json(fixture: &HeadlessFixture, goal: &str) -> JsonRunOutput {
    let out = BufferedSink::default();
    let capture = out.clone();
    let view = HeadlessView::Json(JsonlRenderer::with_writer(
        Some(fixture.thread_id.clone()),
        out,
    ));
    let outcome =
        crate::execute_headless(Arc::clone(&fixture.conversation), goal.to_string(), view);
    let mut events = Vec::new();
    let mut summaries = Vec::new();
    for line in capture.text().lines() {
        let value: Value = serde_json::from_str(line).expect("every stdout line is JSON");
        if let Some(summary) = value.get("summary") {
            summaries.push(summary.clone());
        } else {
            let method = value["method"]
                .as_str()
                .expect("event envelope carries a method");
            assert!(
                value.get("params").is_some(),
                "event envelope carries typed params: {value}"
            );
            events.push((method.to_string(), value["params"].clone()));
        }
    }
    JsonRunOutput {
        outcome,
        events,
        summaries,
    }
}

fn run_print(fixture: &HeadlessFixture, goal: &str) -> (ProcessOutcome, String) {
    let out = BufferedSink::default();
    let capture = out.clone();
    let err = BufferedSink::default();
    let view = HeadlessView::Print(PrintRenderer::with_writers(out, err));
    let outcome =
        crate::execute_headless(Arc::clone(&fixture.conversation), goal.to_string(), view);
    (outcome, capture.text())
}

/// TUI 面：与 `tui.rs` 事件循环同构的驱动（真实 `Conversation.run_turn`）。
fn run_tui(fixture: &HeadlessFixture, goal: &str) -> (String, TurnOutcome) {
    let mut app = TuiApp::new(Arc::clone(&fixture.conversation));
    app.editor.set_text(goal);
    let Action::Submit(submitted) = app.handle_key_at(
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ),
        std::time::Instant::now(),
    ) else {
        panic!("Enter on a non-empty draft must submit");
    };
    assert_eq!(submitted, goal);
    assert_eq!(app.phase, Phase::Running);
    let conversation = Arc::clone(&app.conversation);
    let mut sink = |event: TurnEvent| app.on_turn_event(&event);
    let outcome = conversation
        .run_turn(goal, &mut sink)
        .expect("trusted terminal outcome");
    app.on_chain_finished(&Ok(()));
    (render_text(&mut app), outcome)
}

fn durable_terminal(fixture: &HeadlessFixture) -> (TurnStatus, TurnModelUsage) {
    let terminals: Vec<_> = session_records(fixture)
        .iter()
        .filter_map(|record| match record {
            singularity_agent::session::LedgerRecord::OperationFinished {
                outcome,
                usage: Some(usage),
                ..
            } => Some((*outcome, usage.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(terminals.len(), 1, "exactly one durable terminal outcome");
    terminals[0].clone()
}

fn durable_tool_order(fixture: &HeadlessFixture) -> Vec<String> {
    crate::headless_support::session_entries(fixture)
        .iter()
        .filter_map(|entry| match entry {
            singularity_agent::session::SessionEntry::Message { message, .. }
                if message.role() == singularity_agent::message::AgentMessageRole::ToolResult =>
            {
                message.tool_call_id().cloned()
            }
            _ => None,
        })
        .collect()
}

/// 成功旅程主路径：三种入口的工具动作顺序、ledger 终态与 usage 完全一致；
/// print stdout 只含最终文本；JSON stdout 逐行可解析、以恰好一条 completed
/// summary 收尾；modify 步骤真实改写 workspace 文件。
#[test]
fn success_journey_is_identical_across_tui_print_and_json() {
    let goal = "read, modify and validate notes.txt";

    let json_fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::new(journey_script())));
    let json_output = run_json(&json_fixture, goal);
    assert_eq!(json_output.outcome, ProcessOutcome::Completed);
    assert_eq!(
        json_output.summaries.len(),
        1,
        "exactly one terminal summary line"
    );
    let summary_turn = &json_output.summaries[0]["turn"];
    assert_eq!(summary_turn["status"], json!("completed"));
    assert_eq!(summary_turn["threadId"], json!(json_fixture.thread_id));
    assert_eq!(summary_turn["usage"]["totalTokens"], json!(15));
    assert_eq!(json_output.events.last().unwrap().0, "turn/completed");

    let print_fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::new(journey_script())));
    let (print_outcome, print_stdout) = run_print(&print_fixture, goal);
    assert_eq!(print_outcome, ProcessOutcome::Completed);
    assert_eq!(
        print_stdout, "task complete\n",
        "stdout is only the final text"
    );

    let tui_fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::new(journey_script())));
    let (tui_frame, tui_outcome) = run_tui(&tui_fixture, goal);
    assert_eq!(tui_outcome.turn_status, TurnStatus::Completed);
    assert!(tui_frame.contains("task complete"), "rendered: {tui_frame}");
    assert!(tui_frame.contains("✔ completed"));
    assert!(
        tui_frame.contains("edit") && tui_frame.contains("notes.txt"),
        "tool progress is projected from events alone: {tui_frame}"
    );

    // 同一执行事实：工具动作顺序与终态 usage 三面一致，效果真实发生。
    let json_order = durable_tool_order(&json_fixture);
    assert_eq!(json_order, vec!["c1", "c2", "c3"]);
    assert_eq!(durable_tool_order(&print_fixture), json_order);
    assert_eq!(durable_tool_order(&tui_fixture), json_order);
    let (json_status, json_usage) = durable_terminal(&json_fixture);
    assert_eq!(
        durable_terminal(&print_fixture),
        (json_status, json_usage.clone())
    );
    assert_eq!(
        durable_terminal(&tui_fixture),
        (json_status, json_usage.clone())
    );
    assert_eq!(json_status, TurnStatus::Completed);
    assert_eq!(json_usage.total_tokens, 15);
    assert_eq!(print_fixture.read_file("notes.txt"), "beta\n");
}

/// 失败路径收敛：provider 硬失败与准备阶段失败在三面共享同一终态类别——
/// 事件流可不同，但 summary 恰好一条、cause/状态一致、进程结果精确；
/// 准备失败不留下任何 turn 痕迹。
#[test]
fn failures_converge_to_one_summary_across_entrypoints() {
    let goal = "doomed task";
    let failure = || {
        ScriptedAttempt::failure_kind(singularity_model::ModelErrorKind::AuthError, "key rejected")
    };

    let json_fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::new([failure()])));
    let json_output = run_json(&json_fixture, goal);
    assert!(
        matches!(&json_output.outcome, ProcessOutcome::TurnFailed(message)
            if message.contains("provider_auth") && message.contains("key rejected")),
        "failed turn keeps the provider cause: {:?}",
        json_output.outcome
    );
    assert_eq!(json_output.outcome.finish().0, 1);
    assert_eq!(json_output.summaries.len(), 1);
    assert_eq!(json_output.summaries[0]["turn"]["status"], json!("failed"));
    assert_eq!(
        json_output.summaries[0]["turn"]["threadId"],
        json!(json_fixture.thread_id)
    );
    let error_events: Vec<_> = json_output
        .events
        .iter()
        .filter(|(method, _)| method == "turn/error")
        .collect();
    assert_eq!(error_events.len(), 1, "one turn/error event");
    assert_eq!(error_events[0].1["error"]["cause"], json!("provider_auth"));

    let print_fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::new([failure()])));
    let (print_outcome, print_stdout) = run_print(&print_fixture, goal);
    assert!(matches!(&print_outcome, ProcessOutcome::TurnFailed(message)
        if message.contains("key rejected")));
    assert_eq!(print_stdout, "", "a failed turn writes nothing to stdout");

    let tui_fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::new([failure()])));
    let (tui_frame, tui_outcome) = run_tui(&tui_fixture, goal);
    assert_eq!(tui_outcome.turn_status, TurnStatus::Failed);
    assert!(tui_frame.contains("✖ failed"), "rendered: {tui_frame}");

    for fixture in [&json_fixture, &print_fixture, &tui_fixture] {
        let (status, _) = durable_terminal(fixture);
        assert_eq!(
            status,
            TurnStatus::Failed,
            "same durable terminal across entrypoints"
        );
    }

    // 准备阶段失败：无 operation 痕迹，但 summary 仍恰好一条且诚实。
    let mut prep_fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::ok("never runs")));
    prep_fixture.remove_workspace(); // thread cwd 不可解析 → runner 装配前失败。
    let prep_output = run_json(&prep_fixture, "unreachable workspace");
    assert!(
        matches!(&prep_output.outcome, ProcessOutcome::Preparation(_)),
        "{:?}",
        prep_output.outcome
    );
    assert_eq!(prep_output.summaries.len(), 1);
    let summary = &prep_output.summaries[0];
    assert_eq!(summary["turn"]["status"], json!("failed"));
    assert_eq!(
        summary["turn"]["usage"],
        Value::Null,
        "usage is unknown, not faked"
    );
    assert!(
        prep_output.events.is_empty(),
        "no turn events were ever started"
    );
    assert!(
        session_records(&prep_fixture).is_empty(),
        "preparation leaves no operation trace"
    );
}
