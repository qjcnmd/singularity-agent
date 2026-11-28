mod client;
mod fixtures;
mod mock_model;
mod protocol_checks;

pub use client::{BackendClient, DEFAULT_TIMEOUT, TestEnv, provider_config_json};
pub use fixtures::{
    TINY_PNG_DATA_URL, find_tool_result, messages_text_sse, messages_tools_sse, responses_sse,
    write_test_mcp_script,
};
pub use mock_model::{MockModelServer, ScriptedResponse};
