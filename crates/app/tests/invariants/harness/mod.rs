mod client;
mod fixtures;
mod mock_model;
mod protocol_checks;

pub use client::{BackendClient, DEFAULT_TIMEOUT, TestEnv};
pub use fixtures::{
    TINY_PNG_DATA_URL, messages_text_sse, messages_tools_sse, responses_sse, write_test_mcp_script,
};
pub use mock_model::{MockModelServer, ScriptedResponse};
