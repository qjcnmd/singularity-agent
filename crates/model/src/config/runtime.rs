//! Runtime provider selection、transport capability 组装与不可变快照。
//!
//! 用户配置与认证读取位于兄弟 user 模块；本模块只组装 AgentLoop
//! 执行所需的 provider 实例与协议能力。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use singularity_protocol::{
    CredentialConfigured, ModelConfigurationStatus, ProviderConfigurationInput, RedactedModel,
    RedactedModelCatalog, RedactedProvider, RedactedReasoningVariant, wire_word,
};

use super::*;
use crate::provider::contract::ProviderProtocolContract;
use crate::provider::policy::TurnRetryPolicy;

/// 一次 turn 的不可变模型配置快照：逐回合冻结 selector、声明协议、能力合同与重试策略。
/// 设置变更只产生未来回合的新快照，绝不改写活动快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelConfigurationSnapshot {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_variant: Option<String>,
    pub protocol: ProviderApiProtocol,
    pub capabilities: ProviderProtocolContract,
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

/// 不可变、含密钥的 provider 配置及其白名单模型选择。此类型不实现 Debug。
#[derive(Clone)]
pub(crate) struct ModelSelectionSnapshot {
    pub(crate) default_model: String,
    pub(crate) providers: BTreeMap<String, ConfiguredProvider>,
}

/// 服务级模型选择快照。只捕获一次，使默认选择与按 selector 解析共享同一事实源。
#[derive(Clone)]
pub struct ProviderConfigSnapshot {
    selection: Result<std::sync::Arc<ModelSelectionSnapshot>, ProviderError>,
    runtime_handle: tokio::runtime::Handle,
}

impl ProviderConfigSnapshot {
    /// 读取用户配置目录（config.json + auth.json）并固定一份 provider
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
                parse_user_model_selection(&user_config).map(std::sync::Arc::new)
            }
            Ok(None) => Err(missing_provider_config_error(crate::USER_CONFIG_FILE_NAME)),
        };
        Self {
            selection,
            runtime_handle,
        }
    }

    /// 测试接缝：从指定用户配置目录捕获快照，不读进程环境。生产路径一律经
    /// Self::capture 解析 SINGULARITY_HOME。
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

    /// 返回用户配置目录解析出的默认 selector（provider/model#effort）；
    /// provider 未配置或无法解析时返回 None（调用方保留 Thread.model 为 NULL）。
    pub fn resolved_default_selector(&self) -> Option<String> {
        let selection = self.selection.as_ref().ok()?;
        let (config, model) = resolve_model_selection(selection, None).ok()?;
        Some(compose_model_selector(
            &config.provider_name,
            &model.model_name,
            model.reasoning_variant.as_deref(),
        ))
    }

    /// 对照此不可变快照解析持久化的 provider/model[#variant] 引用；返回的
    /// 执行客户端带裸 model id 与恰好一个目录声明的协议。turn 的
    /// ModelConfigurationSnapshot 由该 provider 实例自身派生。
    pub fn provider_for_selector(
        &self,
        selector: Option<&str>,
    ) -> Result<OpenAiProvider, ProviderError> {
        let selection = self.selection.as_ref().map_err(Clone::clone)?;
        let (config, model) = resolve_model_selection(selection, selector)?;
        OpenAiProvider::new(config.clone(), model, self.runtime_handle.clone())
    }

    /// Validate a selector against the frozen configuration without constructing a client.
    pub fn validate_selector(&self, selector: Option<&str>) -> Result<(), ProviderError> {
        let selection = self.selection.as_ref().map_err(Clone::clone)?;
        resolve_model_selection(selection, selector).map(|_| ())
    }
}

pub struct ModelConfigOwner {
    directory: PathBuf,
    runtime_handle: tokio::runtime::Handle,
}

