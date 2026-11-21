//! `mcp` 的结构化错误类型，保留 IO 错误链并区分取消、超时与配置错误。

/// MCP 配置、连接、发现与调用失败时返回的结构化错误。
#[must_use]
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("{0}")]
    Cancelled(String),
    #[error("{0}")]
    Timeout(String),
    #[error("MCP 服务器不存在。")]
    NotFound,
    #[error("{0}")]
    InvalidConfig(String),
    #[error("{0}")]
    Transport(String),
    #[error("{context}：{source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl McpError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled(_))
    }

    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout(_))
    }

    pub(crate) fn map_message(self, transform: impl FnOnce(String) -> String) -> Self {
        match self {
            Self::Cancelled(message) => Self::Cancelled(transform(message)),
            Self::Timeout(message) => Self::Timeout(transform(message)),
            Self::InvalidConfig(message) => Self::InvalidConfig(transform(message)),
            Self::Transport(message) => Self::Transport(transform(message)),
            other @ (Self::NotFound | Self::Io { .. }) => other,
        }
    }
}

const _: () = {
    const fn assert_error<T: std::error::Error + Send + Sync + 'static>() {}
    assert_error::<McpError>();
};
