//! bash 工具的执行、输出捕获与规格定义。

mod capture;
mod exec;
pub(crate) mod job_object;
mod pump;
mod shell;
mod spec;

pub(crate) use exec::execute;
pub use shell::ensure_available;
pub(crate) use spec::{BashArgs, spec};
