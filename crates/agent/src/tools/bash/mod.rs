//! bash 工具的执行、输出捕获与规格定义。

mod exec;
mod pump;
mod result;
mod shell;
mod spec;
mod tasks;

pub(crate) use spec::{
    BashArgs, TaskIdArgs, TaskListArgs, TaskOutputArgs, spec, task_list_spec, task_output_spec,
    task_stop_spec,
};
pub use tasks::{BashTaskUpdate, BashTasks};
