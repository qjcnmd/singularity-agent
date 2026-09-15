//! 评估入口的会话准备与 Web 工作台运行环境。
//!
//! 两个入口都使用 SINGULARITY_HOME；评估入口为每次执行创建新会话。

use std::sync::{Arc, Mutex};

/// Hold one OS lock for the entire process; a leftover file is not an active lock.
pub fn lock_data_directory() -> Result<(std::path::PathBuf, std::fs::File), String> {
    let home = singularity_core::user_singularity_home_result()?
        .ok_or_else(|| "cannot resolve SINGULARITY_HOME".to_string())?;
    singularity_core::create_data_dir(&home)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("instance.lock"))
        .map_err(|e| e.to_string())?;
    file.try_lock().map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => format!(
            "数据目录已被另一个 Singularity 程序使用：{}",
            home.display()
        ),
        std::fs::TryLockError::Error(e) => {
            format!("cannot lock data directory {}: {e}", home.display())
        }
    })?;
    Ok((home, file))
}

use singularity_model::ModelConfigOwner;
use singularity_runtime::{
    Conversation, ThreadCatalog, TurnRunner, WorkspaceStore, prepare_session_dirs,
};

/// 一次无交互/交互执行的全部运行时句柄。
///
/// Tokio runtime 贯穿执行，为 provider HTTP 请求提供运行环境。
pub struct SessionSetup {
    pub conversation: Arc<Conversation>,
    _tokio_runtime: Arc<tokio::runtime::Runtime>,
}

/// 本地 Web 工作台进程级 owner；所有 Session 共享同一个 runner、目录和 runtime。
pub struct WebSetup {
    pub runtime: Arc<tokio::runtime::Runtime>,
    pub runner: Arc<TurnRunner>,
    pub catalog: ThreadCatalog,
    pub workspaces: WorkspaceStore,
    /// 磁盘模型配置的唯一入口；runner 与设置页面共用这一份实例。
    pub models: Arc<Mutex<ModelConfigOwner>>,
}

pub fn prepare_web(home: &std::path::Path) -> Result<WebSetup, String> {
    let RuntimeParts {
        runtime,
        models,
        runner,
        catalog,
    } = prepare_runtime(home)?;
    let workspaces = WorkspaceStore::open(home)?;
    Ok(WebSetup {
        runtime,
        runner,
        catalog,
        workspaces,
        models,
    })
}

pub fn prepare(home: &std::path::Path, model: Option<&str>) -> Result<SessionSetup, String> {
    // 模型配置 owner 只由 runner 持有，本入口不再单独使用它。
    let RuntimeParts {
        runtime,
        runner,
        catalog,
        ..
    } = prepare_runtime(home)?;
    let default_selector = runner.default_model_selector();

    let current = std::env::current_dir()
        .map_err(|error| format!("failed to read current directory: {error}"))?;
    let cwd = current
        .to_str()
        .ok_or_else(|| "thread cwd is not valid UTF-8".to_string())?;
    let thread = catalog
        .create_thread(cwd, model.map(str::to_string).or(default_selector))
        .map_err(|error| error.to_string())?;

    let conversation = Conversation::new(runner, thread);
    Ok(SessionSetup {
        conversation,
        _tokio_runtime: runtime,
    })
}

/// 两个入口共用的进程级装配：tokio runtime、模型配置 owner、runner 与其目录。
///
/// 差异部分留在各入口：Web 额外登记 workspace，无交互入口额外创建会话。
struct RuntimeParts {
    runtime: Arc<tokio::runtime::Runtime>,
    models: Arc<Mutex<ModelConfigOwner>>,
    runner: Arc<TurnRunner>,
    catalog: ThreadCatalog,
}

fn prepare_runtime(home: &std::path::Path) -> Result<RuntimeParts, String> {
    let runtime = Arc::new(tokio::runtime::Runtime::new().map_err(|error| error.to_string())?);
    prepare_session_dirs(home)?;
    let models = Arc::new(Mutex::new(ModelConfigOwner::open(
        home.to_path_buf(),
        runtime.handle().clone(),
    )));
    let runner = Arc::new(TurnRunner::new(
        home.join(singularity_runtime::SESSIONS_DIR_NAME),
        Arc::clone(&models),
    ));
    let catalog = ThreadCatalog::new(&runner);
    Ok(RuntimeParts {
        runtime,
        models,
        runner,
        catalog,
    })
}
