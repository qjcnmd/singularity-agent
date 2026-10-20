//! 用户配置的保存、脱敏目录和执行快照。文件读写在 user，模型解析在 selection。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use singularity_protocol::{
    ModelConfigurationInput, ProviderConfigurationInput, ReasoningVariant, RedactedModelCatalog,
    RedactedProvider,
};

use super::*;

/// 一次轮次的不可变容量快照：每轮开始时冻结「请求前压缩」和「输出预算」要用的两项容量，
/// 改设置只产生后续轮次的新快照，不改写正在用的快照。模型身份和协议由 SelectedModel、
/// OpenAiProviderConfig 和请求记录承载。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelConfigurationSnapshot {
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
}

impl ModelConfigurationSnapshot {
    /// 请求前压缩判定所用的上下文窗口（已解析值）。
    pub fn context_window(&self) -> u64 {
        u64::from(self.max_context_tokens)
    }
}

/// 服务级配置快照：一次读取就冻结配置和密钥，之后按实际 selector 解析；它只保存配置事实，
/// 不持有 Tokio handle，也不创建任何网络执行对象，因此不实现 Debug。
pub struct ProviderConfigSnapshot {
    data: Result<Option<UserConfigFile>, ProviderError>,
}

impl ProviderConfigSnapshot {
    fn read(directory: &Path) -> Self {
        Self { data: read_user_config_file(directory) }
    }

    fn redacted_catalog(&self) -> RedactedModelCatalog {
        match &self.data {
            Ok(Some(data)) => {
                let selection = match data.default_model.as_deref() {
                    Some(selector) => self.validate_selector(Some(selector)),
                    None => Ok(()),
                };
                catalog_from_data(data, selection)
            }
            Ok(None) => empty_catalog(None),
            Err(error) => empty_catalog(Some(error.to_string())),
        }
    }

    pub(crate) fn config(&self) -> Result<&UserConfigFile, ProviderError> {
        self.data
            .as_ref()
            .map_err(Clone::clone)?
            .as_ref()
            .ok_or_else(|| missing_provider_config_error(crate::USER_CONFIG_FILE_NAME))
    }

    /// 返回从用户配置解析出的默认 selector（provider/model#variant）；
    /// 提供方未配置或解析不了时返回 None（调用方把 Thread.model 保持为 NULL）。
    pub fn resolved_default_selector(&self) -> Option<String> {
        let (config, model) = self.resolve(None).ok()?;
        Some(compose_model_selector(
            &config.provider_name,
            &model.model_name,
            model.reasoning_variant.as_deref(),
        ))
    }

    /// 用这份不可变快照解析持久化的 provider/model[#variant] 引用；返回的连接
    /// 设置和已解析的选择交给具体 Provider 的构造入口使用。
    pub(crate) fn resolve(
        &self,
        selector: Option<&str>,
    ) -> Result<(OpenAiProviderConfig, SelectedModel), ProviderError> {
        resolve_model_selection(self.config()?, selector)
    }

    /// 用冻结的配置校验 selector，不构造 client。
    pub fn validate_selector(&self, selector: Option<&str>) -> Result<(), ProviderError> {
        self.resolve(selector).map(|_| ())
    }
}

/// 数据目录内的模型配置与凭据入口；每次读取与修改在内部串行化。
pub struct ModelConfigManager {
    directory: PathBuf,
    access: Mutex<()>,
}

impl ModelConfigManager {
    /// 把提供方从后续的模型选择里移除。正在跑的轮次仍用它自己的快照。
    pub fn remove_provider(&self, provider_id: &str) -> Result<(), ProviderError> {
        let _access = self.lock();
        let mut data =
            read_user_config_file(&self.directory)?.ok_or_else(|| user_config_error("尚未配置模型提供方"))?;
        if data.providers.remove(provider_id).is_none() {
            return Err(user_config_error("模型提供方不存在"));
        }
        clear_invalid_default_selection(&mut data);
        write_user_config_file(&self.directory, &data)
    }

