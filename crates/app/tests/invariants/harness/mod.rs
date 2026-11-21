//! 黑盒集成测试脚手架：临时目录隔离、`singularity.exe` JSON-RPC 客户端、
//! 本地 HTTP/1.1 SSE 回放与故障注入桩、通用协议不变量校验、测试夹具与 MCP stdio 脚本。

mod client;
mod fixtures;
mod mock_model;
mod protocol_checks;

pub use client::{BackendClient, DEFAULT_TIMEOUT, TestEnv};
pub use fixtures::{
    TINY_PNG_DATA_URL, messages_text_sse, messages_tools_sse, responses_sse, write_test_mcp_script,
};
pub use mock_model::{MockModelServer, ScriptedResponse};
