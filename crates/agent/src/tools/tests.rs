#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
use super::*;
use crate::agent::{AgentEvent, AgentEvents};
use crate::tools::batch::{PreparedToolCall, execute_tool_batch};
use crate::tools::{ToolPreflight, registry::PreparedTool};
use serde_json::{Value, json};
use singularity_core::CancellationToken;
use singularity_model::{ModelToolCall, ModelToolParseStatus, ProviderProtocolContract};

fn tool_call(id: &str, name: &str, args: Value) -> ModelToolCall {
    ModelToolCall {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        raw_arguments: args.to_string(),
        arguments: args,
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    }
}

/// 注册表快照是名单、schema、重放分类的唯一来源：默认工具集确定性排序，
/// 提示词名单与 schema 名单同源。
#[test]
fn registry_snapshot_is_the_single_source_for_names_and_schemas() {
    let registry = ToolRegistrySnapshot::new();
    assert_eq!(
        registry.names(),
        vec!["bash", "edit", "glob", "grep", "read", "write"],
        "names follow the fixed registry order"
    );
    let schema_names = registry
        .provider_schemas(&ProviderProtocolContract::default())
        .into_iter()
        .map(|schema| schema.name)
        .collect::<Vec<_>>();
    assert_eq!(
        registry.names(),
        schema_names.iter().map(String::as_str).collect::<Vec<_>>(),
        "tool names and provider schemas derive from the same snapshot"
    );
}

/// provider schema 投影按能力声明的工具数上限截断，不静默发送超限工具。
#[test]
fn provider_schemas_respect_the_tool_count_cap() {
    let registry = ToolRegistrySnapshot::new();
    let capabilities = ProviderProtocolContract {
        max_tools_per_request: 2,
        ..ProviderProtocolContract::default()
    };
    let schemas = registry.provider_schemas(&capabilities);
    assert_eq!(schemas.len(), 2, "capped to the declared maximum");
    assert_eq!(schemas[0].name, "bash");
    assert_eq!(schemas[1].name, "edit");
}

/// preflight 把未知工具与非法参数都收敛为模型可见拒绝，不进入执行。
#[test]
fn preflight_rejects_unknown_tool_and_invalid_args() {
    let registry = ToolRegistrySnapshot::new();
    assert!(matches!(
        registry.preflight("nope", &json!({})),
        ToolPreflight::Rejected(execution) if execution.is_error
    ));
    // read 缺必填 path。
    assert!(matches!(
        registry.preflight("read", &json!({"offset": 1})),
        ToolPreflight::Rejected(execution) if execution.is_error
    ));
    // read 未知字段（deny_unknown_fields）。
    assert!(matches!(
        registry.preflight("read", &json!({"path": "a", "surprise": 1})),
        ToolPreflight::Rejected(execution) if execution.is_error
    ));
    assert!(matches!(
        registry.preflight("read", &json!({"path": "a"})),
        ToolPreflight::Ready(PreparedTool::Read(_))
    ));
}

/// 批次并发执行：`Started` 与返回结果都按模型给定 source order 排列，
/// `Ended` 随实际完成顺序到达；一个调用失败不阻断其余调用。
#[test]
fn batch_reports_source_order_and_isolates_failures() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::write(dir.path().join("present.txt"), "hello").expect("write fixture");
    let registry = ToolRegistrySnapshot::new();
    let cancellation = CancellationToken::new();

    let calls = [
        PreparedToolCall {
            call: tool_call("c1", "read", json!({"path": "present.txt"})),
            prepared: registry.preflight("read", &json!({"path": "present.txt"})),
            result_entry_id: "r1".to_string(),
        },
        PreparedToolCall {
            call: tool_call("c2", "read", json!({"path": "missing.txt"})),
            prepared: registry.preflight("read", &json!({"path": "missing.txt"})),
            result_entry_id: "r2".to_string(),
        },
        PreparedToolCall {
            call: tool_call("c3", "ghost", json!({})),
            prepared: registry.preflight("ghost", &json!({})),
            result_entry_id: "r3".to_string(),
        },
    ];

    let mut started = Vec::new();
    let mut ended = Vec::new();
    let results = {
        let mut on_event = |event| match event {
            AgentEvent::ToolExecutionStarted { tool_call_id, .. } => started.push(tool_call_id),
            AgentEvent::ToolExecutionEnded { tool_call_id, .. } => ended.push(tool_call_id),
            _ => {}
        };
        let mut events = AgentEvents {
            on_event: Some(&mut on_event),
        };
        execute_tool_batch(&registry, &calls, dir.path(), &cancellation, &mut events)
    };

    assert_eq!(results.len(), 3, "every call yields a result");
    assert!(!results[0].is_error, "present file reads");
    assert_eq!(results[0].content, "hello");
    assert!(results[1].is_error, "missing file fails");
    assert!(results[2].is_error, "unknown tool fails");
    // 失败不阻断：三个调用都执行并各自发出 started/ended。
    assert_eq!(started, vec!["c1", "c2", "c3"], "source order preserved");
    ended.sort();
    assert_eq!(
        ended,
        vec!["c1", "c2", "c3"],
        "every call gets exactly one end"
    );
}

/// 输出上限：超过读取预算的文件正文被截断并带截断标记，不整体返回。
#[test]
fn read_output_is_truncated_at_the_byte_budget() {
    use crate::tools::truncate::DEFAULT_MAX_BYTES;
    let dir = tempfile::tempdir().expect("workspace");
    let big = "x".repeat(DEFAULT_MAX_BYTES * 2);
    std::fs::write(dir.path().join("big.txt"), format!("{big}\n")).expect("write");
    let registry = ToolRegistrySnapshot::new();
    let cancellation = CancellationToken::new();
    let ToolPreflight::Ready(prepared) = registry.preflight("read", &json!({"path": "big.txt"}))
    else {
        panic!("valid read args must prepare");
    };
    let execution = registry.execute_prepared(
        prepared,
        ExecuteContext {
            cwd: dir.path(),
            signal: &cancellation,
            on_update: None,
        },
    );
    assert!(!execution.is_error);
    assert!(
        execution.content.contains("[truncated]"),
        "over-budget read must mark truncation"
    );
    assert!(
        execution.content.len() < big.len(),
        "truncated output is smaller than the file"
    );
}

/// patch 头部行号是模型唯一能读到的坐标：hunk 从哪一行开始就必须写哪一行。
#[test]
fn edit_patch_header_reports_the_first_context_line() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").expect("write file");
    let registry = ToolRegistrySnapshot::new();
    let cancellation = CancellationToken::new();
    let ToolPreflight::Ready(prepared) = registry.preflight(
        "edit",
        &json!({"path": "f.txt", "oldString": "b", "newString": "B"}),
    ) else {
        panic!("valid edit args must prepare");
    };
    let execution = registry.execute_prepared(
        prepared,
        ExecuteContext {
            cwd: dir.path(),
            signal: &cancellation,
            on_update: None,
        },
    );
    assert!(!execution.is_error, "{}", execution.content);
    assert!(
        execution.content.contains("@@ -1,3 +1,3 @@"),
        "three context lines starting at line 1: {}",
        execution.content
    );
}
