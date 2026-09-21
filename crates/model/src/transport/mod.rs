//! provider transport：HTTP client、可取消网络等待、有界读取与 SSE 帧切分。
//!
//! 具体协议的请求编码、SSE 解码与响应终结属于 openai 包各自的协议模块；
//! 本模块不引用具体 Provider，只提供它们共用的传输能力。

pub(crate) mod http;
pub(crate) mod retry;
pub(crate) mod stream;

pub(crate) use http::{
    block_on_provider_future, provider_cancelled_error, provider_client,
    provider_error_from_http_status, provider_reasoning_history_error,
    read_bounded_provider_response_body,
};
pub(crate) use retry::*;
