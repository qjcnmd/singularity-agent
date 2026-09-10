#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
use super::*;
use crate::agent::{AgentEvent, AgentEvents};
use crate::tools::batch::{PreparedToolCall, execute_tool_batch};
use crate::tools::{ToolPreflight, registry::PreparedTool};
use serde_json::{Value, json};
use singularity_core::CancellationToken;
use singularity_model::ModelToolCall;

fn tool_call(id: &str, name: &str, args: Value) -> ModelToolCall {
    ModelToolCall {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        raw_arguments: args.to_string(),
        arguments: args,
        validation_errors: Vec::new(),
    }
}

#[test]
fn batch_mutations_are_barriers_and_completion_follows_commit() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ToolRegistrySnapshot::new();
    let inputs = [
        ("write", json!({"path":"ordered.txt","content":"first"})),
        ("read", json!({"path":"ordered.txt"})),
        ("read", json!({"path":"ordered.txt"})),
        ("write", json!({"path":"ordered.txt","content":"second"})),
        ("read", json!({"path":"ordered.txt"})),
    ];
    let calls: Vec<_> = inputs
        .iter()
        .enumerate()
        .map(|(i, (name, args))| PreparedToolCall {
            call: tool_call(&i.to_string(), name, args.clone()),
            prepared: registry.preflight(name, args),
            result_entry_id: format!("r{i}"),
        })
        .collect();
    let committed = std::cell::RefCell::new(std::collections::HashMap::new());
    let mut on_event = |event| match event {
        AgentEvent::ToolExecutionStarted { tool_call_id, .. } => {
            let prior = match tool_call_id.as_str() {
                "1" | "2" => Some("0"),
                "3" => Some("2"),
                "4" => Some("3"),
                _ => None,
            };
            if let Some(prior) = prior {
                assert!(committed.borrow().contains_key(prior));
            }
        }
        AgentEvent::ToolExecutionEnded {
            tool_call_id,
            execution,
            ..
        } => {
            assert_eq!(committed.borrow().get(&tool_call_id), Some(&execution));
        }
        _ => {}
    };
    execute_tool_batch(
        &registry,
        &calls,
        dir.path(),
        &CancellationToken::new(),
        &mut AgentEvents {
            on_event: Some(&mut on_event),
        },
        &mut |call, result| {
            committed
                .borrow_mut()
                .insert(call.call.tool_call_id.clone(), result.clone());
            Ok::<_, ()>(())
        },
    )
    .unwrap();
    let committed = committed.into_inner();
    assert!(committed.values().all(|result| !result.is_error));
    assert_eq!(committed["1"].content, "first");
    assert_eq!(committed["2"].content, "first");
    assert_eq!(committed["4"].content, "second");
}

#[test]
fn cancellation_and_commit_failure_prevent_later_commands() {
    for fail_commit in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let registry = ToolRegistrySnapshot::new();
        let inputs = [
            ("read", json!({"path":"missing"})),
            ("bash", json!({"command":"echo launched > marker.txt"})),
        ];
        let calls: Vec<_> = inputs
            .iter()
            .enumerate()
            .map(|(i, (name, args))| PreparedToolCall {
                call: tool_call(&i.to_string(), name, args.clone()),
                prepared: registry.preflight(name, args),
                result_entry_id: format!("r{i}"),
            })
            .collect();
        let signal = CancellationToken::new();
        let mut ended = Vec::new();
        let mut on_event = |event| {
            if let AgentEvent::ToolExecutionEnded {
                tool_call_id,
                execution,
                ..
            } = event
            {
                signal.cancel();
                ended.push((tool_call_id, execution));
            }
        };
        let result = execute_tool_batch(
            &registry,
            &calls,
            dir.path(),
            &signal,
            &mut AgentEvents {
                on_event: Some(&mut on_event),
            },
            &mut |_, _| {
                if fail_commit {
                    Err("disk failed")
                } else {
                    Ok(())
                }
            },
        );
        assert!(!dir.path().join("marker.txt").exists());
        if fail_commit {
            assert_eq!(result.unwrap_err(), "disk failed");
            assert!(ended.is_empty());
        } else {
            assert!(result.is_ok());
            assert_eq!(ended[1].1.content, registry::ABORTED_MESSAGE);
        }
    }
}

