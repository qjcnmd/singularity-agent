#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
use super::*;
use crate::agent::AgentEvent;
use crate::tools::batch::{PreparedToolCall, ToolBatchError, execute_tool_batch};
use crate::tools::registry::PreparedTool;
use serde_json::{Value, json};
use singularity_core::CancellationToken;
use singularity_model::ModelToolCall;

fn tool_call(id: &str, name: &str, args: Value) -> ModelToolCall {
    ModelToolCall {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        arguments: args,
    }
}

#[test]
fn grep_bounds_long_ascii_and_unicode_matches_without_splitting_entries() {
    use super::truncate::DEFAULT_MAX_BYTES;
    for text in ["x".repeat(2000), "界".repeat(800)] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("命中.txt"), format!("{text}\n").repeat(600)).unwrap();
        let result = super::grep::execute(
            &super::grep::GrepArgs {
                pattern: ".".into(),
                path: None,
                include: None,
            },
            ExecuteContext {
                cwd: dir.path(),
                signal: &CancellationToken::new(),
                on_update: None,
            },
        );
        assert!(!result.is_error);
        let (entries, note) = result.content.split_once("\n[grep]").unwrap();
        assert!(entries.len() <= DEFAULT_MAX_BYTES);
        assert!(result.content.len() < DEFAULT_MAX_BYTES + 200);
        let lines: Vec<_> = entries.lines().collect();
        assert!(!lines.is_empty() && lines.len() < 500);
        for (index, line) in lines.iter().enumerate() {
            assert!(line.starts_with(&format!("命中.txt:{}:", index + 1)));
            assert!(line.ends_with("..."));
            assert!(!line.contains('\u{fffd}'));
        }
        assert!(note.contains(&format!("truncated at {} matches", lines.len())));
        assert!(note.contains(&format!("{DEFAULT_MAX_BYTES}-byte output limit")));
    }
}

#[test]
fn grep_stops_at_match_limit_before_byte_limit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hits.txt"), "x\n".repeat(600)).unwrap();
    let result = super::grep::execute(
        &super::grep::GrepArgs {
            pattern: "x".into(),
            path: None,
            include: None,
        },
        ExecuteContext {
            cwd: dir.path(),
            signal: &CancellationToken::new(),
            on_update: None,
        },
    );
    assert!(result.content.contains("hits.txt:500:x\n"));
    assert!(!result.content.contains("hits.txt:501:"));
    assert!(
        result
            .content
            .contains("search stopped at 500 matches; results may be incomplete")
    );
}

#[cfg(windows)]
#[test]
fn grep_keeps_matches_and_reports_unreadable_files() {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("readable.txt"), "needle").unwrap();
    let locked_path = dir.path().join("locked.txt");
    std::fs::write(&locked_path, "needle").unwrap();
    let _locked = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&locked_path)
        .unwrap();
    let registry = ToolRegistrySnapshot::default();
    let Ok(prepared) = registry.preflight("grep", &json!({"pattern":"needle"})) else {
        panic!("valid grep arguments");
    };
    let result = prepared.execute(ExecuteContext {
        cwd: dir.path(),
        signal: &CancellationToken::new(),
        on_update: None,
    });
    assert!(!result.is_error);
    assert!(result.content.contains("readable.txt:1:needle"));
    assert!(
        result
            .content
            .contains("search incomplete: 1 unreadable path(s)")
    );
    assert!(result.content.contains("locked.txt"));
}

/// 单文件扫描器直接测试：二进制嗅探、畸形超长行与剩余预算耗尽都在这里收敛，
/// 调用方只负责把结果并入全局累计。
#[test]
fn grep_scan_file_reports_binary_over_limit_lines_and_exhausted_budgets() {
    use regex::Regex;

    let dir = tempfile::tempdir().unwrap();
    let signal = CancellationToken::new();
    let regex = Regex::new("x").unwrap();
    let scan = |name: &str, match_budget: usize, byte_budget: usize| {
        super::grep::scan_file(
            &dir.path().join(name),
            name,
            &regex,
            match_budget,
            byte_budget,
            &signal,
        )
    };

    // 二进制：出现 NUL 字节的文件静默跳过，既不产生命中也不产生警告。
    std::fs::write(dir.path().join("binary.bin"), b"x\0x\n").unwrap();
    assert!(scan("binary.bin", 10, 1024).unwrap().is_none());

    // 畸形超长行：整个文件被跳过并计数，读到的内容不进入结果。
    std::fs::write(
        dir.path().join("long.txt"),
        format!("{}\n", "x".repeat(super::line::MAX_READ_LINE_BYTES + 1)),
    )
    .unwrap();
    let over_limit = scan("long.txt", 10, 1024).unwrap().unwrap();
    assert!(over_limit.over_limit_line);
    assert!(over_limit.lines.is_empty());
    assert!(over_limit.read_error.is_none());
    assert!(over_limit.stop.is_none());

    // 命中与字节预算的收敛由走真实入口的 grep 用例覆盖；这里只钉住单文件扫描器
    // 独有的两种跳过。
    // 打不开的文件是 Err：调用方据此保留已收集的命中并记录警告。
    assert!(scan("missing.txt", 10, 1024).is_err());
}

