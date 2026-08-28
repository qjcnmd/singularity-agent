//! bash 工具的执行、输出与规格模块。

mod capture;
mod exec;
mod job_object;
mod pump;
mod shell;
mod spec;

pub(crate) use exec::{DESCRIPTION, execute};
pub use shell::ensure_available;
pub(crate) use spec::{BashArgs, spec};
