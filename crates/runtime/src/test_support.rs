//! Runtime 包内测试共享的确定性夹具与门控钩子。
//!
//! 提供隔离的临时 sessions 目录、provider 配置快照、请求输入投影、会话构造
//! conversation_with，以及门控替身 GatedProvider。夹具使用隔离 home，注入的替身不触网。
#![allow(clippy::unwrap_used, clippy::expect_used)] // 夹具构造失败即测试环境损坏，直接 panic

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 同步测试入口驱动生产异步执行链；测试本身不增加第二条执行实现。
pub fn run_async<F: std::future::Future>(future: F) -> F::Output {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| tokio::runtime::Runtime::new().expect("test runtime"))
        .block_on(future)
}

use crate::Conversation;
use crate::ThreadCatalog;
use crate::runner::TurnRunner;
use singularity_model::{
    ModelConfigurationSnapshot, ModelErrorKind, ModelTurnRequest, Provider, ProviderError,
};

/// 测试工作目录：线程注册的 cwd 用当前进程目录即可，各测试共用一处。
pub fn cwd() -> String {
    std::env::current_dir().expect("current dir").to_str().expect("utf-8 cwd").to_string()
}

/// 测试装配：夹具自己持有隔离 home（含 sessions 目录与模型配置）。
pub struct SessionsFixture {
    home: tempfile::TempDir,
    pub dir: PathBuf,
}

impl Default for SessionsFixture {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionsFixture {
    pub fn new() -> Self {
        let home = tempfile::TempDir::new().expect("temp home");
        std::fs::create_dir_all(home.path().join(crate::SESSIONS_DIR_NAME)).expect("sessions dir");
        write_provider_fixture(home.path(), "base-model-2");
        Self {
            dir: home.path().join(crate::SESSIONS_DIR_NAME),
            home,
        }
    }

    /// 夹具持有的隔离 home：需要按路径写配置或会话文件时使用。
    pub fn home(&self) -> &Path {
        self.home.path()
    }

    /// provider 省略时按夹具 home 的配置解析。
    pub fn runner(&self, provider: Option<Arc<dyn Provider + Send + Sync>>) -> Arc<TurnRunner> {
        let runner = TurnRunner::new(
            self.dir.clone(),
            Arc::new(singularity_model::ModelConfigManager::open(self.home().to_path_buf())),
            Arc::new(singularity_mcp::McpManager::open(self.home().to_path_buf())),
        );
        Arc::new(match provider {
            Some(provider) => runner.with_provider_override(provider),
            None => runner,
        })
    }