/// glob 的截断提示只在确实存在第 201 个匹配时出现：取满 200 条本身不是还有
/// 剩余结果的证据，否则恰好 200 条会误导模型去缩小范围重搜。
#[test]
fn glob_reports_truncation_only_when_a_match_beyond_the_cap_exists() {
    let cases: [(&str, usize, &[&str], bool); 4] = [
        ("below the cap", 199, &["zzz-nomatch.txt"], false),
        (
            "exactly at the cap",
            200,
            &[".keep", "zzz-nomatch.txt"],
            false,
        ),
        ("a match beyond the cap", 201, &[], true),
        (
            "at the cap with a later match",
            200,
            &["match_zzz.txt"],
            true,
        ),
    ];
    for (label, matches, extra, truncated) in cases {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..matches {
            std::fs::write(dir.path().join(format!("match_{index:03}.txt")), "").unwrap();
        }
        for name in extra {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let result = super::glob::execute(
            &super::glob::GlobArgs {
                pattern: "match_*.txt".into(),
                path: None,
            },
            ExecuteContext {
                cwd: dir.path(),
                signal: &CancellationToken::new(),
                on_update: None,
            },
        );
        assert!(!result.is_error, "{label}: {}", result.content);
        let (entries, note) = result
            .content
            .split_once("\n[glob]")
            .map_or((result.content.as_str(), None), |(entries, note)| {
                (entries, Some(note))
            });
        assert_eq!(note.is_some(), truncated, "{label}: {}", result.content);
        if let Some(note) = note {
            assert!(note.contains("truncated"), "{label}: {note}");
        }
        assert_eq!(entries.lines().count(), matches.min(200), "{label}");
    }
}

/// 不足上限时结果保持排序，且不出现任何截断提示。
#[test]
fn glob_keeps_sorted_results_without_a_truncation_notice() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["b.txt", "a.txt"] {
        std::fs::write(dir.path().join(name), "").unwrap();
    }
    let result = super::glob::execute(
        &super::glob::GlobArgs {
            pattern: "*.txt".into(),
            path: None,
        },
        ExecuteContext {
            cwd: dir.path(),
            signal: &CancellationToken::new(),
            on_update: None,
        },
    );
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "a.txt\nb.txt");
}

/// 已取消的信号让 glob 走既有取消路径，不返回部分结果。
#[test]
fn glob_returns_the_cancellation_result_before_any_match() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("match.txt"), "").unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let result = super::glob::execute(
        &super::glob::GlobArgs {
            pattern: "*.txt".into(),
            path: None,
        },
        ExecuteContext {
            cwd: dir.path(),
            signal: &cancellation,
            on_update: None,
        },
    );
    assert!(result.is_error, "{}", result.content);
    assert!(
        result.content.contains("Operation aborted"),
        "{}",
        result.content
    );
}

#[test]
fn batch_mutations_are_barriers_and_completion_follows_commit() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ToolRegistrySnapshot::default();
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
        AgentEvent::ToolExecutionStarted { item_id, .. } => {
            let prior = match item_id.as_str() {
                "r1" | "r2" => Some("r0"),
                "r3" => Some("r2"),
                "r4" => Some("r3"),
                _ => None,
            };
            if let Some(prior) = prior {
                assert!(committed.borrow().contains_key(prior));
            }
        }
        AgentEvent::ToolExecutionEnded {
            item_id, execution, ..
        } => {
            assert_eq!(committed.borrow().get(&item_id), Some(&execution));
        }
        _ => {}
    };
    execute_tool_batch(
        &calls,
        dir.path(),
        &CancellationToken::new(),
        &mut on_event,
        &mut |call, result| {
            committed
                .borrow_mut()
                .insert(call.result_entry_id.clone(), result.clone());
            Ok::<_, ()>(())
        },
    )
    .unwrap();
    let committed = committed.into_inner();
    assert!(committed.values().all(|result| !result.is_error));
    assert_eq!(committed["r1"].content, "first");
    assert_eq!(committed["r2"].content, "first");
    assert_eq!(committed["r4"].content, "second");
}

/// worker 的 panic 是宿主故障：不伪装成普通工具结果，不再派发后续工具，也不
/// 让模型把故障原因当成可纠正的业务失败继续试。
#[test]
fn a_panicking_tool_worker_stops_the_batch_as_a_host_failure() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("notes.txt");
    std::fs::write(&target, "original").unwrap();
    // 先让 mutation 锁中毒：下一个 edit 取锁时按既有 fail-stop 策略 panic。
    let _poisoned = super::mutation::poison_lock(&target);

    let registry = ToolRegistrySnapshot::default();
    let inputs = [
        (
            "edit",
            json!({"path": "notes.txt", "oldString": "original", "newString": "changed"}),
        ),
        ("read", json!({"path": "notes.txt"})),
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
    let mut started = Vec::new();
    let mut ended = Vec::new();
    let mut on_event = |event| match event {
        AgentEvent::ToolExecutionStarted { item_id, .. } => started.push(item_id),
        AgentEvent::ToolExecutionEnded { item_id, .. } => ended.push(item_id),
        _ => {}
    };
    let error = execute_tool_batch(
        &calls,
        dir.path(),
        &CancellationToken::new(),
        &mut on_event,
        &mut |_, _| Ok::<_, ()>(()),
    )
    .expect_err("a panicking worker is a host failure");
    let ToolBatchError::HostFailure(message) = error else {
        panic!("a panic must not be reported as a commit failure");
    };
    assert!(message.contains("panicked"), "{message}");
    assert!(message.contains("mutation lock poisoned"), "{message}");
    assert_eq!(started, vec!["r0"], "later tools are never dispatched");
    assert!(
        ended.is_empty(),
        "a host failure publishes no completion event"
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "original",
        "the panicking tool produced no side effect"
    );
}

