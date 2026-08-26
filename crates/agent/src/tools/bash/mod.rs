//! bash 工具的执行、输出与规格模块。

mod output;
mod process;
mod spec;

use spec::BashArgs;
pub(crate) use spec::spec;

include!("../bash_exec.rs");