    /// 决定这次模型发现查询用哪份凭据：本次显式提交的密钥优先，否则回退到该
    /// 提供方已存的密钥。查询输入和请求构造由发现实现自己完成。
    pub fn discovery_credential(
        &self,
        provider_id: &str,
        api_key: Option<&str>,
    ) -> Result<String, ProviderError> {
        let _access = self.lock();
        match api_key {
            Some(key) => {
                validate_provider_value(key, "api_key")?;
                Ok(key.to_string())
            }
            None => Ok(read_user_config_file(&self.directory)?
                .and_then(|config| config.providers.get(provider_id).map(|provider| provider.api_key.clone()))
                .unwrap_or_default()),
        }
    }

    /// 绑定数据目录；构造时不读取或创建配置文件。
    pub fn open(directory: PathBuf) -> Self {
        Self { directory, access: Mutex::new(()) }
    }

    /// 冻结当前配置与凭据；读取失败保留在快照中，解析选择时返回原错误。
    pub fn snapshot(&self) -> ProviderConfigSnapshot {
        let _access = self.lock();
        ProviderConfigSnapshot::read(&self.directory)
    }

    /// 用同一次读取的结果生成脱敏目录，不缓存磁盘上的配置。
    pub fn redacted_catalog(&self) -> RedactedModelCatalog {
        self.snapshot().redacted_catalog()
    }

    /// 保存提供方配置，需要时同时替换密钥；密钥省略表示保留原值。
    /// 配置和密钥一起原子提交，失败时保留原文件。
    pub fn save_provider(
        &self,
        input: ProviderConfigurationInput,
        api_key: Option<&str>,
    ) -> Result<(), ProviderError> {
        let _access = self.lock();
        validate_identifier(&input.provider_id, "提供方标识")?;
        if let Some(key) = api_key {
            validate_provider_value(key, "api_key")?;
        }
        // 这里只规范输入形状（去掉空白和结尾斜杠）：地址怎么解释统一交给
        // openai::wire，已写明的端点原样保留，不给自定义前缀拼出错误路由。
        let base_url = crate::openai::canonical_base_url(&input.base_url).to_string();
        validate_base_url(&base_url)?;
        let mut config = read_user_config_file(&self.directory)?.unwrap_or_default();
        // 先只构造模型映射，任何一项校验失败都发生在写配置和凭据之前。
        let models = model_definitions(input.models, input.api_protocol)?;
        let key = api_key
            .map(str::to_string)
            .or_else(|| config.providers.get(&input.provider_id).map(|provider| provider.api_key.clone()))
            .unwrap_or_default();
        config.providers.insert(
            input.provider_id,
            UserConfigProvider {
                api_key: key,
                api_protocol: input.api_protocol,
                display_name: input.display_name.filter(|name| !name.trim().is_empty()),
                base_url,
                models,
            },
        );
        clear_invalid_default_selection(&mut config);
        write_user_config_file(&self.directory, &config)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.access.lock().expect("model configuration lock poisoned")
    }
}

/// 把本次提交的模型输入转成要落盘的模型映射：只做纯转换，不读写配置和凭据；模型 id、容量、
/// 档位和重复项的校验都在返回前完成，调用方拿到完整映射后才提交，失败不会留下写了一半的结果。
///
/// 已保存但未在表单中展示的能力标记按模型 id 保留；已移除的模型不参与转换。
fn model_definitions(
    models: Vec<ModelConfigurationInput>,
    provider_protocol: ProviderApiProtocol,
) -> Result<BTreeMap<String, UserConfigModel>, ProviderError> {
    let mut definitions = BTreeMap::new();
    for model in models {
        validate_model_id(&model.model_id, "模型标识")?;
        if definitions.contains_key(&model.model_id) {
            return Err(user_config_error("同一提供方的模型标识不能重复"));
        }
        let (model_id, configured) = model_definition(model, provider_protocol)?;
        definitions.insert(model_id, configured);
    }
    Ok(definitions)
}

