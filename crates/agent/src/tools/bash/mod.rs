//! bash 工具的执行、输出与规格模块。

mod capture;
mod exec;
mod job_object;
mod pump;
mod shell;
mod spec;

pub(crate) use exec::DESCRIPTION;
pub use shell::ensure_available;
pub(crate) use spec::spec;

#[cfg(test)]
#[path = "../bash_exec_tests.rs"]
mod tests;
