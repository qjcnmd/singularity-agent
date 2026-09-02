//! 无交互入口的会话准备：默认持久化、`--session` 恢复、`--no-session` 临时运行。
//!
//! 三种形态共用同一 runtime 构造路径；区别只在 home 的归属与 Thread 的来源。

use std::sync::Arc;

use singularity_core::user_singularity_home;
use singularity_model::ProviderConfigSnapshot;
use singularity_runtime::{
    Conversation, ResumeError, ThreadCatalog, TurnRunner, prepare_session_dirs,
};

/// 一次无交互/交互执行的全部运行时句柄。
///
/// `_temporary_home` 与 `_tokio_runtime` 贯穿整个进程生命周期：前者承载
/// `--no-session` 的临时会话目录，后者是 provider HTTP 泵依赖的 Handle 背景。
pub struct SessionSetup {
    pub conversation: Arc<Conversation>,
    pub thread_id: String,
    _temporary_home: Option<tempfile::TempDir>,
    _tokio_runtime: Arc<tokio::runtime::Runtime>,
}

/// 会话准备错误；文本可直接写入 stderr。
pub struct SetupError {
    pub message: String,
}

pub fn prepare(
    model: Option<&str>,
    session: Option<&str>,
    no_session: bool,
) -> Result<SessionSetup, SetupError> {
    prepare_inner(model, session, no_session).map_err(|message| SetupError { message })
}

fn prepare_inner(
    model: Option<&str>,
    session: Option<&str>,
    no_session: bool,
) -> Result<SessionSetup, String> {
    let (home, temporary_home) = if no_session {
        let temp =
            tempfile::TempDir::new().map_err(|error| format!("temporary session home: {error}"))?;
        (temp.path().to_path_buf(), Some(temp))
    } else {
        let home =
            user_singularity_home().ok_or_else(|| "cannot resolve SINGULARITY_HOME".to_string())?;
        (home, None)
    };
    let tokio_runtime =
        Arc::new(tokio::runtime::Runtime::new().map_err(|error| error.to_string())?);
    prepare_session_dirs(&home)?;
    let sessions_dir = home.join(singularity_runtime::SESSIONS_DIR_NAME);
    let snapshot = ProviderConfigSnapshot::capture(tokio_runtime.handle().clone());
    let runner = Arc::new(TurnRunner::new(sessions_dir, snapshot));
    let catalog = ThreadCatalog::new(&runner);
    let default_selector = runner.default_model_selector();

    let (thread, model_override) =
        if let Some(session_id) = session.map(str::trim).filter(|id| !id.is_empty()) {
            let thread = catalog
                .resume_thread(session_id)
                .map_err(|error| match error {
                    ResumeError::NotFound(_) => format!("thread {session_id} was not found"),
                    error => format!("failed to resume thread {session_id}: {error}"),
                })?;
            // Existing Thread settings remain durable facts; --model is resolved
            // only into the current execution's model snapshot.
            (thread, model.map(str::to_string))
        } else {
            let current = std::env::current_dir()
                .map_err(|error| format!("failed to read current directory: {error}"))?;
            let cwd = current
                .to_str()
                .ok_or_else(|| "thread cwd is not valid UTF-8".to_string())?;
            (
                catalog.create_thread(cwd, model.map(str::to_string).or(default_selector))?,
                None,
            )
        };

    let thread_id = thread.thread_id.clone();
    let conversation =
        Conversation::new_with_model_override(Arc::clone(&runner), thread, model_override);
    Ok(SessionSetup {
        conversation,
        thread_id,
        _temporary_home: temporary_home,
        _tokio_runtime: tokio_runtime,
    })
}
