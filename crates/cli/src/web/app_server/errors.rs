use super::*;

pub(super) fn turn_terminal(
    result: Result<singularity_runtime::TurnOutcome, ConversationError>,
) -> SessionTerminalSnapshot {
    match result {
        Ok(outcome) => SessionTerminalSnapshot {
            source: SessionTerminalSource::Turn,
            status: outcome.turn_status,
            message: outcome.error.map(|error| error.message),
        },
        Err(error) => SessionTerminalSnapshot {
            source: SessionTerminalSource::Turn,
            status: TurnStatus::Failed,
            message: Some(error.to_string()),
        },
    }
}

/// 「会话不属于所选 Workspace」只有这一种错误形状：热 slot 的校验和会话恢复
/// 路径的失败共用同一份公开分类和引导。
pub(super) fn session_scope_conflict() -> RpcError {
    RpcError::new(
        RpcErrorCode::Conflict,
        "Session 不属于所选 Workspace。",
        "刷新工作台并从所属 Workspace 打开该 Session。",
    )
}

pub(crate) fn invalid_request(message: impl Into<String>) -> RpcError {
    RpcError::new(RpcErrorCode::InvalidRequest, message, "检查输入后重试。")
}

pub(super) fn internal_error(message: impl Into<String>) -> RpcError {
    RpcError::new(
        RpcErrorCode::Internal,
        message,
        "刷新工作台；若问题持续，检查启动终端中的错误。",
    )
}

pub(super) fn configuration_error(message: impl Into<String>) -> RpcError {
    RpcError::new(
        RpcErrorCode::ConfigurationInvalid,
        message,
        "打开模型设置并修正配置。",
    )
}

pub(super) fn model_error(error: singularity_model::ProviderError) -> RpcError {
    match error.code.as_deref() {
        Some(singularity_model::CREDENTIAL_SAVE_FAILED_CODE) => {
            partially_saved(error, "重试保存 API 密钥。")
        }
        Some(singularity_model::CREDENTIAL_DELETE_FAILED_CODE) => {
            partially_saved(error, "重试删除 API 密钥。")
        }
        _ => configuration_error(error.to_string()),
    }
}

/// 配置已经部分生效、剩下凭据没写成功：界面按同一分类提示重试这次操作。
pub(super) fn partially_saved(error: singularity_model::ProviderError, recovery: &str) -> RpcError {
    RpcError::new(
        RpcErrorCode::ConfigurationPartiallySaved,
        error.to_string(),
        recovery,
    )
}

pub(super) fn model_discovery_error(error: singularity_model::ProviderError) -> RpcError {
    use singularity_model::ModelErrorCategory;
    match error.category() {
        ModelErrorCategory::ModelConfiguration | ModelErrorCategory::InvalidRequest => {
            configuration_error(error.to_string())
        }
        ModelErrorCategory::Authentication => RpcError::new(
            RpcErrorCode::ConfigurationInvalid,
            error.to_string(),
            "检查 API 地址和密钥；也可以手动添加模型。",
        ),
        ModelErrorCategory::Network
        | ModelErrorCategory::ProviderUnavailable
        | ModelErrorCategory::UnknownProviderError
        | ModelErrorCategory::JsonSchema => RpcError::new(
            RpcErrorCode::ProviderUnavailable,
            error.to_string(),
            "稍后重试；也可以手动添加模型。",
        ),
        ModelErrorCategory::Cancelled
        | ModelErrorCategory::ContextLengthExceeded
        | ModelErrorCategory::ContentFilter => internal_error(error.to_string()),
    }
}

pub(super) fn conversation_error(error: ConversationError) -> RpcError {
    match error {
        ConversationError::TurnAlreadyActive => session_busy(),
        ConversationError::Configuration(message) => configuration_error(message),
        ConversationError::Compaction(error) => internal_error(error.to_string()),
        ConversationError::Turn(error) => internal_error(error.to_string()),
        ConversationError::Session(error) => internal_error(error.to_string()),
    }
}

pub(super) fn catalog_error(error: CatalogError) -> RpcError {
    match error {
        CatalogError::NotFound(_) => RpcError::new(
            RpcErrorCode::SessionNotFound,
            "任务不存在或已归档。",
            "刷新项目的任务列表。",
        ),
        CatalogError::WriterActive => session_busy(),
        CatalogError::ScopeMismatch(_) => session_scope_conflict(),
        CatalogError::InvalidName => invalid_request(error.to_string()),
        CatalogError::AnchorNotFound(_) => invalid_request("历史分页位置已失效，请重新加载任务。"),
        other => internal_error(other.to_string()),
    }
}

pub(super) fn workspace_error(error: WorkspaceError) -> RpcError {
    match error {
        WorkspaceError::InvalidInput(message) => invalid_request(message),
        WorkspaceError::NotFound => RpcError::new(
            RpcErrorCode::WorkspaceNotFound,
            "项目不存在或已移除。",
            "刷新工作台并重新选择项目。",
        ),
        other => internal_error(other.to_string()),
    }
}

pub(super) fn session_busy() -> RpcError {
    RpcError::new(
        RpcErrorCode::SessionBusy,
        "当前任务正在处理另一项操作。",
        "等待状态变为空闲，或使用当前阶段提供的控制动作。",
    )
}

/// 测试互锁：取出并执行一次性的注入点。取走就没了，后续调用不再停下。
#[cfg(test)]
pub(super) fn take_pause(pause: &Mutex<Option<Arc<dyn Fn() + Send + Sync>>>) {
    let taken = pause
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(taken) = taken {
        taken();
    }
}

pub(super) fn control_error(error: ConversationControlError) -> RpcError {
    match error {
        ConversationControlError::NotRunning => session_busy(),
        ConversationControlError::InvalidInput => invalid_request("输入不能为空。"),
        ConversationControlError::ControlNotFound => RpcError::new(
            RpcErrorCode::ControlNotFound,
            "待处理输入已不存在或已经开始执行。",
            "刷新任务后确认待处理输入队列。",
        ),
    }
}
