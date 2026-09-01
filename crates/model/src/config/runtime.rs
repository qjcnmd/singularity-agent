//! Runtime provider selection、transport capability 组装与不可变快照。
//!
//! 用户配置与认证读取位于兄弟 `user` 模块；本模块只组装 AgentLoop
//! 执行所需的 provider 实例与协议能力。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::*;
use crate::provider::contract::ProviderProtocolContract;
use crate::provider::policy::TurnRetryPolicy;

/// 一次 turn 的不可变模型配置快照（data-model.md 的 Model Configuration
/// Snapshot）：逐回合冻结 selector、声明协议、能力合同、重试策略与凭据
/// 来源。设置变更只产生未来回合的新快照，绝不改写活动快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelConfigurationSnapshot {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_variant: Option<String>,
    pub protocol: ProviderApiProtocol,
    pub capabilities: ProviderProtocolContract,
    /// 凭据来源标签（配置文件与 provider 键），不含任何密钥材料。
    pub credential_provenance: String,
    pub retry: TurnRetryPolicy,
}

impl ModelConfigurationSnapshot {
    /// 快照的完整 selector（`provider/model[#variant]`）。
    pub fn selector(&self) -> String {
        compose_model_selector(
            &self.provider,
            &self.model,
            self.reasoning_variant.as_deref(),
        )
    }

    /// 请求前压缩判定使用的上下文窗口（声明缺失时取默认上限）。
    pub fn context_window(&self) -> u64 {
        u64::from(
            self.capabilities
                .max_context_tokens
                .unwrap_or(crate::DEFAULT_MAX_CONTEXT_TOKENS),
        )
    }

    /// provider 声明的输出上限。
    pub fn max_output_tokens(&self) -> u64 {
        u64::from(self.capabilities.max_output_tokens)
    }
}

/// 不可变、含密钥的 provider 实例及其白名单模型选择。此类型不实现 `Debug`。
#[derive(Clone)]
pub(crate) struct ModelSelectionSnapshot {
    pub(crate) default_model: String,
    pub(crate) providers: BTreeMap<String, ConfiguredProvider>,
}

/// 服务级模型选择快照。只捕获一次，使默认选择与按 selector 解析共享同一事实源。
#[derive(Clone)]
pub struct ProviderConfigSnapshot {
    selection: Result<std::sync::Arc<ModelSelectionSnapshot>, ProviderError>,
}

impl ProviderConfigSnapshot {
    /// 读取用户配置目录（`config.json` + `auth.json`）并固定一份 provider
    /// 配置快照；异步执行使用调用方注入的 runtime。
    pub fn capture(runtime_handle: tokio::runtime::Handle) -> Self {
        Self::from_user_config(read_user_config_data(), runtime_handle)
    }

    fn from_user_config(
        user_config: Result<Option<UserConfigData>, ProviderError>,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        let selection = match user_config {
            Err(error) => Err(error),
            Ok(Some(user_config)) => {
                capture_user_model_selection(&user_config, &runtime_handle).map(std::sync::Arc::new)
            }
            Ok(None) => Err(missing_provider_config_error(crate::USER_CONFIG_FILE_NAME)),
        };
        Self { selection }
    }

    /// 测试接缝：从指定用户配置目录捕获快照，不读进程环境。生产路径一律经
    /// [`Self::capture`] 解析 `SINGULARITY_HOME`。
    #[cfg(feature = "test-support")]
    pub fn capture_from_directory(
        directory: &std::path::Path,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        Self::from_user_config(
            read_user_config_data_from_directory(directory.to_path_buf()),
            runtime_handle,
        )
    }

    /// 返回用户配置目录解析出的默认 selector（`provider/model#effort`）；
    /// provider 未配置或无法解析时返回 `None`（调用方保留 `Thread.model` 为 NULL）。
    pub fn resolved_default_selector(&self) -> Option<String> {
        self.provider().ok()?.resolved_selector()
    }

    /// 从快照创建 provider 实例。
    pub fn provider(&self) -> Result<OpenAiProvider, ProviderError> {
        self.provider_for_selector(None)
    }

    /// 对照此不可变快照解析持久化的 `provider/model[#variant]` 引用；返回的
    /// provider 克隆带裸 model id 与恰好一个目录声明的协议。turn 的
    /// [`ModelConfigurationSnapshot`] 由该 provider 实例自身派生。
    pub fn provider_for_selector(
        &self,
        selector: Option<&str>,
    ) -> Result<OpenAiProvider, ProviderError> {
        match &self.selection {
            Ok(selection) => provider_for_selection(selection, selector),
            Err(error) => Err(error.clone()),
        }
    }
}

pub(crate) fn configuration_error(message: impl Into<String>, code: &'static str) -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(ModelErrorKind::InvalidRequest, message)
            .with_provider_diagnostic(code, ProviderErrorStage::ClientInitialization),
    )
}
