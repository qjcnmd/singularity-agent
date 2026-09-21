//! Runtime 及下游入口测试共享的确定性夹具与门控钩子。
//!
//! 提供隔离的临时 sessions 目录、进程级写者协调器、provider 配置快照、
//! 请求输入投影、注入了 provider 的会话构造 conversation_with，以及门控
//! 替身 GatedProvider：首个请求到达时发出信号并阻塞，让测试在 turn 仍在
//! 执行、写者锁仍被占用时观测 durable 事实，并按采样取消语义响应取消令牌。
//!
//! 全部夹具隔离于真实 SINGULARITY_HOME，provider 经内存替身注入，绝不触网。
#![allow(clippy::unwrap_used, clippy::expect_used)] // 夹具构造失败即测试环境损坏，直接 panic

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::Conversation;
use crate::ThreadCatalog;
use crate::runner::TurnRunner;
use singularity_agent::session::WriterLockCoordinator;
use singularity_model::{
    ModelConfigurationSnapshot, ModelErrorKind, ModelTurnRequest, ModelTurnResponse, Provider,
    ProviderError,
};

/// 每个测试独立的临时 sessions 目录。
pub fn temp_sessions() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().expect("temp home");
    std::fs::create_dir_all(dir.path().join("sessions")).expect("sessions dir");
    dir
}

/// 进程级写者锁协调器（每测试独立目录各持一个即可）。
pub fn coordinator() -> Arc<WriterLockCoordinator> {
    Arc::new(WriterLockCoordinator::default())
}

/// 测试工作目录：线程注册的 cwd 用当前进程目录即可，各测试共用一处。
pub fn cwd() -> String {
    std::env::current_dir()
        .expect("current dir")
        .to_str()
        .expect("utf-8 cwd")
        .to_string()
}

/// 测试装配：夹具自己持有隔离 home（含 sessions 目录）与共享写者协调器。
///
/// 生产入口同样先显式创建这两项，再分别交给 TurnRunner 与 ThreadCatalog；
/// 夹具让测试持有同一对依赖，runner 与目录都不再充当对方的依赖容器。
pub struct SessionsFixture {
    home: tempfile::TempDir,
    pub dir: PathBuf,
    pub coordinator: Arc<WriterLockCoordinator>,
}

impl Default for SessionsFixture {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionsFixture {
    pub fn new() -> Self {
        let home = temp_sessions();
        Self {
            dir: home.path().join(crate::SESSIONS_DIR_NAME),
            coordinator: coordinator(),
            home,
        }
    }

    /// 夹具持有的隔离 home：需要按路径写配置或会话文件时使用。
    pub fn home(&self) -> &Path {
        self.home.path()
    }

    /// 与目录共享写者协调器的执行器；provider 省略时按配置快照解析。
    /// 配置夹具目录与一次性 runtime 都随进程存活：替身注入场景下 provider
    /// 不触网，句柄只需存在。
    pub fn runner(&self, provider: Option<Arc<dyn Provider + Send + Sync>>) -> Arc<TurnRunner> {
        static CONFIG_HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        static RUNTIME_HANDLE: std::sync::OnceLock<tokio::runtime::Handle> =
            std::sync::OnceLock::new();
        let config_home = CONFIG_HOME.get_or_init(|| {
            let directory = tempfile::tempdir().expect("snapshot fixture home");
            let path = directory.path().to_path_buf();
            write_provider_fixture(&path, "base-model-2");
            // 目录随进程存活：owner 按目录读取两文件。
            std::mem::forget(directory);
            path
        });
        let handle = RUNTIME_HANDLE.get_or_init(|| {
            let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
            let handle = runtime.handle().clone();
            std::mem::forget(runtime);
            handle
        });
        let runner = TurnRunner::new(
            self.dir.clone(),
            Arc::new(std::sync::Mutex::new(
                singularity_model::ModelConfigManager::open(config_home.clone()),
            )),
            Arc::clone(&self.coordinator),
            handle.clone(),
        );
        Arc::new(match provider {
            Some(provider) => runner.with_provider_override(provider),
            None => runner,
        })
    }

    /// 与执行器共享写者协调器的会话目录。
    pub fn catalog(&self) -> ThreadCatalog {
        ThreadCatalog::new(self.dir.clone(), Arc::clone(&self.coordinator))
    }
}

/// 每次请求中最后一条人工输入；文件指令上下文不参与输入顺序断言。
/// （更早的输入会作为历史上下文重放，不能用于唯一性判断。）
pub fn input_sequence(requests: &[ModelTurnRequest]) -> Vec<String> {
    requests
        .iter()
        .map(|request| {
            request
                .messages
                .iter()
                .rev()
                .find(|message| {
                    message.role == singularity_model::ModelRole::User
                        && !message.content.starts_with("<system-reminder>")
                })
                .map(|message| message.content.clone())
                .unwrap_or_default()
        })
        .collect()
}

/// 把共享的 provider fixture 写入已有的隔离测试 home。
/// 第二个模型在 runtime 与工作台的选择场景间有所不同。
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
        "version": 1,
        "default_model": "openai_compatible/base-model",
        "providers": {"openai_compatible": {
            "base_url": "http://127.0.0.1:9/v1",
            "models": models
        }}
    });
    let auth = serde_json::json!({
        "schema_version": 1,
        "providers": {"openai_compatible": {"api_key": "test-key-placeholder"}}
    });
    for (name, value) in [("config.json", config), ("auth.json", auth)] {
        singularity_core::atomic_replace_bytes(&home.join(name), value.to_string().as_bytes())
            .expect("write private provider fixture");
    }
}

