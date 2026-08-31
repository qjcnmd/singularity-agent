//! 确定性 Provider 替身：脚本化的 attempt 结果，绝不触网。
//!
//! 该替身是全部无费用确定性测试的唯一模型出口：每次 `complete_stream` 消费
//! 脚本中的下一个 attempt（成功文本或类型化失败），并如实投影
//! [`ProviderAttemptEvent`]（Started + Finished），使 provider-attempt 观测、
//! 重试分类与取消路径可在零真实调用下被断言。脚本耗尽后返回显式错误而非静默
//! 重复最后一个结果，保证测试对"多要了一次调用"这类缺陷敏感。

// 测试基础设施：`Mutex` 中毒意味着测试进程已不可继续，直接 panic 收敛。
#![allow(clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::Mutex;

use singularity_core::CancellationToken;

use crate::config::ModelConfigurationSnapshot;
use crate::error::{ModelError, ModelErrorKind, ProviderError};
use crate::provider::Provider;
use crate::provider::contract::{ProviderApiProtocol, ProviderProtocolContract};
use crate::provider::telemetry::{
    ProviderAttemptEvent, ProviderAttemptOccurrence, ProviderAttemptStarted, ProviderAttemptStatus,
    ProviderStreamEvent,
};
use crate::types::{
    ModelMessage, ModelRole, ModelToolCall, ModelToolParseStatus, ModelTurnRequest,
    ModelTurnResponse, ModelUsage,
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
    /// `tool_calls`；工具批次路径的唯一脚本形状。
    ToolCalls {
        text: String,
        calls: Vec<ModelToolCall>,
        usage: Option<ModelUsage>,
    },
    /// 失败：返回给定类型化 [`ProviderError`]。
    Failure(ProviderError),
    /// 失败：先发出可见文本增量，再以类型化错误结束本次 attempt。与传输层
    /// 「首个可见 delta 之后的流失败不可透明重试」的规则同形：错误以
    /// `without_automatic_retry` 返回，attempt 终态为 `Error`。
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
        Self::tool_calls("", [(call_id, tool_name, arguments)])
    }

    /// 可见文本加一组工具调用的成功 attempt；调用按给定顺序进入响应。
    pub fn tool_calls(
        text: impl Into<String>,
        calls: impl IntoIterator<Item = (impl Into<String>, impl Into<String>, serde_json::Value)>,
    ) -> Self {
        let calls = calls
            .into_iter()
            .map(|(call_id, tool_name, arguments)| ModelToolCall {
                tool_call_id: call_id.into(),
                tool_name: tool_name.into(),
                raw_arguments: arguments.to_string(),
                arguments,
                parse_status: ModelToolParseStatus::Valid,
                validation_errors: Vec::new(),
            })
            .collect();
        Self::ToolCalls {
            text: text.into(),
            calls,
            usage: None,
        }
    }

    /// 类型化失败的 attempt。
    pub fn failure(error: ProviderError) -> Self {
        Self::Failure(error)
    }

    /// 已产生可见文本后失败的 attempt：错误携带「不可自动重放」标记，
    /// 复刻传输层对可见流之后失败的定型。
    pub fn visible_then_fail(text: impl Into<String>, error: ProviderError) -> Self {
        Self::VisibleThenFail {
            text: text.into(),
            error: error.without_automatic_retry(),
        }
    }

    /// 按错误种类构造失败 attempt。
    pub fn failure_kind(kind: ModelErrorKind, message: impl Into<String>) -> Self {
        Self::Failure(ProviderError::from_model_error(ModelError::new(
            kind, message,
        )))
    }
}

/// 记录每次请求并按脚本返回 attempt 结果的确定性 Provider。
///
/// 不持有任何 HTTP 客户端：真实 provider 调用在本替身上不可能发生。
#[derive(Default)]
pub struct ScriptedProvider {
    attempts: Mutex<VecDeque<ScriptedAttempt>>,
    requests: Mutex<Vec<ModelTurnRequest>>,
    contract: ProviderProtocolContract,
}