/// 准备阶段结束、真正提交之前的取消不再产生文件副作用；取消不伪造回滚。
#[test]
fn cancellation_before_the_commit_keeps_the_original_file() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("f.txt");
    std::fs::write(&target, "original\n").unwrap();
    let cancellation = CancellationToken::new();
    let hook_token = cancellation.clone();
    super::edit::BEFORE_COMMIT.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || hook_token.cancel()));
    });
    let registry = ToolRegistrySnapshot::default();
    let prepared = registry
        .preflight(
            "edit",
            &json!({"path": "f.txt", "oldString": "original", "newString": "changed"}),
        )
        .expect("valid edit args must prepare");
    let execution = prepared.execute(ExecuteContext {
        cwd: dir.path(),
        signal: &cancellation,
        on_update: None,
    });
    assert!(
        execution.is_error && execution.content.contains("Operation aborted"),
        "{}",
        execution.content
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "original\n",
        "a cancelled edit must not commit the replacement"
    );
    assert!(execution.diff.is_none(), "no change is reported as applied");
}

#[test]
fn cancellation_and_commit_failure_prevent_later_commands() {
    for fail_commit in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let registry = ToolRegistrySnapshot::default();
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
                item_id, execution, ..
            } = event
            {
                signal.cancel();
                ended.push((item_id, execution));
            }
        };
        let result = execute_tool_batch(&calls, dir.path(), &signal, &mut on_event, &mut |_, _| {
            if fail_commit {
                Err("disk failed")
            } else {
                Ok(())
            }
        });
        assert!(!dir.path().join("marker.txt").exists());
        if fail_commit {
            assert!(matches!(
                result.unwrap_err(),
                ToolBatchError::Commit("disk failed")
            ));
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
    let registry = ToolRegistrySnapshot::default();
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
    // 广告出来的名字都进得了分发：默认注册表的 schema 与可执行集合一致。
    for name in &prompt_names {
        if let Err(error) = registry.preflight(name, &json!({})) {
            assert!(
                !error.content.contains("unknown tool"),
                "{name} is advertised but not dispatchable: {}",
                error.content
            );
        }
    }
}

/// preflight 把未知工具与非法参数都收敛为模型可见拒绝，不进入执行。
#[test]
fn preflight_rejects_unknown_tool_and_invalid_args() {
    let registry = ToolRegistrySnapshot::default();
    for arguments in [json!(["a", null, null]), json!("{\"path\":"), Value::Null] {
        assert!(
            matches!(registry.preflight("read", &arguments), Err(execution) if execution.is_error && execution.content.contains("JSON object"))
        );
    }
    assert!(matches!(
        registry.preflight("nope", &json!({})),
        Err(execution) if execution.is_error
    ));
    // read 缺必填 path。
    assert!(matches!(
        registry.preflight("read", &json!({"offset": 1})),
        Err(execution) if execution.is_error
    ));
    // read 未知字段（deny_unknown_fields）。
    assert!(matches!(
        registry.preflight("read", &json!({"path": "a", "surprise": 1})),
        Err(execution) if execution.is_error
    ));
    assert!(matches!(
        registry.preflight("read", &json!({"path": "a"})),
        Ok(PreparedTool::Read(_))
    ));
}

