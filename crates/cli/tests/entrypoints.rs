#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! 最终产品入口合同：默认 Web 工作台与两个共享执行路径的 headless 投影。

use std::sync::Arc;

use clap::Parser;
use serde_json::{Value, json};
use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};
use singularity_runtime::objects::{TurnModelUsage, TurnStatus};

use crate::headless_support::{BufferedSink, HeadlessFixture, session_records};
use crate::jsonl_mode::JsonlRenderer;
use crate::print_mode::PrintRenderer;
use crate::{Cli, HeadlessView, Mode, ProcessOutcome};

struct JsonRunOutput {
    outcome: ProcessOutcome,
    events: Vec<(String, Value)>,
    summaries: Vec<Value>,
}

#[test]
fn command_line_selects_web_by_default_and_keeps_headless_exclusive() {
    let default = Cli::try_parse_from(["singularity"]).expect("default web command");
    assert_eq!(default.mode().unwrap(), None);
    assert_eq!(default.port, 3080);
    assert!(!default.no_open);

    let ephemeral = Cli::try_parse_from(["singularity", "--port", "0", "--no-open"])
        .expect("ephemeral no-open web command");
    assert_eq!(ephemeral.mode().unwrap(), None);
    assert_eq!(ephemeral.port, 0);
    assert!(ephemeral.no_open);

    let print = Cli::try_parse_from(["singularity", "--print", "goal"]).expect("print command");
    assert_eq!(print.mode().unwrap(), Some(Mode::Print));
    let json = Cli::try_parse_from(["singularity", "--json", "goal"]).expect("json command");
    assert_eq!(json.mode().unwrap(), Some(Mode::Json));

    assert!(Cli::try_parse_from(["singularity", "goal"]).is_err());
    assert!(Cli::try_parse_from(["singularity", "--print", "--json", "goal"]).is_err());
    assert!(Cli::try_parse_from(["singularity", "--print", "--port", "0", "goal"]).is_err());
}

#[test]
fn print_and_json_share_successful_execution_facts() {
    let goal = "read, modify and validate notes.txt";
    let json_fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::new(journey_script())));
    let json_output = run_json(&json_fixture, goal);
    assert_eq!(json_output.outcome, ProcessOutcome::Completed);
    assert_eq!(json_output.summaries.len(), 1);
    assert_eq!(
        json_output.summaries[0]["turn"]["status"],
        json!("completed")
    );
    assert_eq!(
        json_output.summaries[0]["turn"]["usage"]["totalTokens"],
        json!(15)
    );
    assert_eq!(json_output.events.last().unwrap().0, "turn/completed");

    let print_fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::new(journey_script())));
    let (print_outcome, print_stdout) = run_print(&print_fixture, goal);
    assert_eq!(print_outcome, ProcessOutcome::Completed);
    assert_eq!(print_stdout, "task complete\n");

    let json_order = durable_tool_order(&json_fixture);
    assert_eq!(json_order, vec!["c1", "c2", "c3"]);
    assert_eq!(durable_tool_order(&print_fixture), json_order);
    assert_eq!(
        durable_terminal(&json_fixture),
        durable_terminal(&print_fixture)
    );
    assert_eq!(print_fixture.read_file("notes.txt"), "beta\n");
}

#[test]
fn print_and_json_share_failed_execution_facts() {
    let failure = || {
        ScriptedAttempt::failure_kind(singularity_model::ModelErrorKind::AuthError, "key rejected")
    };
    let json_fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::new([failure()])));
    let json_output = run_json(&json_fixture, "doomed task");
    assert!(
        matches!(&json_output.outcome, ProcessOutcome::TurnFailed(message)
        if message.contains("provider_auth") && message.contains("key rejected"))
    );
    assert_eq!(json_output.summaries.len(), 1);
    assert_eq!(json_output.summaries[0]["turn"]["status"], json!("failed"));
    assert_eq!(
        json_output
            .events
            .iter()
            .filter(|(method, _)| method == "turn/error")
            .count(),
        1
    );

    let print_fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::new([failure()])));
    let (print_outcome, print_stdout) = run_print(&print_fixture, "doomed task");
    assert!(matches!(&print_outcome, ProcessOutcome::TurnFailed(message)
        if message.contains("key rejected")));
    assert_eq!(print_stdout, "");
    assert_eq!(
        durable_terminal(&json_fixture),
        durable_terminal(&print_fixture)
    );
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
            let method = value["method"].as_str().expect("event method");
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
    let view = HeadlessView::Print(PrintRenderer::with_writers(out, BufferedSink::default()));
    let outcome =
        crate::execute_headless(Arc::clone(&fixture.conversation), goal.to_string(), view);
    (outcome, capture.text())
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
    assert_eq!(terminals.len(), 1);
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