impl ModelConfigOwner {
    /// Remove a provider from future model selection. Running turns retain their snapshot.
    pub fn remove_provider(
        &mut self,
        provider_id: &str,
    ) -> Result<RedactedModelCatalog, ProviderError> {
        let mut data = read_user_config_data_from_directory(self.directory.clone())?
            .ok_or_else(|| user_config_error("provider configuration is missing"))?;
        let removed = data.config.providers.remove(provider_id).is_some();
        if !removed && !data.auth.providers.contains_key(provider_id) {
            return Err(user_config_error("provider does not exist"));
        }
        if removed {
            repair_default_selection(&mut data.config);
            write_json_file(&self.directory, crate::USER_CONFIG_FILE_NAME, &data.config)?;
        }
        // Credentials are removed only after the provider is no longer selectable.
        // Retrying a partial removal finishes the remaining credential write.
        if data.auth.providers.remove(provider_id).is_some() {
            write_json_file(&self.directory, crate::USER_AUTH_FILE_NAME, &data.auth).map_err(
                |mut error| {
                    error.message = format!(
                        "提供方配置已删除，但 API 密钥删除失败；请重试删除：{}",
                        error.message
                    );
                    error
                },
            )?;
        }
        Ok(catalog_from_data(&data))
    }

    /// Build a read-only listing request from the editor values; secrets never leave the host response.
    pub fn model_discovery_request(
        &self,
        provider_id: &str,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        validate_base_url(base_url)?;
        let key = match api_key.filter(|key| !key.is_empty()) {
            Some(key) => {
                validate_provider_value(key, "api_key")?;
                key.to_string()
            }
            None => read_user_config_data_from_directory(self.directory.clone())?
                .and_then(|data| {
                    data.auth
                        .providers
                        .get(provider_id)
                        .map(|auth| auth.api_key.clone())
                })
                .unwrap_or_default(),
        };
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| user_config_error("模型查询客户端无法启动。"))?;
        let request = client.get(format!("{}/models", base_url.trim_end_matches('/')));
        Ok(if key.is_empty() {
            request
        } else {
            request.bearer_auth(key)
        })
    }

    /// Query model metadata without modifying configuration or credentials.
    pub async fn discover_models(
        request: reqwest::RequestBuilder,
        base_url: &str,
    ) -> Result<Vec<singularity_protocol::DiscoveredModel>, ProviderError> {
        super::discovery::discover(request, base_url).await
    }

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
            Ok(Some(data)) => catalog_from_data(&data),
            Ok(None) => RedactedModelCatalog {
                configuration: ModelConfigurationStatus::Missing,
                message: Some("配置一个模型提供方后即可开始新任务。".to_string()),
                default_selector: None,
                providers: Vec::new(),
                presets: crate::catalog::provider_presets(),
            },
            Err(error) => RedactedModelCatalog {
                configuration: ModelConfigurationStatus::Invalid,
                message: Some(error.to_string()),
                default_selector: None,
                providers: Vec::new(),
                presets: crate::catalog::provider_presets(),
            },
        }
    }

    pub fn save_provider(
        &mut self,
        input: ProviderConfigurationInput,
    ) -> Result<RedactedModelCatalog, ProviderError> {
        validate_identifier(&input.provider_id, "provider id")?;
        validate_base_url(&input.base_url)?;
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
            let api_protocol = wire_word(model.api_protocol);
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
            let previous = previous_models
                .get(&model.model_id)
                .cloned()
                .unwrap_or_default();
            let configured = UserConfigModel {
                display_name: model.display_name.filter(|name| !name.trim().is_empty()),
                api_protocol: Some(api_protocol),
                max_context_tokens: model.max_context_tokens,
                max_output_tokens: model.max_output_tokens,
                reasoning_variants: variants,
                default_variant: model.default_variant,
                _legacy_tool_reasoning_history: None,
                supports_developer_role: previous.supports_developer_role,
                supports_tool_choice: previous.supports_tool_choice,
                requires_reasoning_content_for_tool_calls: previous
                    .requires_reasoning_content_for_tool_calls,
                requires_assistant_content_for_tool_calls: previous
                    .requires_assistant_content_for_tool_calls,
                thinking_wire_format: model.thinking_wire_format,
            };
            configured_model_from_user_file(&configured, &input.provider_id, &model.model_id)?;
            models.insert(model.model_id, configured);
        }
        let first_model = models.keys().next().cloned();
        config.providers.insert(
            input.provider_id.clone(),
            UserConfigProvider {
                display_name: input.display_name.filter(|name| !name.trim().is_empty()),
                base_url: input.base_url,
                models,
            },
        );
        if config.default_provider.is_none()
            && let Some(first_model) = first_model
        {
            config.default_provider = Some(input.provider_id.clone());
            config.default_model = Some(compose_model_selector(
                &input.provider_id,
                &first_model,
                None,
            ));
        }
        repair_default_selection(&mut config);
        write_json_file(&self.directory, crate::USER_CONFIG_FILE_NAME, &config)?;
        Ok(catalog_from_data(&UserConfigData { config, auth }))
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
        write_json_file(&self.directory, crate::USER_AUTH_FILE_NAME, &auth)?;
        Ok(CredentialConfigured {
            provider_id: provider_id.to_string(),
            credential_configured: true,
        })
    }
}