/// 批次并发执行：Started 与返回结果都按模型给定 source order 排列，
/// Ended 随实际完成顺序到达；一个调用失败不阻断其余调用。
#[test]
fn batch_reports_source_order_and_isolates_failures() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::write(dir.path().join("present.txt"), "hello").expect("write fixture");
    let registry = ToolRegistrySnapshot::default();
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
            AgentEvent::ToolExecutionStarted { item_id, .. } => started.push(item_id),
            AgentEvent::ToolExecutionEnded { item_id, .. } => ended.push(item_id),
            _ => {}
        };
        execute_tool_batch(
            &calls,
            dir.path(),
            &cancellation,
            &mut on_event,
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
    assert_eq!(started, vec!["r1", "r2", "r3"], "source order preserved");
    ended.sort();
    assert_eq!(
        ended,
        vec!["r1", "r2", "r3"],
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
    let registry = ToolRegistrySnapshot::default();
    let cancellation = CancellationToken::new();
    let Ok(prepared) = registry.preflight("read", &json!({"path": "big.txt"})) else {
        panic!("valid read args must prepare");
    };
    let execution = prepared.execute(ExecuteContext {
        cwd: dir.path(),
        signal: &cancellation,
        on_update: None,
    });
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

/// 展示预算按实际返回的文本计算：非法 UTF-8 字节经 U+FFFD 替换后可能膨胀三倍。
#[test]
fn read_budget_applies_to_the_replacement_text_it_returns() {
    use crate::tools::truncate::DEFAULT_MAX_BYTES;
    let dir = tempfile::tempdir().unwrap();
    // 20000 个 0xFF 字节是预算的 40%，替换为 U+FFFD 后是 60000 字节。
    std::fs::write(dir.path().join("invalid.txt"), vec![0xFFu8; 20_000]).unwrap();
    let registry = ToolRegistrySnapshot::default();
    let cancellation = CancellationToken::new();
    let Ok(prepared) = registry.preflight("read", &json!({"path": "invalid.txt"})) else {
        panic!("valid read args must prepare");
    };
    let execution = prepared.execute(ExecuteContext {
        cwd: dir.path(),
        signal: &cancellation,
        on_update: None,
    });
    assert!(!execution.is_error);
    let marker = execution
        .content
        .find("…[truncated]")
        .expect("replacement text must respect the byte budget");
    assert!(
        marker <= DEFAULT_MAX_BYTES,
        "returned body of {marker} bytes exceeds the {DEFAULT_MAX_BYTES} byte budget"
    );
    assert!(
        execution.content[..marker]
            .chars()
            .all(|character| character == '\u{fffd}'),
        "the replacement characters themselves must be what was budgeted"
    );
}

#[test]
fn read_records_the_source_range_it_actually_read() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("lines.txt"), "a\nb\nc\n").unwrap();
    std::fs::write(dir.path().join("empty.txt"), "").unwrap();
    let registry = ToolRegistrySnapshot::default();
    let signal = CancellationToken::new();
    let run = |name: &str, args: Value| {
        let Ok(prepared) = registry.preflight(name, &args) else {
            panic!("valid {name} call")
        };
        prepared.execute(ExecuteContext {
            cwd: dir.path(),
            signal: &signal,
            on_update: None,
        })
    };
    // offset 省略、0 与 1 都从第 1 行开始；正文行数不含尾部说明。
    for args in [
        json!({"path": "lines.txt"}),
        json!({"path": "lines.txt", "offset": 0}),
        json!({"path": "lines.txt", "offset": 1}),
    ] {
        assert_eq!(
            run("read", args.clone()).read_source,
            Some(singularity_protocol::ReadSource {
                start_line: 1,
                line_count: 3
            }),
            "{args}"
        );
    }
    let paged = run(
        "read",
        json!({"path": "lines.txt", "offset": 2, "limit": 1}),
    );
    assert_eq!(
        paged.read_source,
        Some(singularity_protocol::ReadSource {
            start_line: 2,
            line_count: 1
        }),
        "分页说明不计入正文行数：{}",
        paged.content
    );
    assert!(paged.content.contains("use offset=3"));
    assert_eq!(
        run("read", json!({"path": "empty.txt"})).read_source,
        Some(singularity_protocol::ReadSource {
            start_line: 1,
            line_count: 0
        })
    );
    // 读取失败与其它工具都没有这份数据。
    assert_eq!(
        run("read", json!({"path": "missing.txt"})).read_source,
        None
    );
    assert_eq!(run("glob", json!({"pattern": "*.txt"})).read_source, None);
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
    let registry = ToolRegistrySnapshot::default();
    let cancellation = CancellationToken::new();
    let read = |offset| {
        let Ok(prepared) =
            registry.preflight("read", &json!({"path":"paged.txt", "offset":offset}))
        else {
            panic!("valid read");
        };
        prepared
            .execute(ExecuteContext {
                cwd: dir.path(),
                signal: &cancellation,
                on_update: None,
            })
            .content
    };
    let page = read(1);
    assert!(page.starts_with(&first));
    assert!(page.contains("use offset=2"));
    assert!(!page.contains("[truncated]"));
    assert_eq!(read(2), format!("{second}\nthird"));
}

#[test]
fn read_honors_its_line_cap_and_returns_a_continuation_offset() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("lines.txt"), "line\n".repeat(2001)).unwrap();
    let registry = ToolRegistrySnapshot::default();
    let signal = CancellationToken::new();
    let read = |offset| {
        let Ok(prepared) = registry.preflight(
            "read",
            &json!({
                "path": "lines.txt", "offset": offset, "limit": 10000
            }),
        ) else {
            panic!("valid read")
        };
        prepared.execute(ExecuteContext {
            cwd: dir.path(),
            signal: &signal,
            on_update: None,
        })
    };
    let first = read(1);
    assert!(!first.is_error);
    assert_eq!(
        first.content.lines().filter(|line| *line == "line").count(),
        2000
    );
    assert!(first.content.contains("use offset=2001"));
    assert_eq!(read(2001).content, "line");
}

/// patch 头部行号是模型唯一能读到的坐标：hunk 从哪一行开始就必须写哪一行。
#[test]
fn edit_patch_header_reports_the_first_context_line() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").expect("write file");
    let registry = ToolRegistrySnapshot::default();
    let cancellation = CancellationToken::new();
    let Ok(prepared) = registry.preflight("read", &json!({"path": "f.txt"})) else {
        panic!("valid read args must prepare");
    };
    let execution = prepared.execute(ExecuteContext {
        cwd: dir.path(),
        signal: &cancellation,
        on_update: None,
    });
    assert!(!execution.is_error, "{}", execution.content);
    let Ok(prepared) = registry.preflight(
        "edit",
        &json!({"path": "f.txt", "oldString": "b", "newString": "B"}),
    ) else {
        panic!("valid edit args must prepare");
    };
    let execution = prepared.execute(ExecuteContext {
        cwd: dir.path(),
        signal: &cancellation,
        on_update: None,
    });
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
        let registry = ToolRegistrySnapshot::default();
        let Ok(read) = registry.preflight("read", &json!({"path":"f.txt"})) else {
            panic!("valid read");
        };
        assert!(!read.execute(context()).is_error);
        let Ok(edit) = registry.preflight(
            "edit",
            &json!({"path":"f.txt", "oldString":old, "newString":new}),
        ) else {
            panic!("valid edit");
        };
        let result = edit.execute(context());
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(std::fs::read(&path).unwrap(), expected.as_bytes());
    }
}

