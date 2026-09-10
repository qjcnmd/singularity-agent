#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! 默认 Web 入口与 JSON 评估输出、持久化事实的一致性。

use std::sync::Arc;

use clap::Parser;
use serde_json::{Value, json};
use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};
use singularity_runtime::objects::{TurnModelUsage, TurnStatus};

use super::support::{BufferedSink, HeadlessFixture, session_records};
use crate::jsonl_mode::JsonlRenderer;
use crate::{Cli, ProcessOutcome};

struct JsonRunOutput {
    outcome: ProcessOutcome,
    events: Vec<(String, Value)>,
    summaries: Vec<Value>,
}

#[test]
fn command_line_selects_web_by_default_and_keeps_headless_exclusive() {
    let default = Cli::try_parse_from(["singularity"]).expect("default web command");
    assert!(!default.json);
    assert_eq!(default.port, 3080);
    assert!(!default.no_open);

    let ephemeral = Cli::try_parse_from(["singularity", "--port", "0", "--no-open"])
        .expect("ephemeral no-open web command");
    assert!(!ephemeral.json);
    assert_eq!(ephemeral.port, 0);
    assert!(ephemeral.no_open);

    let json = Cli::try_parse_from(["singularity", "--json", "--model", "provider/model", "goal"])
        .expect("evaluation command");
    assert!(json.json);
    assert_eq!(json.model.as_deref(), Some("provider/model"));
    assert!(Cli::try_parse_from(["singularity", "goal"]).is_err());
    assert!(Cli::try_parse_from(["singularity", "--json"]).is_err());
    assert!(Cli::try_parse_from(["singularity", "--model", "provider/model"]).is_err());
    assert!(Cli::try_parse_from(["singularity", "--json", "--port", "0", "goal"]).is_err());
}

#[test]
fn json_output_matches_persisted_execution_facts() {
    let goal = "read, modify and validate notes.txt";
    let json_fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::new(journey_script())));
    let json_output = run_json(&json_fixture, goal);
    assert_eq!(json_output.outcome, ProcessOutcome::Completed);
    assert_eq!(json_output.outcome.finish(), (0, None));
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
    assert_eq!(json_output.events.first().unwrap().0, "turn/started");
    assert_eq!(
        json_output
            .events
            .iter()
            .filter(|(method, _)| method == "turn/completed")
            .count(),
        1
    );
    let turn = &json_output.summaries[0]["turn"];
    let terminal = &json_output.events.last().unwrap().1["turn"];
    for key in ["threadId", "status", "usage"] {
        assert_eq!(terminal[key], turn[key]);
    }
    let records = session_records(&json_fixture);
    let durable_turn_ids: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            singularity_agent::session::LedgerRecord::OperationStarted { turn_id, .. }
            | singularity_agent::session::LedgerRecord::OperationFinished { turn_id, .. } => {
                turn_id.as_deref()
            }
            _ => None,
        })
        .collect();
    assert_eq!(durable_turn_ids, [terminal["turnId"].as_str().unwrap(); 2]);
    assert_eq!(turn["threadId"], json_fixture.thread_id);
    let (status, usage) = durable_terminal(&json_fixture);
    assert_eq!(status, TurnStatus::Completed);
    assert_eq!(serde_json::to_value(usage).unwrap(), turn["usage"]);

    let json_order = durable_tool_order(&json_fixture);
    assert_eq!(json_order, vec!["c1", "c2", "c3"]);
    let event_order: Vec<_> = json_output
        .events
        .iter()
        .filter(|(method, _)| method == "tool/execution/start")
        .map(|(_, params)| params["toolCallId"].as_str().unwrap())
        .collect();
    assert_eq!(event_order, json_order);
    assert_eq!(json_fixture.read_file("notes.txt"), "beta\n");
}

#[test]
fn json_reports_provider_failure_and_a_failed_summary() {
    let failure = || {
        ScriptedAttempt::failure_kind(singularity_model::ModelErrorKind::AuthError, "key rejected")
    };
    let json_fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::new([failure()])));
    let json_output = run_json(&json_fixture, "doomed task");
    assert!(
        matches!(&json_output.outcome, ProcessOutcome::TurnFailed(message)
        if message.contains("provider_auth") && message.contains("key rejected"))
    );
    assert_eq!(json_output.outcome.finish().0, 1);
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
}

#[test]
fn json_distinguishes_provider_cancellation_from_failure() {
    let fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::new([
        ScriptedAttempt::failure_kind(singularity_model::ModelErrorKind::Cancelled, "cancelled"),
    ])));
    let output = run_json(&fixture, "cancelled turn");
    assert_eq!(output.outcome.finish(), (130, None));
    assert_eq!(output.summaries[0]["turn"]["status"], "interrupted");
}

fn journey_script() -> Vec<ScriptedAttempt> {
    let usage = singularity_model::ModelUsage {
        input_tokens: 10,
        output_tokens: 5,
        total_tokens: 15,
        usage_present: true,
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
    let renderer = JsonlRenderer::with_writer(Some(fixture.thread_id.clone()), out);
    let outcome = crate::execute_headless(&fixture.conversation, goal, renderer);
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
    super::support::session_entries(fixture)
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
