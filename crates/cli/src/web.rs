//! 单进程本地 Web 工作台：Host 只做可信边界与协议适配，业务事实归 Workbench。

mod host;
mod origin;
mod rpc;
mod static_files;
mod workbench;
mod workspace_files;

pub use host::run;