/// 无变化的 edit 只报它真正命中的条件：命中已经确认，唯一原因是归一化行尾后
/// 旧文本与新文本相同；错误里不再出现已被前置检查排除的“文本不存在”。
#[test]
fn an_edit_without_changes_reports_the_normalized_identity() {
    let cases = [
        // 文本不存在：命中检查先拒绝。
        (
            "target
target
",
            "absent",
            "new",
            "Could not find the exact text",
        ),
        // 重复匹配且未声明 replaceAll：唯一性检查先拒绝。
        (
            "dup
dup
",
            "dup",
            "new",
            "occurrences",
        ),
        // 相同替换：归一化后两段文本相同。
        (
            "single
",
            "single",
            "single",
            "identical once LF and CRLF line endings are normalized",
        ),
        // 仅行尾不同：归一化后同样相同。
        (
            "single
",
            "single
",
            "single
",
            "identical once LF and CRLF line endings are normalized",
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
        let registry = ToolRegistrySnapshot::default();
        let Ok(read) = registry.preflight("read", &json!({"path":"f.txt"})) else {
            panic!("valid read");
        };
        assert!(!read.execute(context()).is_error);
        let Ok(edit) = registry.preflight(
            "edit",
            &json!({"path":"f.txt", "oldString":old, "newString":new}),
        ) else {
            panic!("valid edit");
        };
        let result = edit.execute(context());
        assert!(result.is_error, "{old} -> {new} must fail");
        assert!(
            result.content.contains(expected),
            "expected {expected:?} in {:?}",
            result.content
        );
        assert!(
            !result.content.contains("not existing"),
            "the no-change error must not speculate about text that the earlier checks already confirmed"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before.as_bytes(),
            "a rejected edit leaves the file untouched"
        );
    }
}

/// replaceAll 的每个命中块各用自己的行尾，不因文件里同时存在另一种行尾而被
/// 全局规范化；未命中的区域必须逐字节保持原样。
#[test]
fn replace_all_uses_each_match_block_line_ending_and_keeps_other_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.txt");
    // 两个命中块各以 CRLF 与 LF 结束，替换文本沿用各自块内的行尾：CRLF 命中
    // 得到 "new\r\nNEXT"，LF 命中得到 "new\nNEXT"；其余区域逐字节不变。
    let before = "head\nold\r\nkeep\r\nold\nkeep\nend\r\n";
    std::fs::write(&path, before).unwrap();
    let signal = CancellationToken::new();
    let context = || ExecuteContext {
        cwd: dir.path(),
        signal: &signal,
        on_update: None,
    };
    let registry = ToolRegistrySnapshot::default();
    let Ok(read) = registry.preflight("read", &json!({"path":"mixed.txt"})) else {
        panic!("valid read");
    };
    assert!(!read.execute(context()).is_error);
    let Ok(edit) = registry.preflight(
        "edit",
        &json!({
            "path":"mixed.txt",
            "oldString":"old\r\nkeep",
            "newString":"new\nNEXT",
            "replaceAll":true,
        }),
    ) else {
        panic!("valid edit");
    };
    let result = edit.execute(context());
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        "head\nnew\r\nNEXT\r\nnew\nNEXT\nend\r\n".as_bytes()
    );
}