impl ScriptedProvider {
    /// 以脚本 attempt 序列构造替身。
    pub fn new(attempts: impl IntoIterator<Item = ScriptedAttempt>) -> Self {
        Self {
            attempts: Mutex::new(attempts.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            contract: ProviderProtocolContract::default(),
        }
    }

    /// 单轮恒成功替身。
    pub fn ok(text: impl Into<String>) -> Self {
        Self::new([ScriptedAttempt::success(text)])
    }

    /// 覆盖协议能力声明（例如不支持工具、更小上下文窗口）。
    #[must_use]
    pub fn with_contract(mut self, contract: ProviderProtocolContract) -> Self {
        self.contract = contract;
        self
    }

    /// 已记录请求的快照，用于断言每轮实际看到的输入。
    pub fn requests(&self) -> Vec<ModelTurnRequest> {
        self.requests.lock().expect("request log").clone()
    }

    /// 已消费的模型请求次数。
    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("request log").len()
    }

    fn next_attempt(&self) -> Result<ScriptedAttempt, ProviderError> {
        self.attempts
            .lock()
            .expect("attempt script")
            .pop_front()
            .ok_or_else(|| {
                ProviderError::from_model_error(ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    "ScriptedProvider ran out of scripted attempts",
                ))
                .without_automatic_retry()
            })
    }
}

impl Provider for ScriptedProvider {
    fn model_configuration(&self) -> ModelConfigurationSnapshot {
        ModelConfigurationSnapshot {
            provider: "scripted".to_string(),
            model: "scripted-model".to_string(),
            reasoning_variant: None,
            protocol: ProviderApiProtocol::OpenAiChatCompletions,
            capabilities: self.contract.clone(),
            credential_provenance: "test-support".to_string(),
            retry: crate::provider::policy::TurnRetryPolicy::default(),
        }
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        self.requests
            .lock()
            .expect("request log")
            .push(request.clone());
        let model_name = request
            .model_preferences
            .model_name
            .clone()
            .unwrap_or_else(|| "scripted-model".to_string());
        on_attempt(ProviderAttemptEvent::Started(ProviderAttemptStarted {
            attempt: 0,
            provider_name: "scripted".to_string(),
            model_name: model_name.clone(),
            actual_api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
        }));
        match self.next_attempt()? {
            ScriptedAttempt::Panic => panic!("ScriptedProvider scripted panic"),
            ScriptedAttempt::Failure(error) => Self::finish_error(error, model_name, on_attempt),
            ScriptedAttempt::VisibleThenFail { text, error } => {
                if !text.is_empty() {
                    on_event(ProviderStreamEvent::OutputTextDelta { delta: text });
                }
                Self::finish_error(error, model_name, on_attempt)
            }
            ScriptedAttempt::Success { text, usage } => Self::finish_ok(
                text,
                Vec::new(),
                usage,
                None,
                request,
                model_name,
                on_event,
                on_attempt,
            ),
            ScriptedAttempt::ToolCalls { text, calls, usage } => Self::finish_ok(
                text,
                calls,
                usage,
                Some("tool_calls"),
                request,
                model_name,
                on_event,
                on_attempt,
            ),
        }
    }
}

impl ScriptedProvider {
    /// 失败 attempt 的统一投影：`Finished(Error|Cancelled)` 终态事件加原样
    /// 返回的类型化错误（重试许可标记由脚本自己携带）。
    fn finish_error(
        error: ProviderError,
        model_name: String,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        let category = error.error.category();
        let diagnostic_code = error.error.code.clone();
        on_attempt(ProviderAttemptEvent::Finished(Box::new(
            ProviderAttemptOccurrence {
                attempt: 0,
                provider_name: "scripted".to_string(),
                model_name,
                actual_api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
                terminal_status: if error.error.kind == ModelErrorKind::Cancelled {
                    ProviderAttemptStatus::Cancelled
                } else {
                    ProviderAttemptStatus::Error
                },
                attempt_duration_ms: 0,
                error_category: Some(category),
                diagnostic_code,
                retry_after_ms: error
                    .retry_after
                    .map(|delay| delay.as_millis().min(u128::from(u64::MAX)) as u64),
                retry_after_source: error
                    .retry_after
                    .map(|_| singularity_protocol::RetryAfterSource::ProviderHeader),
                usage: None,
            },
        )));
        Err(error)
    }
}