/// 注册表快照是名单、schema、重放分类的唯一来源：默认工具集确定性排序，
/// 提示词名单与 schema 名单同源。
#[test]
fn registry_snapshot_is_the_single_source_for_names_and_schemas() {
    let registry = ToolRegistrySnapshot::new();
    let prompt_names = registry
        .prompt_lines()
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    assert_eq!(
        prompt_names,
        vec!["bash", "edit", "glob", "grep", "read", "write", "skill"],
        "names follow the fixed registry order"
    );
    let schema_names = registry
        .provider_schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect::<Vec<_>>();
    assert_eq!(
        prompt_names,
        schema_names.iter().map(String::as_str).collect::<Vec<_>>(),
        "tool names and provider schemas derive from the same snapshot"
    );
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

/// 批次并发执行：Started 与返回结果都按模型给定 source order 排列，
/// Ended 随实际完成顺序到达；一个调用失败不阻断其余调用。
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
    let mut results = std::collections::BTreeMap::new();
    {
        let mut on_event = |event| match event {
            AgentEvent::ToolExecutionStarted { tool_call_id, .. } => started.push(tool_call_id),
            AgentEvent::ToolExecutionEnded { tool_call_id, .. } => ended.push(tool_call_id),
            _ => {}
        };
        let mut events = AgentEvents {
            on_event: Some(&mut on_event),
        };
        execute_tool_batch(
            &registry,
            &calls,
            dir.path(),
            &cancellation,
            &mut events,
            &mut |call, result| {
                results.insert(call.call.tool_call_id.clone(), result.clone());
                Ok::<_, std::convert::Infallible>(())
            },
        )
        .unwrap()
    };

    assert_eq!(results.len(), 3, "every call yields a result");
    assert!(!results["c1"].is_error, "present file reads");
    assert_eq!(results["c1"].content, "hello");
    assert!(results["c2"].is_error, "missing file fails");
    assert!(results["c3"].is_error, "unknown tool fails");
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
    assert!(execution.content.contains("only its prefix is shown"));
    assert!(execution.content.contains("byte ranges"));
}

#[test]
fn read_paging_keeps_a_line_that_does_not_fit_the_remaining_byte_budget() {
    use crate::tools::truncate::DEFAULT_MAX_BYTES;
    let dir = tempfile::tempdir().unwrap();
    let first = "a".repeat(DEFAULT_MAX_BYTES - 10);
    let second = format!("{}MUST_SEE", "b".repeat(50));
    std::fs::write(
        dir.path().join("paged.txt"),
        format!("{first}\n{second}\nthird\n"),
    )
    .unwrap();
    let registry = ToolRegistrySnapshot::new();
    let cancellation = CancellationToken::new();
    let read = |offset| {
        let ToolPreflight::Ready(prepared) =
            registry.preflight("read", &json!({"path":"paged.txt", "offset":offset}))
        else {
            panic!("valid read");
        };
        registry
            .execute_prepared(
                prepared,
                ExecuteContext {
                    cwd: dir.path(),
                    signal: &cancellation,
                    on_update: None,
                },
            )
            .content
    };
    let page = read(1);
    assert!(page.starts_with(&first));
    assert!(page.contains("use offset=2"));
    assert!(!page.contains("[truncated]"));
    assert_eq!(read(2), format!("{second}\nthird"));
}

/// patch 头部行号是模型唯一能读到的坐标：hunk 从哪一行开始就必须写哪一行。
#[test]
fn edit_patch_header_reports_the_first_context_line() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").expect("write file");
    let registry = ToolRegistrySnapshot::new();
    let cancellation = CancellationToken::new();
    let ToolPreflight::Ready(prepared) = registry.preflight("read", &json!({"path": "f.txt"}))
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
    assert!(!execution.is_error, "{}", execution.content);
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
        execution
            .diff
            .as_deref()
            .unwrap()
            .contains("@@ -1,3 +1,3 @@"),
        "three context lines starting at line 1: {}",
        execution.content
    );
}

