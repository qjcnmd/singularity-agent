//! model crate 单元测试共用的本地 HTTP 请求读取夹具。

#![allow(clippy::expect_used)]

use std::io::{BufRead, BufReader, Read};
use std::net::TcpStream;
use std::time::Duration;

/// 测试服务器捕获到的一次 HTTP/1.x 请求。
pub(crate) struct CapturedHttpRequest {
    pub(crate) target: String,
    pub(crate) body: Vec<u8>,
    headers: Vec<(String, String)>,
}

impl CapturedHttpRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// 从本地测试连接读取请求行、请求头及 Content-Length 声明的正文。
pub(crate) fn read_http_request(stream: &mut TcpStream) -> CapturedHttpRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set test request read timeout");
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    assert_ne!(
        reader
            .read_line(&mut request_line)
            .expect("read HTTP request line"),
        0,
        "client closed before sending an HTTP request"
    );
    let target = request_line
        .split_whitespace()
        .nth(1)
        .expect("HTTP request line has a target")
        .to_string();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        assert_ne!(
            reader.read_line(&mut line).expect("read HTTP request head"),
            0,
            "client closed before completing the HTTP request head"
        );
        if line == "\r\n" {
            break;
        }
        let (name, value) = line
            .trim_end()
            .split_once(':')
            .expect("HTTP request header contains a colon");
        headers.push((name.to_string(), value.trim().to_string()));
    }
    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| {
            value
                .parse::<usize>()
                .expect("HTTP Content-Length is a number")
        })
        .unwrap_or(0);
    let mut body = vec![0; content_length];
    reader
        .read_exact(&mut body)
        .expect("read complete HTTP request body");
    CapturedHttpRequest {
        target,
        body,
        headers,
    }
}

use crate::config::selection::SelectedModel;
use crate::openai::wire::{DEFAULT_CHAT_OUTPUT_TOKENS_FIELD, ThinkingWireFormat};
use crate::provider::contract::ProviderApiProtocol;

/// 本地协议测试的默认模型能力。
pub(crate) fn test_selection(protocol: ProviderApiProtocol) -> SelectedModel {
    SelectedModel {
        model_name: "model".into(),
        api_protocol: protocol,
        max_context_tokens: 32_000,
        max_output_tokens: 4096,
        reasoning_variant: None,
        reasoning_enabled: false,
        wire_reasoning_effort: None,
        thinking_wire_format: ThinkingWireFormat::ReasoningEffort,
        chat_output_tokens_field: DEFAULT_CHAT_OUTPUT_TOKENS_FIELD.to_string(),
        supports_developer_role: false,
        supports_tool_choice: true,
        requires_reasoning_content_for_tool_calls: false,
        requires_assistant_content_for_tool_calls: false,
    }
}

/// 调用方显式提供本地地址，夹具不接触真实 provider。
pub(crate) fn test_config(
    base_url: impl Into<String>,
) -> crate::config::selection::OpenAiProviderConfig {
    crate::config::selection::OpenAiProviderConfig {
        provider_name: "fixture".into(),
        base_url: base_url.into(),
        api_key: "test".into(),
    }
}
