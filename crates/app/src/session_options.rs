//! 评估入口的会话准备，以及桌面工作台的运行环境。
//!
//! 两个入口都用 SINGULARITY_HOME；评估入口每次执行都新建一个会话。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use singularity_model::ModelConfigManager;
use singularity_runtime::{Conversation, SESSIONS_DIR_NAME, ThreadCatalog, TurnRunner};

use crate::desktop::workspace_store::WorkspaceStore;

/// 在整个进程生命周期内持有一把 OS 锁；锁文件还留在磁盘上，不代表有程序正持有它。
pub fn lock_data_directory() -> Result<(PathBuf, std::fs::File), String> {
    let home = singularity_core::resolve_home()?.path;
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

/// 一次执行（无交互或桌面）用到的全部运行时句柄。
///
/// Tokio runtime 全程存活，provider 的 HTTP 请求靠它运行。
pub struct SessionSetup {
    pub conversation: Arc<Conversation>,
    pub runtime: Arc<tokio::runtime::Runtime>,
    pub mcp: Arc<singularity_mcp::McpManager>,
}

/// 本地桌面工作台的进程级持有者；所有 Session 共用同一个 runner、目录和 runtime。
pub struct DesktopSetup {
    pub runtime: Arc<tokio::runtime::Runtime>,
    pub runner: Arc<TurnRunner>,
    pub catalog: ThreadCatalog,
    pub workspaces: WorkspaceStore,
    /// 磁盘模型配置的唯一入口；runner 和设置页面共用这一个实例。
    pub models: Arc<ModelConfigManager>,
    pub mcp: Arc<singularity_mcp::McpManager>,
    /// 应用主目录：技能发现这类宿主查询和执行链读的是同一个事实。
    pub home: PathBuf,
}

pub fn prepare_desktop(home: &Path) -> Result<DesktopSetup, String> {
    let RuntimeParts {
        runtime,
        models,
        runner,
        catalog,
        mcp,
    } = prepare_runtime(home, true)?;
    let workspaces = WorkspaceStore::open(home)?;
    Ok(DesktopSetup {
        runtime,
        runner,
        catalog,
        workspaces,
        models,
        mcp,
        home: home.to_path_buf(),
    })
}

pub fn prepare(home: &Path, model: Option<&str>) -> Result<SessionSetup, String> {
    let RuntimeParts {
        runtime,
        models,
        runner,
        catalog,
        mcp,
    } = prepare_runtime(home, false)?;
    let default_selector = models.snapshot().resolved_default_selector();

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
        runtime,
        mcp,
    })
}

/// 两个入口共用的进程级装配：tokio runtime、模型配置持有者、runner 及其目录。
///
/// 各入口的差异留给自己：桌面 额外登记 workspace，无交互入口额外创建会话。
struct RuntimeParts {
    runtime: Arc<tokio::runtime::Runtime>,
    models: Arc<ModelConfigManager>,
    runner: Arc<TurnRunner>,
    catalog: ThreadCatalog,
    mcp: Arc<singularity_mcp::McpManager>,
}

/// Runner 和 ThreadCatalog 共用会话目录，装配层不把执行器当成目录的依赖容器。
fn prepare_runtime(home: &Path, user_questions: bool) -> Result<RuntimeParts, String> {
    let runtime = Arc::new(tokio::runtime::Runtime::new().map_err(|error| error.to_string())?);
    let sessions_dir = home.join(SESSIONS_DIR_NAME);
    singularity_core::create_data_dir(&sessions_dir)?;
    let models = Arc::new(ModelConfigManager::open(home.to_path_buf()));
    let mcp = Arc::new(singularity_mcp::McpManager::open(home.to_path_buf()));
    let runner = TurnRunner::new(sessions_dir.clone(), Arc::clone(&models), Arc::clone(&mcp));
    let runner = Arc::new(if user_questions {
        runner.with_user_questions()
    } else {
        runner
    });
    let catalog = ThreadCatalog::new(sessions_dir);
    Ok(RuntimeParts {
        runtime,
        models,
        runner,
        catalog,
        mcp,
    })
}
