use super::*;

pub(super) fn resolve_context_entries(session: &SessionData) -> Vec<ContextPosition> {
    let mut context: Vec<ContextPosition> = Vec::new();
    for index in 0..session.entries().len() {
        apply_context_entry(&mut context, ContextPosition { index, pruned_index: None }, session);
    }
    context
}

/// 将已提交的日志作用于上下文位置；追加与重新打开共用替换、Skill 相邻和工具结果排序规则。
/// 剪枝时返回替换前的位置，供追加路径差量更新估价。
pub(super) fn apply_context_entry(
    context: &mut Vec<ContextPosition>,
    position: ContextPosition,
    session: &SessionData,
) -> Option<ContextPosition> {
    match position.entry(session) {
        SessionEntry::Compaction { compaction, .. } => {
            let index = context
                .iter()
                .position(|candidate| candidate.entry(session).id() == compaction.first_kept_entry_id)
                .expect("compaction retains an active context entry");
            context.drain(..index);
            context.insert(0, position);
            return None;
        }
        SessionEntry::Record {
            record: LedgerRecord::ToolResultPruned { entry_id, .. },
            ..
        } => {
            let original = context
                .iter_mut()
                .find(|candidate| candidate.entry(session).id() == entry_id)
                .expect("pruning references an active tool result");
            let previous = *original;
            original.pruned_index = Some(position.index);
            return Some(previous);
        }
        entry if !is_context_entry(entry) => return None,
        _ => {}
    }
    // 手动 Skill 记录在触发它的用户消息之后落盘；模型视图把它放在该输入之前，
    // 保留用户输入作为本轮最后的指令。文件指令不在历史内，不影响这个邻接关系。
    if matches!(
        &session.entries()[position.index],
        SessionEntry::Record {
            record: LedgerRecord::SkillInstructions { .. },
            ..
        }
    ) && context.last().is_some_and(|last| {
        matches!(last.entry(session), SessionEntry::Message { message: AgentMessage::User { .. }, .. })
    }) {
        context.insert(context.len() - 1, position);
        return None;
    }
    if let Some(insert_at) = context_insertion_index(context, &session.entries()[position.index], session) {
        context.insert(insert_at, position);
    } else {
        context.push(position);
    }
    None
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
        context.iter().enumerate().rev().find_map(|(index, candidate)| {
            let SessionEntry::Message { message, .. } = &session.entries()[candidate.index] else {
                return None;
            };
            let ordinal = message.tool_calls().position(|call| call.tool_call_id == *call_id)?;
            Some((index, message, ordinal))
        })?;
    Some(
        context
            .iter()
            .enumerate()
            .skip(assistant_index + 1)
            .find_map(|(index, candidate)| {
                let SessionEntry::Message { message, .. } = &session.entries()[candidate.index] else {
                    return None;
                };
                let id = message.tool_call_id()?;
                let existing = assistant.tool_calls().position(|call| call.tool_call_id == *id)?;
                (existing > ordinal).then_some(index)
            })
            .unwrap_or(context.len()),
    )
}
