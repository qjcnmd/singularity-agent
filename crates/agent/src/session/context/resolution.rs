use super::*;

pub(super) fn resolve_context_entries(session: &SessionData) -> Vec<ContextPosition> {
    let mut context: Vec<ContextPosition> = Vec::new();
    for (entry_index, entry) in session.entries().iter().enumerate() {
        match entry {
            SessionEntry::Compaction { compaction, .. } => {
                let index = context
                    .iter()
                    .position(|candidate| {
                        session.entries()[candidate.index].id() == compaction.first_kept_entry_id
                    })
                    .expect("compaction retains an active context entry");
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
                let original = context
                    .iter_mut()
                    .find(|candidate| session.entries()[candidate.index].id() == entry_id)
                    .expect("pruning references an active tool result");
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
    context
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
