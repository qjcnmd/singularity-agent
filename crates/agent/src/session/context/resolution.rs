use super::super::format::SessionError;
use super::*;

pub(super) fn resolve_context_entries(session: &SessionData) -> Result<Vec<ContextPosition>> {
    let mut context: Vec<ContextPosition> = Vec::new();
    for (entry_index, entry) in session.entries().iter().enumerate() {
        match entry {
            SessionEntry::Compaction { compaction, .. } => {
                let index = context
                    .iter()
                    .position(|candidate| {
                        session.entries()[candidate.index].id() == compaction.first_kept_entry_id
                    })
                    .ok_or_else(|| SessionError::LedgerCorrupt {
                        reason: "invalid_compaction_anchor".into(),
                        detail: format!(
                            "compaction {} references an inactive anchor {}",
                            entry.id(),
                            compaction.first_kept_entry_id
                        ),
                    })?;
                if !entries_balanced(
                    context[..index]
                        .iter()
                        .map(|position| &session.entries()[position.index]),
                ) {
                    return Err(SessionError::LedgerCorrupt {
                        reason: "invalid_compaction_anchor".into(),
                        detail: "compaction splits a tool call/result pair".into(),
                    });
                }
                context.drain(..index);
                context.insert(
                    0,
                    ContextPosition {
                        index: entry_index,
                        pruned_index: None,
                    },
                );
            }
            SessionEntry::Record {
                record: LedgerRecord::ToolResultPruned { entry_id, .. },
                ..
            } => {
                let original = context.iter_mut().find(|candidate| {
                    let entry = &session.entries()[candidate.index];
                    entry.id() == entry_id
                        && matches!(
                            entry,
                            SessionEntry::Message {
                                message: AgentMessage::ToolResult { .. },
                                ..
                            }
                        )
                });
                let Some(original) = original else {
                    return Err(SessionError::LedgerCorrupt {
                        reason: "invalid_prune_anchor".into(),
                        detail: format!("pruning references inactive tool result {entry_id}"),
                    });
                };
                original.pruned_index = Some(entry_index);
            }
            _ if is_context_entry(entry) => {
                push_context_entry(
                    &mut context,
                    ContextPosition {
                        index: entry_index,
                        pruned_index: None,
                    },
                    session,
                );
            }
            // 操作记录、请求观测与元数据都不进入模型上下文。
            _ => {}
        }
    }
    Ok(context)
}

/// 完成顺序是持久事实，但 provider 重放时要按 assistant 声明的调用顺序排列
/// 同级结果。实时执行和重新打开会话都套用同一套投影。
pub(super) fn push_context_entry(
    context: &mut Vec<ContextPosition>,
    position: ContextPosition,
    session: &SessionData,
) {
    // 手动 Skill 记录在触发它的用户消息之后落盘；模型视图把它放在该输入之前，
    // 保留用户输入作为本轮最后的指令。文件指令不在历史内，不影响这个邻接关系。
    if matches!(
        &session.entries()[position.index],
        SessionEntry::Record {
            record: LedgerRecord::SkillInstructions { .. },
            ..
        }
    ) && context.last().is_some_and(|last| {
        matches!(
            last.entry(session),
            SessionEntry::Message {
                message: AgentMessage::User { .. },
                ..
            }
        )
    }) {
        context.insert(context.len() - 1, position);
        return;
    }
    if let Some(insert_at) =
        context_insertion_index(context, &session.entries()[position.index], session)
    {
        context.insert(insert_at, position);
    } else {
        context.push(position);
    }
}

fn context_insertion_index(
    context: &[ContextPosition],
    entry: &SessionEntry,
    session: &SessionData,
) -> Option<usize> {
    let SessionEntry::Message { message, .. } = entry else {
        return None;
    };
    let call_id = message.tool_call_id()?;
    // 声明这个调用的 assistant 就是顺序来源：借用它的工具列表，一次查找同时得到
    // 该调用在其中的序号，不必另建 ID 数组。
    let (assistant_index, assistant, ordinal) =
        context
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, candidate)| {
                let SessionEntry::Message { message, .. } = &session.entries()[candidate.index]
                else {
                    return None;
                };
                let ordinal = message
                    .tool_calls()
                    .position(|call| call.tool_call_id == *call_id)?;
                Some((index, message, ordinal))
            })?;
    Some(
        context
            .iter()
            .enumerate()
            .skip(assistant_index + 1)
            .find_map(|(index, candidate)| {
                let SessionEntry::Message { message, .. } = &session.entries()[candidate.index]
                else {
                    return None;
                };
                let id = message.tool_call_id()?;
                let existing = assistant
                    .tool_calls()
                    .position(|call| call.tool_call_id == *id)?;
                (existing > ordinal).then_some(index)
            })
            .unwrap_or(context.len()),
    )
}

/// 按日志顺序吸收一条条目，推进工具配对状态。None 表示这个结果找不到对应的待配对
/// 调用（孤立结果，此后任何更长的前缀都不可能闭合）；Some 表示当前前缀末尾是否
/// 已经没有未配对的调用。
pub(super) fn absorb_tool_pairing<'a>(
    pending: &mut std::collections::HashSet<&'a str>,
    entry: &'a SessionEntry,
) -> Option<bool> {
    if let SessionEntry::Message { message, .. } = entry {
        pending.extend(message.tool_calls().map(|call| call.tool_call_id.as_str()));
        if let crate::message::AgentMessage::ToolResult { tool_call_id, .. } = message
            && !pending.remove(tool_call_id.as_str())
        {
            return None;
        }
    }
    Some(pending.is_empty())
}

fn entries_balanced<'a>(entries: impl IntoIterator<Item = &'a SessionEntry>) -> bool {
    let mut pending = std::collections::HashSet::new();
    entries
        .into_iter()
        .all(|entry| absorb_tool_pairing(&mut pending, entry).is_some())
        && pending.is_empty()
}