    /// 夹具的会话目录。
    pub fn catalog(&self) -> ThreadCatalog {
        ThreadCatalog::new(self.dir.clone())
    }
}

/// 每次生成请求中最后一条人工输入；项目指令位于对话历史之前。
/// （更早的输入会作为历史上下文重放，无法用来区分身份。）
pub fn input_sequence(requests: &[ModelTurnRequest]) -> Vec<String> {
    requests
        .iter()
        .map(|request| {
            request
                .messages
                .iter()
                .rev()
                .find(|message| message.role == singularity_model::ModelRole::User)
                .map(|message| message.content.clone())
                .unwrap_or_default()
        })
        .collect()
}

/// 把共享的 provider fixture 写入已有的隔离测试 home。
pub fn write_provider_fixture(home: &Path, alternate_model: &str) {
    let models = ["base-model", alternate_model]
        .into_iter()
        .map(|id| {
            (
                id.to_string(),
                serde_json::json!({
                    "api_protocol": "chat",
                    "max_context_tokens": 128_000,
                    "max_output_tokens": 4_096
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let config = serde_json::json!({
        "default_model": "openai_compatible/base-model",
        "providers": {"openai_compatible": {
            "base_url": "http://127.0.0.1:9/v1",
            "models": models
        }}
    });
    let auth = serde_json::json!({
        "providers": {"openai_compatible": {"api_key": "test-key-placeholder"}}
    });
    for (name, value) in [("config.json", config), ("auth.json", auth)] {
        singularity_core::atomic_replace_bytes(&home.join(name), value.to_string().as_bytes())
            .expect("write private provider fixture");
    }
}

/// 在给定夹具上注入 fake provider 构造会话协调器，返回会话和 thread 的规范 session
/// 文件路径；model 是 thread 初始 selector（None 走目录默认）。夹具由调用方持有，
/// 保证会话目录在执行期间保留。
pub fn conversation_with(
    fixture: &SessionsFixture,
    provider: Arc<dyn Provider + Send + Sync>,
    model: Option<&str>,
) -> (Arc<Conversation>, PathBuf) {
    let runner = fixture.runner(Some(provider));
    let thread = fixture
        .catalog()
        .create_thread(std::env::current_dir().unwrap().to_str().unwrap(), model.map(str::to_string))
        .expect("create thread");
    let path = fixture.dir.join(singularity_agent::session::session_file_name(&thread.thread_id));
    (Conversation::new(runner, thread), path)
}

/// 模型边界门控替身。首个请求到达时发出 started 信号并阻塞，直到测试释放或关闭通道；
/// 经门控时已取消的请求按采样取消语义返回 Cancelled，其余请求委托给 inner 替身。断言能
/// 锚定在「turn 已在执行、operation 起始记录已 durable、执行窗口仍被占用」的时刻。
pub struct GatedProvider {
    started: std::sync::mpsc::Sender<()>,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    inner: Arc<dyn Provider + Send + Sync>,
}

impl GatedProvider {
    /// 包装 inner 新建门控替身，返回替身与「首个请求已到达」的接收端。
    pub fn new(inner: Arc<dyn Provider + Send + Sync>) -> (Arc<Self>, std::sync::mpsc::Receiver<()>) {
        let (sender, receiver) = std::sync::mpsc::channel();
        (
            Arc::new(Self {
                started: sender,
                release: std::sync::Mutex::new(None),
                inner,
            }),
            receiver,
        )
    }

    /// 注入一个释放通道：测试通过它放行被阻塞的请求（可选）。
    pub fn with_release(&self, release: std::sync::mpsc::Receiver<()>) {
        *self.release.lock().expect("gate lock") = Some(release);
    }
}

impl Provider for GatedProvider {
    fn model_configuration(&self) -> ModelConfigurationSnapshot {
        self.inner.model_configuration()
    }

    fn complete_stream<'a>(
        &'a self,
        request: &'a ModelTurnRequest,
        cancellation: &'a tokio_util::sync::CancellationToken,
        observer: &'a mut dyn singularity_model::ProviderObserver,
    ) -> singularity_model::ProviderFuture<'a> {
        Box::pin(async move {
            let _ = self.started.send(());
            let release = self.release.lock().expect("gate lock").take();
            if let Some(release) = release {
                let _ = tokio::task::spawn_blocking(move || release.recv()).await;
            }
            if cancellation.is_cancelled() {
                return Err(ProviderError::new(ModelErrorKind::Cancelled, "cancelled at stop gate").into());
            }
            self.inner.complete_stream(request, cancellation, observer).await
        })
    }
}

/// 为手动压缩准备非空历史前缀；摘要校验失败的用例需要可被替换的内容。
pub fn seed_compaction_history(fixture: &SessionsFixture, thread_id: &str) {
    use singularity_agent::message::{AgentMessage, ContentBlock};
    use singularity_agent::session::SessionManager;

    let path = fixture.dir.join(singularity_agent::session::session_file_name(thread_id));
    let mut session = SessionManager::open_existing(&path, thread_id).expect("open session");
    for (user, text) in [
        (true, "first user ".repeat(5_000)),
        (false, "first assistant ".repeat(5_000)),
        (true, "recent user ".repeat(5_000)),
        (false, "recent assistant ".repeat(5_000)),
    ] {
        let content = vec![ContentBlock::Text { text }];
        let message = if user {
            AgentMessage::User { content, display_text: None }
        } else {
            AgentMessage::Assistant { content, provider_reasoning_replay: None }
        };
        session.append_message(message).expect("append history");
    }
}
