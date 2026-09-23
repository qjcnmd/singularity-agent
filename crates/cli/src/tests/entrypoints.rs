#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! 默认 Web 入口与 JSON 评估输出、持久化事实的一致性。

use std::sync::Arc;

use clap::Parser;
use serde_json::{Value, json};
use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};
use singularity_protocol::TurnStatus;

use super::support::{BufferedSink, FailOnSubstring, HeadlessFixture, session_records};
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
    assert_eq!(default.port, 3081);
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
    // 每条已接受输入都有控制身份，因此轮次开始先发布它开始为新一轮的处置；
    // turn/started 仍是该轮第一个 turn 生命周期事件。
    assert_eq!(json_output.events[0].0, "turn/controlChanged");
    assert_eq!(json_output.events[1].0, "turn/started");
    assert_eq!(
        json_output
            .events
            .iter()
            .filter(|(method, _)| method == "turn/controlChanged")
            .count(),
        1,
        "one control starts this turn"
    );
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
    assert_eq!(durable_terminal(&json_fixture), TurnStatus::Completed);

    let json_order = durable_tool_order(&json_fixture);
    assert_eq!(json_order, vec!["c1", "c2", "c3"]);
    let event_order: Vec<_> = json_output
        .events
        .iter()
        .filter(|(method, _)| method == "tool/execution/start")
        .map(|(_, params)| params["item"]["itemId"].as_str().unwrap())
        .collect();
    let durable_item_ids: Vec<_> = super::support::session_entries(&json_fixture)
        .iter()
        .filter(|entry| {
            matches!(entry,
                singularity_agent::session::SessionEntry::Message { message, .. }
                    if matches!(message, singularity_agent::message::AgentMessage::ToolResult { .. })
            )
        })
        .map(|entry| entry.id().to_string())
        .collect();
    assert_eq!(event_order, durable_item_ids);
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
        matches!(&json_output.outcome, ProcessOutcome::Failed(message)
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

/// 任务先失败、stdout 随后失败：两个原因都进入唯一进程结果，原始任务原因
/// 不被输出故障覆盖。
#[test]
fn task_failure_is_not_replaced_by_an_output_failure() {
    let fixture = HeadlessFixture::new(Arc::new(ScriptedProvider::new([
        ScriptedAttempt::failure_kind(singularity_model::ModelErrorKind::AuthError, "key rejected"),
    ])));
    let out = BufferedSink::default();
    let capture = out.clone();
    let renderer = JsonlRenderer::with_writer(
        Some(fixture.thread_id.clone()),
        FailOnSubstring::new(out, "{\"summary\""),
    );
    let outcome = crate::execute_headless(&fixture.conversation, "doomed task", renderer);
    let (code, message) = outcome.finish();
    assert_eq!(
        code, 1,
        "an output failure never turns a failure into success"
    );
    let message = message.expect("failure message");
    assert!(
        message.contains("provider_auth") && message.contains("key rejected"),
        "the task reason survives: {message}"
    );
    assert!(
        message.contains("simulated stdout failure"),
        "the output failure is reported too: {message}"
    );
    assert!(
        capture.text().contains("turn/error"),
        "the failure event is still projected to the output channel"
    );
}

/// 输出故障与各类任务结果的合并规则：两个入口共用这一条规则。
/// 用户中断保留 130 与中断事实；输出正常时结果不变。任务失败与任务成功两种
/// 组合由 `task_failure_is_not_replaced_by_an_output_failure` 与
/// `summary_write_failure_never_looks_like_success` 端到端覆盖。
#[test]
fn an_interruption_keeps_its_exit_code_and_a_healthy_output_changes_nothing() {
    let interrupted =
        ProcessOutcome::Interrupted(None).with_output_failure(Some("simulated stdout failure"));
    assert_eq!(
        interrupted.finish().0,
        130,
        "an interruption keeps its exit code"
    );
    assert!(
        matches!(&interrupted, ProcessOutcome::Interrupted(Some(message))
        if message.contains("simulated stdout failure")),
        "the interruption and the output failure are both reported: {interrupted:?}"
    );

    assert_eq!(
        ProcessOutcome::Failed("prepare failed".to_string()).with_output_failure(None),
        ProcessOutcome::Failed("prepare failed".to_string()),
        "a healthy output leaves the result untouched"
    );
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

fn durable_terminal(fixture: &HeadlessFixture) -> TurnStatus {
    let terminals: Vec<_> = session_records(fixture)
        .iter()
        .filter_map(|record| match record {
            singularity_agent::session::LedgerRecord::OperationFinished { outcome, .. } => {
                Some(*outcome)
            }
            _ => None,
        })
        .collect();
    assert_eq!(terminals.len(), 1);
    terminals[0]
}

fn durable_tool_order(fixture: &HeadlessFixture) -> Vec<String> {
    super::support::session_entries(fixture)
        .iter()
        .filter_map(|entry| match entry {
            singularity_agent::session::SessionEntry::Message { message, .. }
                if matches!(
                    message,
                    singularity_agent::message::AgentMessage::ToolResult { .. }
                ) =>
            {
                message.tool_call_id().map(str::to_string)
            }
            _ => None,
        })
        .collect()
}
