//! 单进程本地 Web 工作台：Host 只做可信边界与协议适配，业务事实归 AppServer。

mod app_server;
mod directory_picker;
mod origin;
mod rpc;
mod server;
mod static_files;
mod workspace_files;

pub use server::run;