/// 命中块自身不含换行时沿用文件级兜底行尾，同样不能改写未命中的另一种行尾。
#[test]
fn file_fallback_line_ending_does_not_normalize_other_regions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fallback.txt");
    std::fs::write(&path, "head\r\ntail\n").unwrap();
    let signal = CancellationToken::new();
    let context = || ExecuteContext {
        cwd: dir.path(),
        signal: &signal,
        on_update: None,
    };
    let registry = ToolRegistrySnapshot::default();
    let Ok(read) = registry.preflight("read", &json!({"path":"fallback.txt"})) else {
        panic!("valid read");
    };
    assert!(!read.execute(context()).is_error);
    let Ok(edit) = registry.preflight(
        "edit",
        &json!({
            "path":"fallback.txt",
            "oldString":"head",
            "newString":"HEAD\nADDED",
        }),
    ) else {
        panic!("valid edit");
    };
    let result = edit.execute(context());
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        "HEAD\r\nADDED\r\ntail\n".as_bytes()
    );
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
    let registry = ToolRegistrySnapshot::default();
    let Ok(read) = registry.preflight("read", &json!({"path":"f.txt"})) else {
        panic!("valid read");
    };
    assert!(!read.execute(context()).is_error);
    for (old, replace_all, expected_error) in [
        ("a\nb", false, "2 occurrences"),
        ("a \nb", true, "Could not find"),
        ("a\nb", true, ""),
    ] {
        let Ok(edit) = registry.preflight(
            "edit",
            &json!({"path":"f.txt", "oldString":old, "newString":"A\nB", "replaceAll":replace_all}),
        ) else {
            panic!("valid edit");
        };
        let result = edit.execute(context());
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

/// 判定次序与文案：空 oldString → 未命中 → 多重命中 → 归一化后无变化。
/// 行尾归一化与「规范化偏移映射回原文」由走真实入口的 edit 用例覆盖。
#[test]
fn prepare_edit_reports_each_rejection_reason() {
    let prepare = |content: &str, old: &str, new: &str, replace_all: bool| {
        super::edit::prepare_edit(
            "f.txt",
            content,
            &super::edit::EditArgs {
                path: "f.txt".into(),
                old_string: old.into(),
                new_string: new.into(),
                replace_all,
            },
        )
    };
    for (old, new, replace_all, expected) in [
        ("", "x", false, "must not be empty"),
        ("absent", "x", false, "Could not find"),
        ("dup", "x", false, "2 occurrences"),
        ("same", "same", false, "identical once LF and CRLF"),
    ] {
        let error = prepare("dup\ndup\nsame\n", old, new, replace_all).unwrap_err();
        assert!(error.contains(expected), "{old:?} -> {expected:?}: {error}");
    }
}

/// 文件编辑与整文件重写无需先调用 read 工具即可工作。
#[test]
fn mutations_work_without_a_prior_read_tool_call() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::write(dir.path().join("f.txt"), "original\n").expect("write file");
    let registry = ToolRegistrySnapshot::default();
    let cancellation = CancellationToken::new();
    let Ok(prepared) = registry.preflight(
        "edit",
        &json!({"path": "f.txt", "oldString": "original", "newString": "clobbered"}),
    ) else {
        panic!("valid edit args must prepare");
    };
    let execution = prepared.execute(ExecuteContext {
        cwd: dir.path(),
        signal: &cancellation,
        on_update: None,
    });
    assert!(!execution.is_error, "{}", execution.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        "clobbered\n"
    );
    let Ok(prepared) =
        registry.preflight("write", &json!({"path": "f.txt", "content": "clobbered\n"}))
    else {
        panic!("valid write args must prepare");
    };
    let execution = prepared.execute(ExecuteContext {
        cwd: dir.path(),
        signal: &cancellation,
        on_update: None,
    });
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
    let registry = ToolRegistrySnapshot::default();
    let cancellation = CancellationToken::new();
    let execute = |name: &str, args: Value| {
        let Ok(prepared) = registry.preflight(name, &args) else {
            panic!("valid tool args");
        };
        prepared.execute(ExecuteContext {
            cwd: dir.path(),
            signal: &cancellation,
            on_update: None,
        })
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
                let registry = ToolRegistrySnapshot::default();
                let Ok(prepared) = registry.preflight(
                    "edit",
                    &json!({"path":path, "oldString":old, "newString":new}),
                ) else {
                    panic!("valid arguments")
                };
                barrier.wait();
                prepared.execute(ExecuteContext {
                    cwd,
                    signal: &CancellationToken::new(),
                    on_update: None,
                })
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

/// bash 以真实进程退出状态与超时终止向模型报告失败；错误标记与文案是
/// 模型判断命令结果的依据。
#[test]
fn bash_reports_nonzero_exit_and_timeout_as_model_visible_failures() {
    let dir = tempfile::tempdir().unwrap();
    let cancellation = CancellationToken::new();
    let context = ExecuteContext {
        cwd: dir.path(),
        signal: &cancellation,
        on_update: None,
    };

    let failed = super::bash::execute(
        &super::bash::BashArgs {
            command: "exit 7".into(),
            timeout_ms: None,
        },
        context,
    );
    assert!(failed.is_error);
    assert!(
        failed.content.contains("Command exited with code 7"),
        "{}",
        failed.content
    );

    let timed_out = super::bash::execute(
        &super::bash::BashArgs {
            command: "sleep 30".into(),
            timeout_ms: Some(300),
        },
        ExecuteContext {
            cwd: dir.path(),
            signal: &cancellation,
            on_update: None,
        },
    );
    assert!(timed_out.is_error);
    assert!(
        timed_out.content.contains("Command timed out after 300 ms"),
        "{}",
        timed_out.content
    );
}

/// 取消与超时、正常退出共用同一处整树终止与有界回收；取消前已产生的输出仍是
/// 模型可见结果的一部分，且整个收尾必须是有界的。
#[test]
fn bash_cancellation_terminates_the_tree_and_keeps_output_produced_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let cancellation = CancellationToken::new();
    let cancel_from_update = cancellation.clone();
    let mut on_update = move |tail: String| {
        if tail.contains("started") {
            cancel_from_update.cancel();
        }
    };
    let started = std::time::Instant::now();
    let result = super::bash::execute(
        &super::bash::BashArgs {
            command: "echo started; sleep 30".into(),
            // 取消未被观察到时的兜底界：本调用不得拖到默认的 300 秒。
            timeout_ms: Some(15_000),
        },
        ExecuteContext {
            cwd: dir.path(),
            signal: &cancellation,
            on_update: Some(&mut on_update),
        },
    );
    assert!(result.is_error, "{}", result.content);
    assert!(
        result.content.contains("Operation aborted"),
        "{}",
        result.content
    );
    assert!(result.content.contains("started"), "{}", result.content);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "termination and reclamation must be bounded, took {:?}",
        started.elapsed()
    );
}

/// 命令进程已退出、但后台成员仍持有管道写端时，两个排空窗口在各自期限收敛
/// 并给出截断提示；命令本身成功，截断仅为信息提示而非错误。
#[test]
fn bash_background_writer_holding_the_pipe_is_reported_as_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let result = super::bash::execute(
        &super::bash::BashArgs {
            command: "sleep 30 & echo done".into(),
            timeout_ms: None,
        },
        ExecuteContext {
            cwd: dir.path(),
            signal: &CancellationToken::new(),
            on_update: None,
        },
    );
    assert!(result.content.contains("done"), "{}", result.content);
    assert!(
        result
            .content
            .contains("[output truncated: a background process is still writing]"),
        "{}",
        result.content
    );
    assert!(!result.is_error, "{}", result.content);
}

