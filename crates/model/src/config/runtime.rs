//! Runtime provider selection、transport capability 组装与不可变快照。
//!
//! 用户配置与认证读取位于兄弟 `user` 模块；本模块只组装 AgentLoop
//! 执行所需的 provider 实例与协议能力。

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::*;
use crate::error::ModelErrorCategory;
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

/// 不可变、含密钥的 provider 实例及其白名单模型选择。此类型绝不实现
/// `Debug`；外层快照只打印脱敏状态。
#[derive(Clone)]
pub(crate) struct ModelSelectionSnapshot {
    pub(crate) default_model: String,
    pub(crate) providers: BTreeMap<String, ConfiguredProvider>,
}

/// 服务级模型提供方配置快照，包含脱敏状态和已初始化的模型提供方。
///
/// 只捕获一次，使报告与执行使用同一份配置，同时不暴露 API 密钥
/// 或其他原始配置值。
#[derive(Clone)]
pub struct ProviderConfigSnapshot {
    redacted_config: ModelProviderConfig,
    configuration: ProviderConfigurationStatus,
    provider: Result<OpenAiProvider, ProviderError>,
    model_selection: Option<std::sync::Arc<ModelSelectionSnapshot>>,
}

impl fmt::Debug for ProviderConfigSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderConfigSnapshot")
            .field("redacted_config", &self.redacted_config)
            .field("configuration", &self.configuration)
            .field("model_selection_present", &self.model_selection.is_some())
            .finish()
    }
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
        let (redacted_config, provider, model_selection) = match user_config {
            Err(error) => (redacted_models_config(), Err(error), None),
            Ok(Some(user_config)) => {
                match capture_user_model_selection(&user_config, &runtime_handle) {
                    Ok((catalog, redacted)) => {
                        let provider = provider_for_selection(&catalog, None);
                        (redacted, provider, Some(std::sync::Arc::new(catalog)))
                    }
                    Err(error) => (redacted_models_config(), Err(error), None),
                }
            }
            Ok(None) => (
                redacted_models_config(),
                Err(missing_provider_config_error(crate::USER_CONFIG_FILE_NAME)),
                None,
            ),
        };
        let mut configuration = ProviderConfigurationStatus::from_config(&redacted_config);
        if configuration.configured
            && let Err(error) = &provider
        {
            configuration.configured = false;
            configuration.blocker = provider_initialization_blocker(&error.error);
        }
        Self {
            redacted_config,
            configuration,
            provider,
            model_selection,
        }
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

    /// 返回脱敏后的 provider 配置。
    pub fn redacted_config(&self) -> &ModelProviderConfig {
        &self.redacted_config
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
        if let Some(selection) = &self.model_selection {
            return provider_for_selection(selection, selector);
        }
        // 不变量：构造成功的 provider 必带模型选择；走到这里说明快照未配置，
        // 原样返回捕获期记录的配置错误。
        Err(self
            .provider
            .clone()
            .err()
            .unwrap_or_else(|| missing_provider_config_error(crate::USER_CONFIG_FILE_NAME)))
    }
}

pub(crate) fn redacted_models_config() -> ModelProviderConfig {
    ModelProviderConfig {
        provider_name: None,
        model_name: None,
        base_url_present: false,
        api_key_present: false,
    }
}

pub(crate) fn configuration_error(message: impl Into<String>, code: &'static str) -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(ModelErrorKind::InvalidRequest, message)
            .with_provider_diagnostic(code, ProviderErrorStage::ClientInitialization),
    )
}

pub(crate) fn provider_initialization_blocker(error: &ModelError) -> Option<ModelBlockerKind> {
    match error.category() {
        ModelErrorCategory::Authentication => Some(ModelBlockerKind::AuthenticationProviderError),
        ModelErrorCategory::Network | ModelErrorCategory::ProviderUnavailable => {
            Some(ModelBlockerKind::BaseUrlNetworkError)
        }
        ModelErrorCategory::ModelConfiguration
        | ModelErrorCategory::InvalidRequest
        | ModelErrorCategory::UnsupportedCapability => Some(ModelBlockerKind::ModelNameConfigError),
        ModelErrorCategory::Cancelled
        | ModelErrorCategory::ContextLengthExceeded
        | ModelErrorCategory::JsonSchema
        | ModelErrorCategory::ContentFilter
        | ModelErrorCategory::UnknownProviderError => None,
    }
}

impl ProviderConfigurationStatus {
    /// 从 provider 配置生成脱敏状态。
    pub fn from_config(config: &ModelProviderConfig) -> Self {
        let validation = validate_provider_config(config);
        Self {
            configured: validation.valid,
            provider_name: config.provider_name.clone(),
            model_name: config.model_name.clone(),
            api_key_status: redacted_presence(config.api_key_present),
            base_url_status: redacted_presence(config.base_url_present),
            blocker: if validation.valid {
                None
            } else {
                Some(ModelBlockerKind::RequiredConfigMissing)
            },
        }
    }
}