#[test]
fn edits_accept_read_line_endings_and_preserve_original_bytes_outside_the_match() {
    let cases = [
        (
            "\u{feff}开头\r\n旧值\r\n结尾",
            "旧值\n结尾",
            "新值\n新增",
            "\u{feff}开头\r\n新值\r\n新增",
        ),
        (
            "head\nold\nend\n",
            "old\r\nend",
            "new\r\nextra",
            "head\nnew\nextra\n",
        ),
        (
            "head\nold\r\nend\r\ntail\n",
            "old\nend",
            "new\nextra",
            "head\nnew\r\nextra\r\ntail\n",
        ),
        (
            "head\r\nold\r\ntail\r\n",
            "old",
            "new\nextra",
            "head\r\nnew\r\nextra\r\ntail\r\n",
        ),
        (
            "head\r\nold\r\ntail",
            "\nold\n",
            "\nnew\n",
            "head\r\nnew\r\ntail",
        ),
    ];
    for (before, old, new, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, before).unwrap();
        let signal = CancellationToken::new();
        let context = || ExecuteContext {
            cwd: dir.path(),
            signal: &signal,
            on_update: None,
        };
        let registry = ToolRegistrySnapshot::new();
        let ToolPreflight::Ready(read) = registry.preflight("read", &json!({"path":"f.txt"}))
        else {
            panic!("valid read");
        };
        assert!(!registry.execute_prepared(read, context()).is_error);
        let ToolPreflight::Ready(edit) = registry.preflight(
            "edit",
            &json!({"path":"f.txt", "oldString":old, "newString":new}),
        ) else {
            panic!("valid edit");
        };
        let result = registry.execute_prepared(edit, context());
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(std::fs::read(&path).unwrap(), expected.as_bytes());
    }
}

#[test]
fn line_ending_matching_keeps_uniqueness_and_other_whitespace_exact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    let before = "a\r\nb\r\na\nb\n";
    std::fs::write(&path, before).unwrap();
    let signal = CancellationToken::new();
    let context = || ExecuteContext {
        cwd: dir.path(),
        signal: &signal,
        on_update: None,
    };
    let registry = ToolRegistrySnapshot::new();
    let ToolPreflight::Ready(read) = registry.preflight("read", &json!({"path":"f.txt"})) else {
        panic!("valid read");
    };
    assert!(!registry.execute_prepared(read, context()).is_error);
    for (old, replace_all, expected_error) in [
        ("a\nb", false, "2 occurrences"),
        ("a \nb", true, "Could not find"),
        ("a\nb", true, ""),
    ] {
        let ToolPreflight::Ready(edit) = registry.preflight(
            "edit",
            &json!({"path":"f.txt", "oldString":old, "newString":"A\nB", "replaceAll":replace_all}),
        ) else {
            panic!("valid edit");
        };
        let result = registry.execute_prepared(edit, context());
        assert_eq!(
            result.is_error,
            !expected_error.is_empty(),
            "{}",
            result.content
        );
        if result.is_error {
            assert!(
                result.content.contains(expected_error),
                "{}",
                result.content
            );
            assert_eq!(std::fs::read(&path).unwrap(), before.as_bytes());
        }
    }
    assert_eq!(std::fs::read(&path).unwrap(), b"A\r\nB\r\nA\nB\n");
}

