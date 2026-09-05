//! 单进程本地 Web 工作台：Host 只做可信边界与协议适配，业务事实归 Workbench。

mod auth;
mod host;
mod rpc;
mod static_files;
mod workbench;
mod workspace_files;

use crate::session_options::WebSetup;

pub async fn run(setup: WebSetup, port: u16, no_open: bool) -> Result<(), String> {
    host::run(setup, port, no_open).await
}
