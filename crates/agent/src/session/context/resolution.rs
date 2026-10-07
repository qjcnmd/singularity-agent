use super::*;

pub(super) fn resolve_context_entries(session: &SessionData) -> Vec<ContextPosition> {
    let mut context: Vec<ContextPosition> = Vec::new();
    for index in 0..session.entries().len() {
        apply_context_entry(&mut context, ContextPosition { index, pruned_index: None }, session);
    }
    context
}

/// 按账本顺序应用摘要、剪枝和工具结果排序规则。
fn apply_context_entry(context: &mut Vec<ContextPosition>, position: ContextPosition, session: &SessionData) {
    match position.entry(session) {
        SessionEntry::Compaction { compaction, .. } => {
            let index = context
                .iter()
                .position(|candidate| candidate.entry(session).id() == compaction.first_kept_entry_id)
                .expect("compaction retains an active context entry");
            context.drain(..index);
            context.insert(0, position);
            return;
        }
        SessionEntry::Record {
            record: LedgerRecord::ToolResultPruned { entry_id, .. },
            ..
        } => {
            let original = context
                .iter_mut()
                .find(|candidate| candidate.entry(session).id() == entry_id)
                .expect("pruning references an active tool result");
            original.pruned_index = Some(position.index);
            return;
        }
        entry if !is_context_entry(entry) => return,
        _ => {}
    }
    if let Some(insert_at) = context_insertion_index(context, &session.entries()[position.index], session) {
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
