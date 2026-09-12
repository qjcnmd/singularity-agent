//! 评估入口的会话准备与 Web 工作台运行环境。
//!
//! 两个入口都使用 SINGULARITY_HOME；评估入口为每次执行创建新会话。

use std::sync::Arc;

/// Hold one OS lock for the entire process; a leftover file is not an active lock.
pub fn lock_data_directory() -> Result<std::fs::File, String> {
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
    Ok(file)
}

use singularity_core::user_singularity_home;
use singularity_model::{ModelConfigOwner, ProviderConfigSnapshot};
use singularity_runtime::{
    Conversation, ThreadCatalog, TurnRunner, WorkspaceStore, prepare_session_dirs,
};

/// 一次无交互/交互执行的全部运行时句柄。
///
/// Tokio runtime 贯穿执行，为 provider HTTP 请求提供运行环境。
pub struct SessionSetup {
    pub conversation: Arc<Conversation>,
    pub thread_id: String,
    _tokio_runtime: Arc<tokio::runtime::Runtime>,
}

/// 本地 Web 工作台进程级 owner；所有 Session 共享同一个 runner、目录和 runtime。
pub struct WebSetup {
    pub runtime: Arc<tokio::runtime::Runtime>,
    pub runner: Arc<TurnRunner>,
    pub catalog: ThreadCatalog,
    pub workspaces: WorkspaceStore,
    pub models: ModelConfigOwner,
}

pub fn prepare_web() -> Result<WebSetup, String> {
    let home =
        user_singularity_home().ok_or_else(|| "cannot resolve SINGULARITY_HOME".to_string())?;
    let runtime = Arc::new(tokio::runtime::Runtime::new().map_err(|error| error.to_string())?);
    prepare_session_dirs(&home)?;
    let models =
        ModelConfigOwner::open(runtime.handle().clone()).map_err(|error| error.to_string())?;
    let runner = Arc::new(TurnRunner::new(
        home.join(singularity_runtime::SESSIONS_DIR_NAME),
        models.snapshot(),
    ));
    let catalog = ThreadCatalog::new(&runner);
    let workspaces = WorkspaceStore::open(&home)?;
    Ok(WebSetup {
        runtime,
        runner,
        catalog,
        workspaces,
        models,
    })
}

pub fn prepare(model: Option<&str>) -> Result<SessionSetup, String> {
    let home =
        user_singularity_home().ok_or_else(|| "cannot resolve SINGULARITY_HOME".to_string())?;
    let tokio_runtime =
        Arc::new(tokio::runtime::Runtime::new().map_err(|error| error.to_string())?);
    prepare_session_dirs(&home)?;
    let sessions_dir = home.join(singularity_runtime::SESSIONS_DIR_NAME);
    let snapshot = ProviderConfigSnapshot::capture(tokio_runtime.handle().clone());
    let runner = Arc::new(TurnRunner::new(sessions_dir, snapshot));
    let catalog = ThreadCatalog::new(&runner);
    let default_selector = runner.default_model_selector();

    let current = std::env::current_dir()
        .map_err(|error| format!("failed to read current directory: {error}"))?;
    let cwd = current
        .to_str()
        .ok_or_else(|| "thread cwd is not valid UTF-8".to_string())?;
    let thread = catalog
        .create_thread(cwd, model.map(str::to_string).or(default_selector))
        .map_err(|error| error.to_string())?;

    let thread_id = thread.thread_id.clone();
    let conversation = Conversation::new(runner, thread);
    Ok(SessionSetup {
        conversation,
        thread_id,
        _tokio_runtime: tokio_runtime,
    })
}
