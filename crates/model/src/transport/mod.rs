//! provider 传输层：HTTP 客户端、可取消的网络等待与 SSE 帧切分。各协议的
//! 请求编码、SSE 解码和响应终结属于 openai 包的协议模块；本模块不引用具体 Provider，
//! 只提供它们共用的传输能力。

pub(crate) mod http;
pub(crate) mod retry;
pub(crate) mod stream;

pub(crate) use http::{
    provider_cancelled_error, provider_client, provider_error_from_http_status, provider_future,
};
pub(crate) use retry::*;
