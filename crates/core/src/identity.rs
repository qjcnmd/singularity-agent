//! 产品名称、环境变量与持久化目录等对外标识常量（内部模块不再散落硬编码）。

pub const APP_NAME: &str = "Singularity";
pub const APP_SLUG: &str = "singularity";
pub const HOME_ENV_VAR: &str = "SINGULARITY_HOME";
pub const LOG_ENV_VAR: &str = "SINGULARITY_LOG";
pub const DATA_DIR_NAME: &str = ".singularity";
pub const LOG_FILE_NAME: &str = "singularity.log";
pub const USER_AGENT: &str = concat!("singularity-agent/", env!("CARGO_PKG_VERSION"));
