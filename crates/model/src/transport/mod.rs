//! 与协议无关的传输部分：HTTP 客户端、可取消的网络等待和 SSE 分帧。
//! Messages / Responses 的编码和解码在各自模块，本模块不依赖它们。

pub(crate) mod http;
pub(crate) mod stream;

pub(crate) use http::{provider_client, provider_future, retry_after_delay};