/// 单行超字节上限时截断说明给出真实的展示量与行位置，并且 spill 保存的是
/// 完整输出而不是被裁剪的尾部；terminated 情形覆盖长行以换行结束（旧说明
/// 会把该显示行报成 0 B）。
#[test]
fn bash_long_line_truncation_reports_real_amounts_and_spills_complete_output() {
    use super::truncate::{DEFAULT_MAX_BYTES, format_size};
    let dir = tempfile::tempdir().unwrap();
    let line_bytes = DEFAULT_MAX_BYTES + 100;
    for (label, terminator) in [("terminated line", "\n"), ("unterminated line", "")] {
        let command = format!("head -c {line_bytes} /dev/zero | tr '\\0' x{terminator}");
        let result = super::bash::execute(
            &super::bash::BashArgs {
                command,
                timeout_ms: None,
            },
            ExecuteContext {
                cwd: dir.path(),
                signal: &CancellationToken::new(),
                on_update: None,
            },
        );
        assert!(!result.is_error, "{label}: {}", result.content);
        assert!(
            result.content.contains(&format!(
                "[Showing the last {} of output, ending at line 1.]",
                format_size(DEFAULT_MAX_BYTES)
            )),
            "{label}: {}",
            result.content
        );
        assert!(!result.content.contains("line is"), "{label}");
        let Some((_, path)) = result.content.split_once("Full output: ") else {
            panic!("{label}: truncated output must spill: {}", result.content);
        };
        let path = path.lines().next().unwrap().trim();
        let saved = std::fs::read_to_string(path).unwrap();
        // 行尾是命令的真实输出：有换行时只允许末尾多出行终止符，正文逐字节不变。
        let body = saved.trim_end_matches(['\r', '\n']);
        assert_eq!(body.len(), line_bytes, "{label}");
        assert!(body.bytes().all(|byte| byte == b'x'), "{label}");
    }
}

/// 子进程归属是启动契约，不是时序巧合：命令一执行就派生的后代必须与主进程
/// 一起被终止，包括主进程提前退出留下的孤儿。
///
/// 探针进程由被测 shell 自己以 `sleep` 派生，因此断言依赖的是真实的系统进程
/// 树关系，而不是测试对实现的建模。
#[cfg(windows)]
mod process_tree {
    use super::*;

    /// 判定一个进程是否仍然存活。
    ///
    /// 用 `tasklist` 而不是 `OpenProcess`：它的输出是普通文本，测试因此不必
    /// 复制生产代码拥有的句柄处理。
    fn is_alive(pid: u32) -> bool {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .expect("tasklist runs");
        let text = String::from_utf8_lossy(&output.stdout);
        text.contains(&format!("\"{pid}\""))
    }

