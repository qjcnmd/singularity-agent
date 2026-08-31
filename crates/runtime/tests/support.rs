//! runtime 集成测试的共享确定性夹具与钩子（T004）。
//!
//! 提供隔离的临时 sessions 目录、进程级写者协调器、provider 配置快照、
//! 请求输入投影，以及进程停止钩子 [`StopGateProvider`]：首个请求到达时发出
//! 信号并阻塞，让测试在 turn 仍在执行、写者锁仍被占用时观测 durable 事实，
//! 并按采样取消语义响应取消令牌。
//!
//! 全部夹具隔离于真实 `SINGULARITY_HOME`，provider 经内存替身注入，绝不触网。
#![allow(clippy::unwrap_used, clippy::expect_used)] // 夹具构造失败即测试环境损坏，直接 panic

use std::path::{Path, PathBuf};
use std::sync::Arc;

use singularity_agent::session::WriterLockCoordinator;
use singularity_model::{
    ModelConfigurationSnapshot, ModelTurnRequest, ModelTurnResponse, Provider, ProviderError,
    ProviderProtocolContract,
};

/// 每个测试独立的临时 sessions 目录。
pub fn temp_sessions() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().expect("temp home");
    std::fs::create_dir_all(dir.path().join("sessions")).expect("sessions dir");
    dir
}

/// 进程级写者锁协调器（每测试独立目录各持一个即可）。
pub fn coordinator(sessions: &Path) -> Arc<WriterLockCoordinator> {
    Arc::new(WriterLockCoordinator::new(sessions))
}

/// 每次请求中最后一条 user 消息：即该请求所属 turn 的新增输入。
/// （更早的输入会作为历史上下文重放，不能用于唯一性判断。）
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

/// 目录快照来自隔离的用户配置目录：config.json 声明 openai_compatible 的
/// base-model 与 base-model-2，auth.json 提供测试 key。fake provider 经
/// provider_override 注入，不经 HTTP；Handle 背后的 runtime 无需存活。
pub fn provider_snapshot() -> singularity_model::ProviderConfigSnapshot {
    static FIXTURE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let home = FIXTURE.get_or_init(|| {
        let directory = tempfile::tempdir().expect("snapshot fixture home");
        let path = directory.path().to_path_buf();
        let config = serde_json::json!({
            "version": 1,
            "default_provider": "openai_compatible",
            "default_model": "openai_compatible/base-model",
            "providers": {
                "openai_compatible": {
                    "base_url": "http://127.0.0.1:9/v1",
                    "models": {
                        "base-model": {
                            "api_protocol": "chat",
                            "max_context_tokens": 128_000,
                            "max_output_tokens": 4_096
                        },
                        "base-model-2": {
                            "api_protocol": "chat",
                            "max_context_tokens": 128_000,
                            "max_output_tokens": 4_096
                        }
                    }
                }
            }
        });
        std::fs::write(path.join("config.json"), config.to_string()).expect("write fixture config");
        let auth = serde_json::json!({
            "schema_version": 1,
            "providers": { "openai_compatible": { "api_key": "test-key-placeholder" } }
        });
        let auth_path = path.join("auth.json");
        std::fs::write(&auth_path, auth.to_string()).expect("write fixture auth");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))
                .expect("restrict fixture auth");
        }
        // fixture 目录随进程存活：capture 按目录读取两文件。
        std::mem::forget(directory);
        path
    });
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = runtime.handle().clone();
    std::mem::forget(runtime);
    singularity_model::ProviderConfigSnapshot::capture_from_directory(home, handle)
}

/// 测试 provider 的模型配置快照：能力合同取默认，身份字段仅供快照一致性。
pub fn test_model_configuration() -> ModelConfigurationSnapshot {
    ModelConfigurationSnapshot {
        provider: "test".to_string(),
        model: "test-model".to_string(),
        reasoning_variant: None,
        protocol: singularity_model::ProviderApiProtocol::OpenAiChatCompletions,
        capabilities: ProviderProtocolContract::default(),
        credential_provenance: "test".to_string(),
        retry: singularity_model::TurnRetryPolicy::default(),
    }
}

/// 进程停止钩子：首个请求到达时发出 `started` 信号并阻塞，直到测试取消或
/// 释放。让断言精确锚定在「turn 已在执行、operation 起始记录已 durable、
/// 写者锁已被占用」的时刻。后续请求直接放行。
pub struct StopGateProvider {
    started: std::sync::mpsc::Sender<()>,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl StopGateProvider {
    /// 新建替身，返回替身与「首个请求已到达」的接收端。
    pub fn new() -> (Arc<Self>, std::sync::mpsc::Receiver<()>) {
        let (sender, receiver) = std::sync::mpsc::channel();
        (
            Arc::new(Self {
                started: sender,
                release: std::sync::Mutex::new(None),
            }),
            receiver,
        )
    }

    /// 注入一个释放通道：测试通过它放行被阻塞的请求（可选）。
    pub fn with_release(&self, release: std::sync::mpsc::Receiver<()>) {
        *self.release.lock().expect("gate lock") = Some(release);
    }
}

impl Provider for StopGateProvider {
    fn model_configuration(&self) -> ModelConfigurationSnapshot {
        test_model_configuration()
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &singularity_core::CancellationToken,
        _on_event: &mut dyn FnMut(singularity_model::ProviderStreamEvent),
        _on_attempt: &mut dyn FnMut(singularity_model::ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        let _ = self.started.send(());
        if let Some(release) = self.release.lock().expect("gate lock").take() {
            // 阻塞直到测试释放或通道关闭（测试线程退出）。
            let _ = release.recv();
        }
        if cancellation.is_cancelled() {
            return Err(ProviderError::from_model_error(
                singularity_model::ModelError::new(
                    singularity_model::ModelErrorKind::Cancelled,
                    "cancelled at stop gate",
                ),
            ));
        }
        Ok(ModelTurnResponse::completed(
            request.request_id.clone(),
            "resp-stop-gate",
            "done".to_string(),
        ))
    }
}
