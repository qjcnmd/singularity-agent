//! Runtime provider selection、transport capability 组装与不可变快照。
//!
//! 用户配置与认证读取位于兄弟 `user` 模块；本模块只组装 AgentLoop
//! 执行所需的 provider 实例与协议能力。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use singularity_protocol::{
    CredentialConfigured, ModelConfigurationStatus, ProviderApiProtocol as ProviderProtocolInput,
    ProviderConfigurationInput, RedactedModel, RedactedModelCatalog, RedactedProvider,
    RedactedReasoningVariant,
};

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
        self.provider_for_selector(None).ok()?.resolved_selector()
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

pub struct ModelConfigOwner {
    directory: PathBuf,
    runtime_handle: tokio::runtime::Handle,
}

impl ModelConfigOwner {
    pub fn open(runtime_handle: tokio::runtime::Handle) -> Result<Self, ProviderError> {
        let directory = user_config_directory_result()?.ok_or_else(|| {
            user_config_error("cannot resolve the Singularity configuration directory")
        })?;
        Ok(Self {
            directory,
            runtime_handle,
        })
    }

    #[cfg(feature = "test-support")]
    pub fn open_at(directory: PathBuf, runtime_handle: tokio::runtime::Handle) -> Self {
        Self {
            directory,
            runtime_handle,
        }
    }

    pub fn snapshot(&self) -> ProviderConfigSnapshot {
        ProviderConfigSnapshot::from_user_config(
            read_user_config_data_from_directory(self.directory.clone()),
            self.runtime_handle.clone(),
        )
    }

    pub fn redacted_catalog(&self) -> RedactedModelCatalog {
        match read_user_config_data_from_directory(self.directory.clone()) {
            Ok(Some(data)) => catalog_from_data(&data, &self.runtime_handle),
            Ok(None) => RedactedModelCatalog {
                configuration: ModelConfigurationStatus::Missing,
                message: Some("配置一个模型提供方后即可开始新任务。".to_string()),
                default_selector: None,
                providers: Vec::new(),
            },
            Err(error) => RedactedModelCatalog {
                configuration: ModelConfigurationStatus::Invalid,
                message: Some(error.to_string()),
                default_selector: None,
                providers: Vec::new(),
            },
        }
    }

    pub fn save_provider(
        &mut self,
        input: ProviderConfigurationInput,
    ) -> Result<RedactedModelCatalog, ProviderError> {
        validate_identifier(&input.provider_id, "provider id")?;
        validate_base_url(&input.base_url)?;
        if input.models.is_empty() {
            return Err(user_config_error(
                "provider configuration requires at least one model",
            ));
        }
        let existing = read_user_config_data_from_directory(self.directory.clone())?;
        let mut config = existing
            .as_ref()
            .map(|data| data.config.clone())
            .unwrap_or_default();
        let auth = existing.map(|data| data.auth).unwrap_or_default();
        let previous_models = config
            .providers
            .get(&input.provider_id)
            .map(|provider| provider.models.clone())
            .unwrap_or_default();
        let mut models = BTreeMap::new();
        for model in input.models {
            validate_model_id(&model.model_id, "model id")?;
            if models.contains_key(&model.model_id) {
                return Err(user_config_error("provider model ids must be unique"));
            }
            let api_protocol = match model.api_protocol {
                ProviderProtocolInput::Chat => "chat",
                ProviderProtocolInput::Responses => "responses",
            };
            let protocol = parse_catalog_protocol(api_protocol)?;
            if let Some(limit) = model.max_context_tokens {
                validate_catalog_limit(limit, "max_context_tokens", MAX_CONFIGURED_CONTEXT_TOKENS)?;
            }
            if let Some(limit) = model.max_output_tokens {
                validate_catalog_limit(limit, "max_output_tokens", MAX_CONFIGURED_OUTPUT_TOKENS)?;
            }
            let mut variants = BTreeMap::new();
            for variant in model.reasoning_variants {
                validate_identifier(&variant.id, "reasoning variant")?;
                if variants
                    .insert(
                        variant.id,
                        ModelsFileReasoningVariant {
                            enabled: variant.enabled,
                            wire_effort: variant.wire_effort,
                        },
                    )
                    .is_some()
                {
                    return Err(user_config_error("reasoning variant ids must be unique"));
                }
            }
            validate_reasoning_variants(protocol, &variants, model.default_variant.as_deref())?;
            parse_tool_reasoning_history(model.tool_reasoning_history.as_deref(), protocol)?;
            let previous = previous_models
                .get(&model.model_id)
                .cloned()
                .unwrap_or_default();
            models.insert(
                model.model_id,
                UserConfigModel {
                    api_protocol: Some(api_protocol.to_string()),
                    max_context_tokens: model.max_context_tokens,
                    max_output_tokens: model.max_output_tokens,
                    reasoning_variants: variants,
                    default_variant: model.default_variant,
                    tool_reasoning_history: model.tool_reasoning_history,
                    supports_developer_role: previous.supports_developer_role,
                    supports_tool_choice: previous.supports_tool_choice,
                    requires_reasoning_content_for_tool_calls: previous
                        .requires_reasoning_content_for_tool_calls,
                    requires_assistant_content_for_tool_calls: previous
                        .requires_assistant_content_for_tool_calls,
                    thinking_wire_format: previous.thinking_wire_format,
                },
            );
        }
        let first_model = models
            .keys()
            .next()
            .cloned()
            .ok_or_else(|| user_config_error("provider configuration requires a model"))?;
        config.providers.insert(
            input.provider_id.clone(),
            UserConfigProvider {
                base_url: input.base_url,
                models,
            },
        );
        if input.make_default || config.default_provider.is_none() {
            config.default_provider = Some(input.provider_id.clone());
            config.default_model = Some(compose_model_selector(
                &input.provider_id,
                &first_model,
                None,
            ));
        }
        write_json_file(
            &self.directory,
            crate::USER_CONFIG_FILE_NAME,
            &config,
            false,
        )?;
        Ok(catalog_from_data(
            &UserConfigData { config, auth },
            &self.runtime_handle,
        ))
    }