// Keep the selected model when an edit removes its explicit reasoning variant.
// Only choose another model when the previous model itself no longer exists.
fn repair_default_selection(config: &mut UserConfigFile) {
    let current = config.default_model.as_deref().and_then(|selector| {
        let selected = parse_model_selector(selector).ok()?;
        let model = config
            .providers
            .get(selected.provider_name)?
            .models
            .get(selected.model_name)?;
        let effort = selected.reasoning_effort.filter(|effort| {
            model
                .reasoning_variants
                .get(*effort)
                .is_some_and(|variant| variant.enabled || *effort == "off")
        });
        Some((
            selected.provider_name.to_string(),
            compose_model_selector(selected.provider_name, selected.model_name, effort),
        ))
    });
    let next = current.or_else(|| {
        config.providers.iter().find_map(|(id, provider)| {
            provider
                .models
                .keys()
                .next()
                .map(|model| (id.clone(), compose_model_selector(id, model, None)))
        })
    });
    config.default_provider = next.as_ref().map(|(id, _)| id.clone());
    config.default_model = next.map(|(_, selector)| selector);
}

fn catalog_from_data(data: &UserConfigData) -> RedactedModelCatalog {
    if data.config.providers.is_empty() {
        return RedactedModelCatalog {
            configuration: ModelConfigurationStatus::Missing,
            message: Some("添加一个模型提供方即可开始。".to_string()),
            default_selector: None,
            providers: Vec::new(),
            presets: crate::catalog::provider_presets(),
        };
    }
    let selection = parse_user_model_selection(data);
    let (configuration, message, default_selector) = match selection {
        _ if data
            .config
            .providers
            .values()
            .all(|provider| provider.models.is_empty()) =>
        {
            (
                ModelConfigurationStatus::Missing,
                Some("为提供方添加一个模型后即可开始。".to_string()),
                None,
            )
        }
        Ok(selection) => (
            ModelConfigurationStatus::Ready,
            None,
            Some(selection.default_model),
        ),
        Err(error) if error.kind == crate::ModelErrorKind::AuthError => (
            ModelConfigurationStatus::Missing,
            Some(error.to_string()),
            data.config.default_model.clone(),
        ),
        Err(error) => (
            ModelConfigurationStatus::Invalid,
            Some(error.to_string()),
            data.config.default_model.clone(),
        ),
    };
    let providers = data
        .config
        .providers
        .iter()
        .map(|(provider_id, provider)| RedactedProvider {
            provider_id: provider_id.clone(),
            display_name: provider.display_name.clone(),
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
                    display_name: model.display_name.clone(),
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
                    thinking_wire_format: model.thinking_wire_format.clone(),
                })
                .collect(),
        })
        .collect();
    RedactedModelCatalog {
        configuration,
        message,
        default_selector,
        providers,
        presets: crate::catalog::provider_presets(),
    }
}

fn write_json_file(
    directory: &Path,
    file_name: &str,
    value: &impl Serialize,
) -> Result<(), ProviderError> {
    singularity_core::create_data_dir(directory).map_err(user_config_error)?;
    let path = directory.join(file_name);
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        user_config_error(format!(
            "user provider config could not be serialized: {error}"
        ))
    })?;
    bytes.push(b'\n');
    singularity_core::atomic_replace_bytes(&path, &bytes)
        .map_err(|error| user_config_error(format!("could not update {}: {error}", path.display())))
}
