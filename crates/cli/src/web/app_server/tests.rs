#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use singularity_agent::session::test_support::WorkspaceFixture;
use singularity_model::{
    ModelErrorKind, ModelTurnRequest, ModelTurnResponse, Provider, ProviderError,
    ProviderStreamEvent,
};
use singularity_protocol::{RpcErrorCode, StreamEvent, Workspace};
use singularity_runtime::test_support::SessionsFixture;
use tokio_util::sync::CancellationToken;

use super::*;

mod compaction;
mod execution;
mod history;
mod queue;
mod workspaces;

struct BlockingProvider {
    started: Sender<String>,
    release: Arc<Mutex<Receiver<()>>>,
    /// 放行后额外发布的增量条数：0 表示只发布一条固定增量。
    deltas: usize,
}

impl Provider for BlockingProvider {
    fn model_configuration(&self) -> singularity_model::ModelConfigurationSnapshot {
        singularity_runtime::test_support::test_model_configuration()
    }

    fn complete_stream<'a>(
        &'a self,
        request: &'a ModelTurnRequest,
        cancellation: &'a CancellationToken,
        observer: &'a mut dyn singularity_model::ProviderObserver,
    ) -> singularity_model::ProviderFuture<'a> {
        Box::pin(async move {
            use singularity_model::{
                ProviderApiProtocol, ProviderAttemptEvent, ProviderAttemptOccurrence,
                ProviderAttemptStarted,
            };

            let input = request
                .messages
                .iter()
                .rev()
                .find(|message| message.role == singularity_model::ModelRole::User)
                .map(|message| message.content.clone())
                .unwrap_or_default();
            let panic_requested = input == "panic-provider";
            let protocol = ProviderApiProtocol::Chat;
            let started = ProviderAttemptStarted {
                provider_name: "blocking".into(),
                model_name: "blocking-model".into(),
                actual_api_protocol: protocol,
            };
            observer
                .record_attempt(ProviderAttemptEvent::Started(started.clone()))
                .await?;
            self.started.send(input).expect("report request");
            // 释放信号决定本次 attempt 何时结束；panic 场景也需要它，测试才能先
            // 交付已接受的控制输入，再确定性地观察 panic 之后的交还。
            let release = Arc::clone(&self.release);
            let _ =
                tokio::task::spawn_blocking(move || release.lock().expect("release lock").recv())
                    .await;
            assert!(!panic_requested, "injected provider panic");
            let error = if cancellation.is_cancelled() {
                Some(ProviderError::new(
                    ModelErrorKind::Cancelled,
                    "cancelled by test",
                ))
            } else {
                None
            };
            observer
                .record_attempt(ProviderAttemptEvent::Finished(Box::new(
                    ProviderAttemptOccurrence::finished(started, 0, None, error.as_ref()),
                )))
                .await?;
            if let Some(error) = error {
                return Err(error.into());
            }
            observer.on_stream(ProviderStreamEvent::OutputTextDelta { delta: "do".into() });
            for index in 0..self.deltas {
                observer.on_stream(ProviderStreamEvent::OutputTextDelta {
                    delta: format!("{index} "),
                });
            }
            Ok(ModelTurnResponse::completed("done"))
        })
    }
}

struct Fixture {
    _sessions: SessionsFixture,
    _runtime: tokio::runtime::Runtime,
    workspace: WorkspaceFixture,
    app_server: Arc<AppServer>,
}

fn fixture(provider: Arc<dyn Provider + Send + Sync>) -> Fixture {
    let sessions = SessionsFixture::new();
    singularity_runtime::test_support::write_provider_fixture(sessions.home(), "chosen-model");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let models = Arc::new(Mutex::new(ModelConfigManager::open(
        sessions.home().to_path_buf(),
    )));
    let runner = TurnRunner::new(
        sessions.dir.clone(),
        Arc::clone(&models),
        Arc::clone(&sessions.coordinator),
    )
    .with_provider_override(provider);
    let catalog = sessions.catalog();
    let workspaces = WorkspaceStore::open(sessions.home()).expect("workspace store");
    let app_server = AppServer::new(
        Arc::new(runner),
        runtime.handle().clone(),
        catalog,
        workspaces,
        models,
        sessions.home().to_path_buf(),
    );
    Fixture {
        _sessions: sessions,
        _runtime: runtime,
        workspace: WorkspaceFixture::new(),
        app_server,
    }
}

/// 这些用例共用的最小前置：登记一个项目并新建一个未命名任务。
fn session_in(fixture: &Fixture) -> (&Arc<AppServer>, Workspace, String) {
    let host = &fixture.app_server;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    let id = host
        .create_session(&workspace.workspace_id)
        .unwrap()
        .history
        .summary
        .thread_id;
    (host, workspace, id)
}

fn wait_for_idle(app_server: &AppServer, workspace: &Workspace, sessions: &[String]) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let idle = sessions.iter().all(|session_id| {
            app_server
                .read_session(&workspace.workspace_id, session_id, 100, None)
                .is_ok_and(|snapshot| snapshot.runtime.phase == SessionPhase::Idle)
        });
        if idle {
            return;
        }
        assert!(Instant::now() < deadline, "sessions did not settle");
        std::thread::sleep(Duration::from_millis(10));
    }
}