    /// 等到进程从 tasklist 里消失（小上界内）。
    ///
    /// 终止本身是同步的：`TerminateJobObject` 返回时作业内已经没有任何活动进程
    /// （实测活动进程数在同一毫秒内归零）。但 tasklist 的可见性会滞后几毫秒——
    /// 已终止的进程对象仍会被列出一小会儿——所以这里轮询一个上界，而不是把
    /// 「正在从列表里消失」当成「仍在运行」。
    fn waits_until_gone(pid: u32) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if !is_alive(pid) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn sleep_seconds() -> &'static str {
        "30"
    }

    /// 命令脚本：打印一个后台 `sleep` 的 PID，主命令立即退出，使该后代成为
    /// 孤儿并继续持有 stdout 写端。
    fn orphan_script() -> String {
        // shell 的 $! 是该 sleep 的 PID；主命令不再等待它。
        format!(
            "sleep {} >/dev/null 2>&1 & echo PID:$! ; exit 0",
            sleep_seconds()
        )
    }

    fn child_pid(output: &str) -> u32 {
        output
            .lines()
            .find_map(|line| line.trim().strip_prefix("PID:"))
            .expect("the command reports its detached child pid")
            .trim()
            .parse()
            .expect("the reported pid is numeric")
    }

    /// 正常退出路径：命令结束时整树都被终止，`&` 派生的后代不残留。
    #[test]
    fn a_detached_descendant_does_not_survive_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let result = super::super::bash::execute(
            &super::super::bash::BashArgs {
                command: orphan_script(),
                timeout_ms: None,
            },
            ExecuteContext {
                cwd: dir.path(),
                signal: &CancellationToken::new(),
                on_update: None,
            },
        );
        let pid = child_pid(&result.content);
        // 终止是同步承诺：命令返回时该后代已经被终止，只等 tasklist 把它移出列表。
        assert!(
            waits_until_gone(pid),
            "a descendant created after launch must belong to the call's job; \
             pid {pid} survived:\n{}",
            result.content
        );
    }

    /// 子进程从第一次执行起就能派生子进程，说明它在运行前已经可执行、且执行
    /// 前已被本次调用拥有——归属不是「运行后再补绑」。
    #[test]
    fn a_descendant_created_immediately_still_belongs_to_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let result = super::super::bash::execute(
            &super::super::bash::BashArgs {
                // 在第一条命令里立刻派生，不给启动边界留任何调度空窗。
                command: format!(
                    "sleep {} >/dev/null 2>&1 & echo PID:$!; wait",
                    sleep_seconds()
                ),
                // 超时只需晚于 shell 启动：后台 sleep 会一直持住命令，超时与整树
                // 终止的断言都不变；1s 在并发测试下会早于 Git Bash 打印 PID 行。
                timeout_ms: Some(3_000),
            },
            ExecuteContext {
                cwd: dir.path(),
                signal: &CancellationToken::new(),
                on_update: None,
            },
        );
        let pid = child_pid(&result.content);
        assert!(
            result.is_error,
            "the wait exceeds the timeout: {}",
            result.content
        );
        assert!(
            waits_until_gone(pid),
            "the first-created descendant must be terminated with the tree; \
             pid {pid} survived:\n{}",
            result.content
        );
    }

    /// 取消路径同样按树终止，且取消前产生的输出仍保留；收尾有界。
    #[test]
    fn cancellation_terminates_a_descendant_and_keeps_earlier_output() {
        let dir = tempfile::tempdir().unwrap();
        let cancellation = CancellationToken::new();
        let cancel_from_update = cancellation.clone();
        let mut on_update = move |tail: String| {
            if tail.contains("PID:") {
                cancel_from_update.cancel();
            }
        };
        let started = std::time::Instant::now();
        let result = super::super::bash::execute(
            &super::super::bash::BashArgs {
                command: format!(
                    "sleep {} >/dev/null 2>&1 & echo PID:$!; sleep 30",
                    sleep_seconds()
                ),
                timeout_ms: Some(15_000),
            },
            ExecuteContext {
                cwd: dir.path(),
                signal: &cancellation,
                on_update: Some(&mut on_update),
            },
        );
        let pid = child_pid(&result.content);
        assert!(
            result.content.contains("Operation aborted"),
            "{}",
            result.content
        );
        assert!(waits_until_gone(pid), "pid {pid} survived cancellation");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "termination and reclamation must be bounded, took {:?}",
            started.elapsed()
        );
    }

    /// 主进程被超时终止后，管道必须闭合到 EOF：整树都没有遗留写端，读取侧
    /// 不靠宽限窗口也能收敛，输出不被误报为截断。
    #[test]
    fn a_timed_out_tree_closes_its_pipes_without_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let result = super::super::bash::execute(
            &super::super::bash::BashArgs {
                command: format!("echo hello; sleep {} & sleep 30", sleep_seconds()),
                timeout_ms: Some(1_000),
            },
            ExecuteContext {
                cwd: dir.path(),
                signal: &CancellationToken::new(),
                on_update: None,
            },
        );
        assert!(result.content.contains("hello"), "{}", result.content);
        assert!(
            result.content.contains("Command timed out after 1000 ms"),
            "{}",
            result.content
        );
        assert!(
            !result
                .content
                .contains("[output truncated: a background process is still writing]"),
            "the whole tree is terminated, so no writer keeps the pipe open: {}",
            result.content
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "the drain windows converge once every writer is gone, took {:?}",
            started.elapsed()
        );
    }
}

/// 遍历顺序与排除目录是确定性契约；回调要求停止时整棵树立即停止，包括正在
/// 递归的嵌套目录，而不是只结束当前目录。
#[test]
fn walk_reports_files_in_sorted_order_and_stops_the_whole_tree() {
    use super::walk::{WalkControl, walk_files};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    for name in [
        "b.txt",
        "a.txt",
        "nested/c.txt",
        "nested/d.txt",
        ".git/ignored.txt",
    ] {
        std::fs::write(dir.path().join(name), "").unwrap();
    }
    let signal = CancellationToken::new();
    let collect = |stop_after: usize| {
        let mut seen = Vec::new();
        walk_files(dir.path(), &signal, &mut |relative| {
            seen.push(singularity_core::display_path(&relative));
            if seen.len() == stop_after {
                WalkControl::Stop
            } else {
                WalkControl::Continue
            }
        })
        .unwrap();
        seen
    };
    assert_eq!(
        collect(usize::MAX),
        vec!["a.txt", "b.txt", "nested/c.txt", "nested/d.txt"],
        "目录条目排序后进入，.git 子树被排除"
    );
    assert_eq!(
        collect(3),
        vec!["a.txt", "b.txt", "nested/c.txt"],
        "停止发生在嵌套目录内部，同目录的后续文件不再被访问"
    );
}

/// 回调内触发的取消同样立即停止整棵树：后续条目与嵌套目录都不再被访问。
#[test]
fn walk_stops_after_a_cancellation_raised_inside_the_callback() {
    use super::walk::{WalkControl, walk_files};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    std::fs::write(dir.path().join("a.txt"), "").unwrap();
    std::fs::write(dir.path().join("nested/b.txt"), "").unwrap();
    let signal = CancellationToken::new();
    let cancel_from_callback = signal.clone();
    let mut seen = Vec::new();
    walk_files(dir.path(), &signal, &mut |relative| {
        seen.push(singularity_core::display_path(&relative));
        cancel_from_callback.cancel();
        WalkControl::Continue
    })
    .unwrap();
    assert_eq!(seen, vec!["a.txt"]);
}

/// 根目录本身读不到是真实 I/O 失败，不降级成「没有文件」。
#[test]
fn walk_reports_an_unreadable_root_as_an_error() {
    use super::walk::{WalkControl, walk_files};
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    let error = match walk_files(&missing, &CancellationToken::new(), &mut |_| {
        WalkControl::Continue
    }) {
        Ok(_) => panic!("a missing root must not look like an empty directory"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}