/// 转换单个模型并检查其完整配置；保留表单没有展示的能力，返回后才允许写入文件。
fn model_definition(
    mut model: ModelConfigurationInput,
    provider_protocol: ProviderApiProtocol,
) -> Result<(String, UserConfigModel), ProviderError> {
    if provider_protocol == ProviderApiProtocol::Responses {
        model.thinking_wire_format = None;
        model.chat_output_tokens_field = None;
        model.requires_reasoning_content_for_tool_calls = None;
    }
    let declared_variants = model.reasoning_variants.is_some();
    let mut variants = BTreeMap::new();
    for variant in model.reasoning_variants.into_iter().flatten() {
        if variants
            .insert(variant.id, ModelsFileReasoningVariant { wire_effort: variant.wire_effort })
            .is_some()
        {
            return Err(user_config_error("推理档位标识不能重复"));
        }
    }
    let configured = UserConfigModel {
        max_context_tokens: model.max_context_tokens,
        max_output_tokens: model.max_output_tokens,
        reasoning_variants: declared_variants.then_some(variants),
        default_variant: model.default_variant,
        requires_reasoning_content_for_tool_calls: model.requires_reasoning_content_for_tool_calls,
        chat_output_tokens_field: model.chat_output_tokens_field.filter(|field| !field.is_empty()),
        thinking_wire_format: model.thinking_wire_format,
    };
    resolve_model_definition(&configured, provider_protocol, &model.model_id, None)?;
    Ok((model.model_id, configured))
}

// 已指定的模型或显式档位被移除后清空默认选择，由用户重新选择。
fn clear_invalid_default_selection(config: &mut UserConfigFile) {
    let valid = config.default_model.as_deref().is_none_or(|selector| {
        let Ok(selected) = parse_model_selector(selector) else {
            return false;
        };
        let Some(model) = config
            .providers
            .get(selected.provider_name)
            .and_then(|provider| provider.models.get(selected.model_name))
        else {
            return false;
        };
        selected.reasoning_variant.is_none_or(|variant| {
            model.reasoning_variants.as_ref().is_some_and(|variants| variants.contains_key(variant))
        })
    });
    if !valid {
        config.default_model = None;
    }
}

fn empty_catalog(error: Option<String>) -> RedactedModelCatalog {
    RedactedModelCatalog {
        error,
        default_selector: None,
        providers: Vec::new(),
    }
}

fn catalog_from_data(data: &UserConfigFile, selection: Result<(), ProviderError>) -> RedactedModelCatalog {
    if data.providers.is_empty() {
        return empty_catalog(None);
    }
    let (error, default_selector) = match selection {
        _ if data.providers.values().all(|provider| provider.models.is_empty()) => (None, None),
        Ok(()) => (None, data.default_model.clone()),
        Err(error) => (Some(error.to_string()), data.default_model.clone()),
    };
    let providers = data
        .providers
        .iter()
        .map(|(provider_id, provider)| RedactedProvider {
            api_protocol: provider.api_protocol,
            provider_id: provider_id.clone(),
            display_name: provider.display_name.clone(),
            base_url: provider.base_url.clone(),
            credential_configured: !provider.api_key.is_empty(),
            models: provider
                .models
                .iter()
                .map(|(model_id, model)| ModelConfigurationInput {
                    model_id: model_id.clone(),
                    max_context_tokens: model.max_context_tokens,
                    max_output_tokens: model.max_output_tokens,
                    reasoning_variants: model.reasoning_variants.as_ref().map(|variants| {
                        variants
                            .iter()
                            .map(|(id, variant)| ReasoningVariant {
                                id: id.clone(),
                                wire_effort: variant.wire_effort.clone(),
                            })
                            .collect()
                    }),
                    default_variant: model.default_variant.clone(),
                    thinking_wire_format: model.thinking_wire_format.clone(),
                    chat_output_tokens_field: model.chat_output_tokens_field.clone(),
                    requires_reasoning_content_for_tool_calls: model
                        .requires_reasoning_content_for_tool_calls,
                })
                .collect(),
        })
        .collect();
    RedactedModelCatalog { error, default_selector, providers }
}
