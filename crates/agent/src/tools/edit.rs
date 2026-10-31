//! 单文件批量精确替换；同一目标在整个进程内依次读改写。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::registry::{ABORTED_MESSAGE, ToolExecution, error_result};

static FILE_EDITS: LazyLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditArgs {
    path: String,
    edits: Vec<Replacement>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Replacement {
    old_text: String,
    new_text: String,
}

pub(crate) fn spec() -> singularity_model::ModelToolSchema {
    singularity_model::ModelToolSchema {
        name: "edit".into(),
        description: "Edit an existing UTF-8 text file using one or more exact replacements. Each old_text must be nonempty and unique in the original file; edits must not overlap. All replacements are checked before writing once. Read the relevant content first. LF and CRLF match equivalently; other whitespace and characters must match exactly. Use an empty new_text to delete text. Keep enough context to identify each location. Combine independent changes to one file in a single call; do not issue dependent edits concurrently.".into(),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Existing text file, relative to the working directory or absolute" },
                "edits": {
                    "type": "array", "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_text": { "type": "string", "minLength": 1 },
                            "new_text": { "type": "string" }
                        },
                        "required": ["old_text", "new_text"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        }),
    }
}

pub(crate) async fn execute(args: EditArgs, cwd: PathBuf, signal: CancellationToken) -> ToolExecution {
    if signal.is_cancelled() {
        return error_result(ABORTED_MESSAGE);
    }
    if args.edits.is_empty() {
        return error_result("edits must contain at least one replacement");
    }
    let target = cwd.join(&args.path);
    let target = match tokio::task::spawn_blocking(move || target.canonicalize())
        .await
        .expect("tool worker completes while the runtime is running")
    {
        Ok(target) => target,
        Err(error) => return error_result(format!("Could not edit {}: {error}", args.path)),
    };
    let key = singularity_core::workspace_key(&target);
    let lock = {
        let mut files = FILE_EDITS.lock().expect("file edit registry lock poisoned");
        let lock = files.get(&key).and_then(Weak::upgrade).unwrap_or_default();
        files.insert(key, Arc::downgrade(&lock));
        lock
    };
    let result = tokio::select! {
        biased;
        _ = signal.cancelled() => error_result(ABORTED_MESSAGE),
        guard = Arc::clone(&lock).lock_owned() => {
            // 工作线程持有锁直到写入结束，停止不能提前放行下一个编辑。
            tokio::task::spawn_blocking(move || {
                let _guard = guard;
                execute_locked(&args, &target, &signal)
                    .unwrap_or_else(error_result)
            }).await.expect("tool worker completes while the runtime is running")
        }
    };
    drop(lock);
    FILE_EDITS
        .lock()
        .expect("file edit registry lock poisoned")
        .retain(|_, lock| lock.strong_count() > 0);
    result
}

fn execute_locked(
    args: &EditArgs,
    target: &Path,
    signal: &CancellationToken,
) -> Result<ToolExecution, String> {
    if signal.is_cancelled() {
        return Err(ABORTED_MESSAGE.into());
    }
    let content = std::fs::read_to_string(target)
        .map_err(|error| format!("Could not read {} as UTF-8 text: {error}", args.path))?;
    if content.contains('\0') {
        return Err(format!("{} is not a text file", args.path));
    }
    let source = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let view = source.replace("\r\n", "\n");
    // 只记录被折叠的 CR，匹配位置映射回原字节，避免重写未修改区域的换行。
    let removed: Vec<_> = source
        .match_indices("\r\n")
        .enumerate()
        .map(|(index, (offset, _))| offset - index)
        .collect();
    let source_offset = |offset| offset + removed.partition_point(|removed| *removed < offset);
    let mut matches = Vec::with_capacity(args.edits.len());
    for (index, edit) in args.edits.iter().enumerate() {
        let old = edit.old_text.replace("\r\n", "\n");
        if old.is_empty() {
            return Err(format!("edits[{index}].old_text must not be empty"));
        }
        let start = view.find(&old).ok_or_else(|| {
            format!(
                "edits[{index}].old_text was not found in {}. Read the current text and retry.",
                args.path
            )
        })?;
        let next = start + old.chars().next().expect("old text is nonempty").len_utf8();
        if view[next..].contains(&old) {
            return Err(format!(
                "edits[{index}].old_text matches more than once in {}. Include more surrounding context.",
                args.path
            ));
        }
        matches.push((source_offset(start), source_offset(start + old.len()), index));
    }
    matches.sort_unstable();
    for pair in matches.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(format!(
                "edits[{}] and edits[{}] overlap. Merge them into one replacement.",
                pair[0].2, pair[1].2
            ));
        }
    }
    let mut updated = String::with_capacity(content.len());
    if content.starts_with('\u{feff}') {
        updated.push('\u{feff}');
    }
    let mut cursor = 0;
    for (start, end, index) in matches {
        updated.push_str(&source[cursor..start]);
        let region = &source[start..end];
        let replacement = args.edits[index].new_text.replace("\r\n", "\n");
        let ending = if region.contains("\r\n") || (!region.contains('\n') && source.contains("\r\n")) {
            "\r\n"
        } else {
            "\n"
        };
        updated.push_str(&replacement.replace('\n', ending));
        cursor = end;
    }
    updated.push_str(&source[cursor..]);
    if updated == content {
        return Err("No changes to apply: the replacements leave the file unchanged".into());
    }
    if signal.is_cancelled() {
        return Err(ABORTED_MESSAGE.into());
    }
    // 提交开始后报告真实写入结果，不能把已完成的修改改报为取消。
    singularity_core::write_bytes_atomic(target, updated.as_bytes())
        .map_err(|error| format!("Could not write {}: {error}", args.path))?;
    Ok(super::edit_diff::result(&args.path, &content, &updated))
}
