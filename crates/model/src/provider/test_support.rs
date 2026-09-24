//! 确定性 Provider 替身：脚本化的 attempt 结果，绝不触网。
//!
//! 该替身是全部无费用确定性测试的唯一模型出口：每次 complete_stream 消费
//! 脚本中的下一个 attempt（成功文本或类型化失败），并如实投影
//! ProviderAttemptEvent（Started + Finished），使 provider-attempt 观测、
//! 重试分类与取消路径可在零真实调用下被断言。脚本耗尽后返回显式错误而非静默
//! 重复最后一个结果，保证测试对"多要了一次调用"这类缺陷敏感。

// 测试基础设施：Mutex 中毒意味着测试进程已不可继续，直接 panic 收敛。
#![allow(clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::Mutex;

use tokio_util::sync::CancellationToken;

use crate::config::ModelConfigurationSnapshot;
use crate::error::{ModelErrorKind, ProviderError};
use crate::provider::contract::ProviderApiProtocol;
use crate::provider::telemetry::{
    ProviderAttemptEvent, ProviderAttemptOccurrence, ProviderAttemptStarted, ProviderStreamEvent,
};
use crate::provider::{Provider, ProviderObserver};
use crate::types::{
    ModelMessage, ModelRole, ModelStopReason, ModelToolCall, ModelTurnRequest, ModelTurnResponse,
    ModelUsage,
};

/// 一次脚本化 attempt 的结果。
#[derive(Debug, Clone)]
pub enum ScriptedAttempt {
    /// 成功：返回给定 assistant 文本，可选携带真实 usage。
    Success {
        text: String,
        usage: Option<ModelUsage>,
    },
    /// 成功：assistant 文本（可为空）携带工具调用，finish reason 为
    /// tool_calls；工具批次路径的唯一脚本形状。
    ToolCalls {
        text: String,
        calls: Vec<ModelToolCall>,
        usage: Option<ModelUsage>,
    },
    /// 失败：返回给定类型化 ProviderError。
    Failure(ProviderError),
    /// 先发出可见文本增量，再以类型化错误结束本次 attempt。
    VisibleThenFail { text: String, error: ProviderError },
    /// 抛出 panic：验证工具/采样层的 panic 隔离路径。
    Panic,
}

impl ScriptedAttempt {
    /// 无 usage 的成功 attempt。
    pub fn success(text: impl Into<String>) -> Self {
        Self::Success {
            text: text.into(),
            usage: None,
        }
    }

    /// 携带真实 usage 的成功 attempt。
    pub fn success_with_usage(text: impl Into<String>, usage: ModelUsage) -> Self {
        Self::Success {
            text: text.into(),
            usage: Some(usage),
        }
    }

    /// 单个工具调用的成功 attempt（无可见文本）。
    pub fn tool_call(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self::ToolCalls {
            text: String::new(),
            calls: vec![ModelToolCall {
                tool_call_id: call_id.into(),
                tool_name: tool_name.into(),
                arguments,
            }],
            usage: None,
        }
    }

    /// 已产生可见文本后失败的 attempt。
    pub fn visible_then_fail(text: impl Into<String>, error: ProviderError) -> Self {
        Self::VisibleThenFail {
            text: text.into(),
            error,
        }
    }

    /// 按错误种类构造失败 attempt。
    pub fn failure_kind(kind: ModelErrorKind, message: impl Into<String>) -> Self {
        Self::Failure(ProviderError::new(kind, message))
    }
}

/// 记录每次请求并按脚本返回 attempt 结果的确定性 Provider。
///
/// 不持有任何 HTTP 客户端：真实 provider 调用在本替身上不可能发生。
pub struct ScriptedProvider {
    attempts: Mutex<VecDeque<ScriptedAttempt>>,
    requests: Mutex<Vec<ModelTurnRequest>>,
}

