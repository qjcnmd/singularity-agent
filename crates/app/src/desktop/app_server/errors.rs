use super::*;

pub(super) fn turn_terminal(
    result: Result<singularity_runtime::TurnOutcome, singularity_runtime::TurnRunError>,
) -> SessionTerminalSnapshot {
    match result {
        Ok(outcome) => SessionTerminalSnapshot {
            source: SessionTerminalSource::Turn,
            status: outcome.turn_status,
            manually_stopped: outcome.manually_stopped,
            // 已保存的失败由会话历史展示，运行状态只补充未保存的原因。
            error: None,
        },
        Err(error) => SessionTerminalSnapshot {
            source: SessionTerminalSource::Turn,
            status: TurnStatus::Failed,
            manually_stopped: false,
            error: Some(error.detail()),
        },
    }
}

pub(crate) fn invalid_request(message: impl Into<String>) -> RpcError {
    RpcError::new(RpcErrorCode::InvalidRequest, message, "")
}

pub(super) fn internal_error(message: impl Into<String>) -> RpcError {
    RpcError::new(RpcErrorCode::Internal, message, "")
}

pub(super) fn configuration_error(message: impl Into<String>) -> RpcError {
    RpcError::new(RpcErrorCode::ConfigurationInvalid, message, "打开模型设置并修正配置。")
}

pub(super) fn model_error(error: singularity_model::ProviderError) -> RpcError {
    configuration_error(error.to_string())
}

pub(super) fn model_discovery_error(error: singularity_model::ProviderError) -> RpcError {
    use singularity_model::ModelErrorKind;
    match error.kind {
        ModelErrorKind::InvalidRequest => configuration_error(error.to_string()),
        ModelErrorKind::AuthError => RpcError::new(
            RpcErrorCode::ConfigurationInvalid,
            error.to_string(),
            "检查 API 地址和密钥；也可以手动添加模型。",
        ),
        ModelErrorKind::NetworkError
        | ModelErrorKind::Timeout
        | ModelErrorKind::RateLimited
        | ModelErrorKind::ProviderOverloaded
        | ModelErrorKind::UnknownProviderError
        | ModelErrorKind::JsonSchemaViolation => RpcError::new(
            RpcErrorCode::ProviderUnavailable,
            error.to_string(),
            "稍后重试；也可以手动添加模型。",
        ),
        ModelErrorKind::Cancelled | ModelErrorKind::ContextLengthExceeded | ModelErrorKind::ContentFilter => {
            internal_error(error.to_string())
        }
    }
}

pub(super) fn conversation_error(error: ConversationError) -> RpcError {
    match error {
        ConversationError::Busy => session_busy(),
        ConversationError::InvalidName => invalid_request(error.to_string()),
        ConversationError::Configuration(message) => configuration_error(message),
        ConversationError::Session(error) => internal_error(error.to_string()),
    }
}

pub(super) fn catalog_error(error: CatalogError) -> RpcError {
    match error {
        CatalogError::NotFound(_) => {
            RpcError::new(RpcErrorCode::SessionNotFound, "任务不存在或已归档。", "刷新项目的任务列表。")
        }
        CatalogError::InvalidModel(_) => invalid_request(error.to_string()),
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
