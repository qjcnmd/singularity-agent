//! `AppServer` 的传输层：stdio JSON-Lines 控制面。
//!
//! 输入由单一 reader owner 分类；普通请求进入有界单 owner 队列，turn controls
//! 使用共享活动句柄的窄 control lane；所有输出通过唯一 writer 顺序写出。

pub(crate) mod error;
pub(crate) mod framing;
pub(crate) mod output;
pub(crate) mod supervisor;

pub(crate) use error::{internal_error_value, request_error_value, transport_error_value};
pub(crate) use framing::read_bounded_line;
pub(crate) use output::{
    send_app_server_outputs, send_output, send_output_async, write_output_queue,
};
pub(crate) use supervisor::run;

use crate::AppServerCancellationHandle;
use std::time::Duration;

pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
pub(crate) const OUTPUT_QUEUE_CAPACITY: usize = 256;
/// 单条 JSON-Lines frame（含 JSON-RPC 请求/响应）的字节上限。
pub(crate) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub(crate) trait ExecutionStop: Send + Sync {
    fn request_execution_stop(&self);
}

impl ExecutionStop for AppServerCancellationHandle {
    fn request_execution_stop(&self) {
        let _ = AppServerCancellationHandle::request_execution_stop(self);
    }
}

#[cfg(test)]
#[path = "../tests/transport.rs"]
mod tests;