    pub fn set_api_key(
        &mut self,
        provider_id: &str,
        api_key: &str,
    ) -> Result<CredentialConfigured, ProviderError> {
        validate_identifier(provider_id, "provider id")?;
        validate_provider_value(api_key, "api_key")?;
        if api_key.is_empty() {
            return Err(user_config_error("API key must not be empty"));
        }
        let mut auth = match read_user_config_data_from_directory(self.directory.clone())? {
            Some(data) => data.auth,
            None => UserAuthFile::default(),
        };
        auth.providers.insert(
            provider_id.to_string(),
            UserAuthProvider {
                api_key: api_key.to_string(),
            },
        );
        write_json_file(&self.directory, crate::USER_AUTH_FILE_NAME, &auth, true)?;
        Ok(CredentialConfigured {
            provider_id: provider_id.to_string(),
            credential_configured: true,
        })
    }
}

fn catalog_from_data(
    data: &UserConfigData,
    runtime_handle: &tokio::runtime::Handle,
) -> RedactedModelCatalog {
    let selection = capture_user_model_selection(data, runtime_handle);
    let (configuration, message, default_selector) = match selection {
        Ok(selection) => (
            ModelConfigurationStatus::Ready,
            None,
            Some(selection.default_model),
        ),
        Err(error) if error.error.kind == crate::ModelErrorKind::AuthError => (
            ModelConfigurationStatus::Missing,
            Some(error.to_string()),
            configured_default_selector(&data.config),
        ),
        Err(error) => (
            ModelConfigurationStatus::Invalid,
            Some(error.to_string()),
            configured_default_selector(&data.config),
        ),
    };
    let providers = data
        .config
        .providers
        .iter()
        .map(|(provider_id, provider)| RedactedProvider {
            provider_id: provider_id.clone(),
            base_url: provider.base_url.clone(),
            credential_configured: data
                .auth
                .providers
                .get(provider_id)
                .is_some_and(|credential| !credential.api_key.is_empty()),
            models: provider
                .models
                .iter()
                .map(|(model_id, model)| RedactedModel {
                    model_id: model_id.clone(),
                    api_protocol: model
                        .api_protocol
                        .clone()
                        .unwrap_or_else(|| "chat".to_string()),
                    max_context_tokens: model.max_context_tokens,
                    max_output_tokens: model.max_output_tokens,
                    reasoning_variants: model
                        .reasoning_variants
                        .iter()
                        .map(|(id, variant)| RedactedReasoningVariant {
                            id: id.clone(),
                            enabled: variant.enabled,
                            wire_effort: variant.wire_effort.clone(),
                        })
                        .collect(),
                    default_variant: model.default_variant.clone(),
                    tool_reasoning_history: model.tool_reasoning_history.clone(),
                })
                .collect(),
        })
        .collect();
    RedactedModelCatalog {
        configuration,
        message,
        default_selector,
        providers,
    }
}

fn configured_default_selector(config: &UserConfigFile) -> Option<String> {
    config.default_model.clone()
}

fn write_json_file(
    directory: &Path,
    file_name: &str,
    value: &impl Serialize,
    private: bool,
) -> Result<(), ProviderError> {
    singularity_core::create_owner_only_dir(directory)
        .map_err(|_| user_config_error("user provider config directory could not be created"))?;
    let path = directory.join(file_name);
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|_| user_config_error("user provider config could not be serialized"))?;
    bytes.push(b'\n');
    singularity_core::atomic_replace_bytes(&path, &bytes)
        .map_err(|_| user_config_error("user provider config could not be updated"))?;
    if private {
        singularity_core::ensure_owner_only_file(&path)
            .map_err(|_| user_config_error("user provider auth file is not owner-only"))?;
    }
    Ok(())
}

pub(crate) fn configuration_error(message: impl Into<String>, code: &'static str) -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(ModelErrorKind::InvalidRequest, message)
            .with_provider_diagnostic(code, ProviderErrorStage::ClientInitialization),
    )
}
