//! 与协议无关的传输部分：HTTP 客户端、可取消的网络等待和 SSE 分帧。
//! Chat / Responses 的编码和解码在 openai 模块，本模块不依赖它们。

pub(crate) mod http;
pub(crate) mod stream;

pub(crate) use http::{provider_client, provider_error_from_http_status, provider_future, retry_after_delay};
