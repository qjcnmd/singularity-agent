#![deny(unsafe_code)]

//! 跨 crate 共享的 JSON-RPC 基础类型和 workspace 规则。

mod cancellation;
mod fs_owner;
mod project_instructions;
mod user_home;

pub use cancellation::CancellationToken;
pub use fs_owner::{create_owner_only_dir, ensure_owner_only_dir, ensure_owner_only_file};
pub use project_instructions::{
    PROJECT_INSTRUCTIONS_FILE_NAME, PROJECT_INSTRUCTIONS_MAX_FILE_BYTES,
    PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES, ProjectInstructionError, ProjectInstructionErrorCode,
    ProjectInstructions, find_workspace_root, load_project_instructions,
    load_project_instructions_from_cwd,
};
pub use user_home::user_singularity_home;

/// 创建仅属主可访问的新文件（在 Unix 系统上以 0600 权限创建）。
pub fn create_owner_only_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        let file = options.open(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        options.open(path)
    }
}

use serde::{Deserialize, Serialize};

/// JSON-RPC 请求结构无效。
pub const JSON_RPC_INVALID_REQUEST: i64 = -32600;
/// JSON-RPC 方法不存在。
pub const JSON_RPC_METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC 参数无效。
pub const JSON_RPC_INVALID_PARAMS: i64 = -32602;
/// JSON-RPC 内部错误。
pub const JSON_RPC_INTERNAL_ERROR: i64 = -32603;
/// AppServer 尚未初始化。
pub const APP_ERROR_NOT_INITIALIZED: i64 = -32002;
/// AppServer 已经初始化。
pub const APP_ERROR_ALREADY_INITIALIZED: i64 = -32003;
/// 请求的持久化对象不存在。
pub const APP_ERROR_NOT_FOUND: i64 = -32004;

/// 连接 AppServer 的客户端身份信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub title: String,
    pub version: String,
}

impl ClientInfo {
    /// 创建客户端身份信息。
    pub fn new(
        name: impl Into<String>,
        title: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            title: title.into(),
            version: version.into(),
        }
    }
}

/// JSON-RPC 错误码和脱敏错误消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorCode {
    pub code: i64,
    pub message: String,
}

impl ErrorCode {
    /// 创建 JSON-RPC 错误码。
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// 构造未初始化错误。
    pub fn not_initialized() -> Self {
        Self::new(APP_ERROR_NOT_INITIALIZED, "Not initialized")
    }

    /// 构造重复初始化错误。
    pub fn already_initialized() -> Self {
        Self::new(APP_ERROR_ALREADY_INITIALIZED, "Already initialized")
    }

    /// 构造无效请求错误。
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(JSON_RPC_INVALID_REQUEST, message)
    }

    /// 构造无效参数错误。
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(JSON_RPC_INVALID_PARAMS, message)
    }

    /// 构造资源不存在错误。
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(APP_ERROR_NOT_FOUND, message)
    }

    /// 返回错误消息。
    pub fn message(&self) -> &str {
        &self.message
    }
}
