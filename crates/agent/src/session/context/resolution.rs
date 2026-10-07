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
    // 最近一次声明该调用的 assistant 决定工具结果的顺序。
    for (assistant_index, candidate) in context.iter().enumerate().rev() {
        let SessionEntry::Message { message: assistant, .. } = candidate.entry(session) else {
            continue;
        };
        let Some(call_ordinal) = assistant.tool_calls().position(|call| call.tool_call_id == *call_id) else {
            continue;
        };
        for (result_index, candidate) in context.iter().enumerate().skip(assistant_index + 1) {
            let SessionEntry::Message { message: result, .. } = candidate.entry(session) else {
                continue;
            };
            let Some(result_call_id) = result.tool_call_id() else {
                continue;
            };
            let Some(result_ordinal) =
                assistant.tool_calls().position(|call| call.tool_call_id == *result_call_id)
            else {
                continue;
            };
            if result_ordinal > call_ordinal {
                return Some(result_index);
            }
        }
        return Some(context.len());
    }
    None
}