impl ScriptedProvider {
    /// 成功 attempt 的统一投影：可见文本增量、`Ok` attempt 终态事件与
    /// assistant 响应（文本 + 可选工具调用）一次成型。
    #[allow(clippy::too_many_arguments)]
    fn finish_ok(
        text: String,
        calls: Vec<ModelToolCall>,
        usage: Option<ModelUsage>,
        finish_reason: Option<&'static str>,
        request: &ModelTurnRequest,
        model_name: String,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        if !text.is_empty() {
            on_event(ProviderStreamEvent::OutputTextDelta {
                delta: text.clone(),
            });
        }
        on_attempt(ProviderAttemptEvent::Finished(Box::new(
            ProviderAttemptOccurrence {
                attempt: 0,
                provider_name: "scripted".to_string(),
                model_name,
                actual_api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
                terminal_status: ProviderAttemptStatus::Ok,
                attempt_duration_ms: 0,
                error_category: None,
                diagnostic_code: None,
                retry_after_ms: None,
                retry_after_source: None,
                usage: usage.clone(),
            },
        )));
        let mut message = ModelMessage::text(ModelRole::Assistant, text);
        message.tool_calls = calls;
        let mut response = ModelTurnResponse {
            request_id: request.request_id.clone(),
            response_id: "resp-scripted".to_string(),
            assistant_message: Some(message),
            usage: ModelUsage::default(),
            finish_reason: finish_reason.map(str::to_string),
            provider_name: None,
            model_name: None,
            provider_reasoning_history: Vec::new(),
        };
        if let Some(usage) = usage {
            response.usage = usage;
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;

    fn request(id: &str) -> ModelTurnRequest {
        ModelTurnRequest::new(
            id.to_string(),
            vec![crate::types::ModelMessage::text(
                crate::types::ModelRole::User,
                "hi",
            )],
        )
    }

    /// 脚本按序消费：成功返回文本并记录请求，耗尽后显式失败。
    #[test]
    fn scripted_provider_consumes_attempts_in_order() {
        let provider = ScriptedProvider::new([
            ScriptedAttempt::success("first"),
            ScriptedAttempt::success("second"),
        ]);
        let cancellation = CancellationToken::new();
        let first = provider
            .complete_stream(&request("r1"), &cancellation, &mut |_| {}, &mut |_| {})
            .unwrap();
        assert_eq!(first.assistant_message.unwrap().content, "first");
        let second = provider
            .complete_stream(&request("r2"), &cancellation, &mut |_| {}, &mut |_| {})
            .unwrap();
        assert_eq!(second.assistant_message.unwrap().content, "second");
        assert_eq!(provider.request_count(), 2);
        let exhausted =
            provider.complete_stream(&request("r3"), &cancellation, &mut |_| {}, &mut |_| {});
        assert!(exhausted.is_err(), "exhausted script must fail loudly");
    }

    /// 失败 attempt 如实投影类型化错误与 Finished(Error) 事件。
    #[test]
    fn scripted_failure_projects_attempt_event() {
        let provider = ScriptedProvider::new([ScriptedAttempt::failure_kind(
            ModelErrorKind::RateLimited,
            "slow down",
        )]);
        let mut statuses = Vec::new();
        let result = provider.complete_stream(
            &request("r1"),
            &CancellationToken::new(),
            &mut |_| {},
            &mut |event| {
                if let ProviderAttemptEvent::Finished(occurrence) = event {
                    statuses.push(occurrence.terminal_status);
                }
            },
        );
        assert!(result.is_err());
        assert_eq!(statuses, vec![ProviderAttemptStatus::Error]);
    }
}