impl ScriptedProvider {
    /// 以脚本 attempt 序列构造替身。
    pub fn new(attempts: impl IntoIterator<Item = ScriptedAttempt>) -> Self {
        Self {
            attempts: Mutex::new(attempts.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// 单轮恒成功替身。
    pub fn ok(text: impl Into<String>) -> Self {
        Self::new([ScriptedAttempt::success(text)])
    }

    /// 已记录请求的快照，用于断言每轮实际看到的输入。
    pub fn requests(&self) -> Vec<ModelTurnRequest> {
        self.requests.lock().expect("request log").clone()
    }

    fn next_attempt(&self) -> Result<ScriptedAttempt, ProviderError> {
        self.attempts
            .lock()
            .expect("attempt script")
            .pop_front()
            .ok_or_else(|| {
                ProviderError::new(
                    ModelErrorKind::InvalidRequest,
                    "ScriptedProvider ran out of scripted attempts",
                )
            })
    }
}

impl Provider for ScriptedProvider {
    fn model_configuration(&self) -> ModelConfigurationSnapshot {
        ModelConfigurationSnapshot {
            max_context_tokens: crate::DEFAULT_MAX_CONTEXT_TOKENS,
            max_output_tokens: crate::DEFAULT_MAX_OUTPUT_TOKENS,
        }
    }

    fn complete_stream<'a>(
        &'a self,
        request: &'a ModelTurnRequest,
        _cancellation: &'a CancellationToken,
        observer: &'a mut dyn ProviderObserver,
    ) -> crate::provider::ProviderFuture<'a> {
        Box::pin(async move {
            let started = ProviderAttemptStarted {
                provider_name: "scripted".to_string(),
                model_name: "scripted-model".to_string(),
                actual_api_protocol: ProviderApiProtocol::Chat,
            };
            observer
                .record_attempt(ProviderAttemptEvent::Started(started.clone()))
                .await?;
            self.requests
                .lock()
                .expect("request log")
                .push(request.clone());
            match self.next_attempt().unwrap_or_else(ScriptedAttempt::Failure) {
                ScriptedAttempt::Panic => panic!("ScriptedProvider scripted panic"),
                ScriptedAttempt::Failure(error) => {
                    Self::finish_error(error, started, observer).await
                }
                ScriptedAttempt::VisibleThenFail { text, error } => {
                    if !text.is_empty() {
                        observer.on_stream(ProviderStreamEvent::OutputTextDelta { delta: text });
                    }
                    Self::finish_error(error, started, observer).await
                }
                ScriptedAttempt::Success { text, usage } => {
                    Self::finish_ok(text, Vec::new(), usage, None, started, observer).await
                }
                ScriptedAttempt::ToolCalls { text, calls, usage } => {
                    Self::finish_ok(
                        text,
                        calls,
                        usage,
                        Some(ModelStopReason::Stop),
                        started,
                        observer,
                    )
                    .await
                }
            }
        })
    }
}

impl ScriptedProvider {
    /// 失败 attempt 的统一投影：Finished(Error|Cancelled) 终态事件加原样
    /// 返回的类型化错误（重试许可标记由脚本自己携带）。
    async fn finish_error(
        error: ProviderError,
        started: ProviderAttemptStarted,
        observer: &mut dyn ProviderObserver,
    ) -> Result<ModelTurnResponse, crate::ProviderCallError> {
        observer
            .record_attempt(ProviderAttemptEvent::Finished(Box::new(
                ProviderAttemptOccurrence::finished(started, 0, None, Some(&error)),
            )))
            .await?;
        Err(error.into())
    }
}

impl ScriptedProvider {
    /// 成功 attempt 的统一投影：可见文本增量、Ok attempt 终态事件与
    /// assistant 响应（文本 + 可选工具调用）一次成型。
    async fn finish_ok(
        text: String,
        calls: Vec<ModelToolCall>,
        usage: Option<ModelUsage>,
        stop_reason: Option<ModelStopReason>,
        started: ProviderAttemptStarted,
        observer: &mut dyn ProviderObserver,
    ) -> Result<ModelTurnResponse, crate::ProviderCallError> {
        if !text.is_empty() {
            observer.on_stream(ProviderStreamEvent::OutputTextDelta {
                delta: text.clone(),
            });
        }
        observer
            .record_attempt(ProviderAttemptEvent::Finished(Box::new(
                ProviderAttemptOccurrence::finished(started, 0, usage.clone(), None),
            )))
            .await?;
        let mut message = ModelMessage::text(ModelRole::Assistant, text);
        message.tool_calls = calls;
        let mut response = ModelTurnResponse {
            assistant_message: message,
            thinking: String::new(),
            usage: ModelUsage::default(),
            stop_reason,
        };
        if let Some(usage) = usage {
            response.usage = usage;
        }
        Ok(response)
    }
}
