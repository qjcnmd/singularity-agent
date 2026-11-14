use super::*;

pub(super) fn conversation_error(error: ConversationError) -> RpcError {
    match error {
        ConversationError::Busy => session_busy(),
        other => RpcError::new(other),
    }
}

pub(super) fn catalog_error(error: CatalogError) -> RpcError {
    match error {
        CatalogError::NotFound(_) => RpcError::new("任务不存在或已删除。"),
        other => RpcError::new(other),
    }
}

pub(super) fn session_busy() -> RpcError {
    RpcError::new("当前任务正在处理另一项操作。")
}
