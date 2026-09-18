//! bash 工具的执行、输出与规格模块。

mod capture;
mod exec;
#[cfg(test)]
mod faults;
pub(crate) mod job_object;
mod pump;
mod shell;
mod spec;

pub(crate) use exec::execute;
pub use shell::ensure_available;
pub(crate) use spec::{BashArgs, spec};