/// File edits and full rewrites work without a prior read-tool call.
#[test]
fn mutations_work_without_a_prior_read_tool_call() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::write(dir.path().join("f.txt"), "original\n").expect("write file");
    let registry = ToolRegistrySnapshot::new();
    let cancellation = CancellationToken::new();
    let ToolPreflight::Ready(prepared) = registry.preflight(
        "edit",
        &json!({"path": "f.txt", "oldString": "original", "newString": "clobbered"}),
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
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        "clobbered\n"
    );
    let ToolPreflight::Ready(prepared) =
        registry.preflight("write", &json!({"path": "f.txt", "content": "clobbered\n"}))
    else {
        panic!("valid write args must prepare");
    };
    let execution = registry.execute_prepared(
        prepared,
        ExecuteContext {
            cwd: dir.path(),
            signal: &cancellation,
            on_update: None,
        },
    );
    assert!(
        !execution.is_error,
        "existing targets can be rewritten without a read-tool call"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).expect("read back"),
        "clobbered\n"
    );
}

#[test]
fn mutations_report_all_actual_changes_and_never_a_failed_diff() {
    let dir = tempfile::tempdir().expect("workspace");
    let registry = ToolRegistrySnapshot::new();
    let cancellation = CancellationToken::new();
    let execute = |name: &str, args: Value| {
        let ToolPreflight::Ready(prepared) = registry.preflight(name, &args) else {
            panic!("valid tool args");
        };
        registry.execute_prepared(
            prepared,
            ExecuteContext {
                cwd: dir.path(),
                signal: &cancellation,
                on_update: None,
            },
        )
    };
    let created = execute(
        "write",
        json!({"path": "f.txt", "content": "old\nmiddle\nold\n"}),
    );
    assert!(!created.is_error);
    assert!(!created.content.contains("+old"));
    assert!(
        created
            .diff
            .as_deref()
            .unwrap()
            .contains("+old\n+middle\n+old")
    );
    let edited = execute(
        "edit",
        json!({"path": "f.txt", "oldString": "old", "newString": "new", "replaceAll": true}),
    );
    assert!(!edited.is_error);
    assert_eq!(
        edited
            .diff
            .as_deref()
            .unwrap()
            .lines()
            .filter(|line| *line == "-old")
            .count(),
        2
    );
    assert_eq!(
        edited
            .diff
            .as_deref()
            .unwrap()
            .lines()
            .filter(|line| *line == "+new")
            .count(),
        2
    );
    let failed = execute(
        "edit",
        json!({"path": "f.txt", "oldString": "missing", "newString": "never"}),
    );
    assert!(failed.is_error);
    assert!(failed.diff.is_none());
    let overwritten = execute(
        "write",
        json!({"path": "f.txt", "content": "replacement\n"}),
    );
    assert!(!overwritten.is_error);
    assert!(overwritten.diff.as_deref().unwrap().contains("-middle"));
    assert!(
        overwritten
            .diff
            .as_deref()
            .unwrap()
            .contains("+replacement")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        "replacement\n"
    );
}

#[test]
fn concurrent_edits_preserve_each_others_changes() {
    use std::sync::{Arc, Barrier};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shared.txt");
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(&path, "left\nright\n").unwrap();
    let barrier = Arc::new(Barrier::new(3));
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for (path, old, new) in [
            ("shared.txt", "left", "LEFT"),
            ("sub/../shared.txt", "right", "RIGHT"),
        ] {
            let barrier = Arc::clone(&barrier);
            let cwd = dir.path();
            workers.push(scope.spawn(move || {
                let registry = ToolRegistrySnapshot::new();
                let ToolPreflight::Ready(prepared) = registry.preflight(
                    "edit",
                    &json!({"path":path, "oldString":old, "newString":new}),
                ) else {
                    panic!("valid arguments")
                };
                barrier.wait();
                registry.execute_prepared(
                    prepared,
                    ExecuteContext {
                        cwd,
                        signal: &CancellationToken::new(),
                        on_update: None,
                    },
                )
            }));
        }
        barrier.wait();
        for worker in workers {
            let result = worker.join().unwrap();
            assert!(!result.is_error, "{}", result.content);
        }
    });
    assert_eq!(std::fs::read_to_string(path).unwrap(), "LEFT\nRIGHT\n");
}