/// 测试 provider 的模型容量快照：与替身声明同一份默认容量。
pub fn test_model_configuration() -> ModelConfigurationSnapshot {
    singularity_model::test_support::ScriptedProvider::ok("").model_configuration()
}

/// 在给定夹具上注入 fake provider 构造会话协调器，返回会话与其 thread 的
/// 规范 session 文件路径；model 为 thread 初始 selector（None 走目录默认）。
/// 夹具由调用方持有，因此需要同一写者协调器的目录操作可与它共享。
pub fn conversation_with(
    fixture: &SessionsFixture,
    provider: Arc<dyn Provider + Send + Sync>,
    model: Option<&str>,
) -> (Arc<Conversation>, PathBuf) {
    let runner = fixture.runner(Some(provider));
    let thread = fixture
        .catalog()
        .create_thread(
            std::env::current_dir().unwrap().to_str().unwrap(),
            model.map(str::to_string),
        )
        .expect("create thread");
    let path = fixture.dir.join(format!("{}.jsonl", thread.thread_id));
    (Conversation::new(runner, thread), path)
}

/// 模型边界门控替身：首个请求到达时发出 started 信号并阻塞，直到测试释放
/// 或关闭通道；经门控时已取消的请求按采样取消语义返回 Cancelled。其余请求
/// 委托给注入的 inner 替身。让断言精确锚定在「turn 已在执行、operation
/// 起始记录已 durable、写者锁已被占用」的时刻。
pub struct GatedProvider {
    started: std::sync::mpsc::Sender<()>,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    inner: Arc<dyn Provider + Send + Sync>,
}

impl GatedProvider {
    /// 包装 inner 新建门控替身，返回替身与「首个请求已到达」的接收端。
    pub fn new(
        inner: Arc<dyn Provider + Send + Sync>,
    ) -> (Arc<Self>, std::sync::mpsc::Receiver<()>) {
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

    /// 进程停止钩子形状：门控恒成功的 DoneProvider。
    pub fn stop_gate() -> (Arc<Self>, std::sync::mpsc::Receiver<()>) {
        Self::new(Arc::new(DoneProvider))
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

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &singularity_core::CancellationToken,
        on_event: &mut dyn FnMut(singularity_model::ProviderStreamEvent),
        record_attempt: &mut dyn FnMut(
            singularity_model::ProviderAttemptEvent,
        ) -> std::io::Result<()>,
    ) -> Result<ModelTurnResponse, singularity_model::ProviderCallError> {
        let _ = self.started.send(());
        if let Some(release) = self.release.lock().expect("gate lock").take() {
            // 阻塞直到测试释放或通道关闭（测试线程退出）。
            let _ = release.recv();
        }
        if cancellation.is_cancelled() {
            return Err(
                ProviderError::new(ModelErrorKind::Cancelled, "cancelled at stop gate").into(),
            );
        }
        self.inner
            .complete_stream(request, cancellation, on_event, record_attempt)
    }
}

/// 恒成功 provider：每个请求返回 done，作为停止钩子门控的放行形态——
/// 同一测试里门控之后的续接请求同样放行。
struct DoneProvider;

impl Provider for DoneProvider {
    fn model_configuration(&self) -> ModelConfigurationSnapshot {
        test_model_configuration()
    }

    fn complete_stream(
        &self,
        _request: &ModelTurnRequest,
        _cancellation: &singularity_core::CancellationToken,
        _on_event: &mut dyn FnMut(singularity_model::ProviderStreamEvent),
        record_attempt: &mut dyn FnMut(
            singularity_model::ProviderAttemptEvent,
        ) -> std::io::Result<()>,
    ) -> Result<ModelTurnResponse, singularity_model::ProviderCallError> {
        use singularity_model::{
            ProviderApiProtocol, ProviderAttemptEvent, ProviderAttemptOccurrence,
            ProviderAttemptStarted, ProviderAttemptStatus,
        };
        let protocol = ProviderApiProtocol::Chat;
        let started = ProviderAttemptStarted {
            provider_name: "done".into(),
            model_name: "done-model".into(),
            actual_api_protocol: protocol,
        };
        record_attempt(ProviderAttemptEvent::Started(started.clone()))?;
        let response = ModelTurnResponse::completed("done");
        record_attempt(ProviderAttemptEvent::Finished(Box::new(
            ProviderAttemptOccurrence {
                started,
                terminal_status: ProviderAttemptStatus::Ok,
                attempt_duration_ms: 0,
                error_category: None,
                diagnostic_code: None,
                retry_after_ms: None,
                usage: None,
            },
        )))?;
        Ok(response)
    }
}
