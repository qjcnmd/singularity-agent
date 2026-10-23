//! 模型配置的保存、隐藏密钥的目录，以及执行时解析模型选择。文件读写在 user，
//! 解析模型选择在 selection。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use singularity_protocol::{
    ModelConfigurationInput, ProviderConfigurationInput, RedactedModelCatalog, RedactedProvider,
};

use super::*;

/// 本轮模型的上下文窗口和最大输出，每轮开始时确定，用于压缩判断和输出预算。
/// 修改设置只影响之后的回合。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelConfigurationSnapshot {
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
}

impl ModelConfigurationSnapshot {
    /// 上下文窗口的 Token 数。
    pub fn context_window(&self) -> u64 {
        u64::from(self.max_context_tokens)
    }
}

/// 数据目录里 `config.json` 的读写入口；读取和修改按顺序进行，不缓存文件内容。
pub struct ModelConfigManager {
    directory: PathBuf,
    access: Mutex<()>,
}

impl ModelConfigManager {
    /// 读取一次配置并解析模型选择；返回值归本轮所有，之后修改配置不影响它。
    pub(crate) fn resolve(
        &self,
        selection: &ModelSelection,
    ) -> Result<(OpenAiProviderConfig, SelectedModel), ProviderError> {
        let _access = self.lock();
        let config = read_user_config_file(&self.directory)?
            .ok_or_else(|| missing_provider_config_error(crate::USER_CONFIG_FILE_NAME))?;
        resolve_model_selection(&config, selection)
    }

    /// 只检查模型选择能否解析，不创建 HTTP 客户端。
    pub fn validate_selection(&self, selection: &ModelSelection) -> Result<(), ProviderError> {
        self.resolve(selection).map(|_| ())
    }
    /// 移除提供方，返回保存后的目录（不含密钥）。正在运行的回合不受影响。
    pub fn remove_provider(&self, provider_id: &str) -> Result<RedactedModelCatalog, ProviderError> {
        let _access = self.lock();
        let mut data = read_user_config_file(&self.directory)?
            .ok_or_else(|| configuration_error("尚未配置模型提供方"))?;
        if data.providers.remove(provider_id).is_none() {
            return Err(configuration_error("模型提供方不存在"));
        }
        clear_invalid_default_selection(&mut data);
        write_user_config_file(&self.directory, &data)?;
        Ok(catalog_from_data(&data))
    }

    /// 模型发现用的密钥：优先用这次表单里填的，没有就用该提供方已保存的。
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

    /// 读取配置并生成不含密钥的目录。
    pub fn redacted_catalog(&self) -> RedactedModelCatalog {
        let _access = self.lock();
        match read_user_config_file(&self.directory) {
            Ok(Some(data)) => catalog_from_data(&data),
            Ok(None) => empty_catalog(None),
            Err(error) => empty_catalog(Some(error.to_string())),
        }
    }

    /// 保存提供方配置；传了密钥就替换，没传保留原值。
    /// 配置和密钥一次原子写入，成功后返回保存后的目录（不含密钥）。
    pub fn save_provider(
        &self,
        input: ProviderConfigurationInput,
        api_key: Option<&str>,
    ) -> Result<RedactedModelCatalog, ProviderError> {
        let _access = self.lock();
        validate_identifier(&input.provider_id, "提供方标识")?;
        if let Some(key) = api_key {
            validate_provider_value(key, "api_key")?;
        }
        // API 根只去掉首尾空白和结尾斜杠；保留完整路径，再追加各协议的端点。
        let base_url = crate::openai::canonical_base_url(&input.base_url).to_string();
        validate_base_url(&base_url)?;
        let mut config = read_user_config_file(&self.directory)?.unwrap_or_default();
        // 先检查全部模型，任何一项不通过都在写文件之前返回。
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
        write_user_config_file(&self.directory, &config)?;
        Ok(catalog_from_data(&config))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.access.lock().expect("model configuration lock poisoned")
    }
}

/// 把表单提交的模型列表转换成要保存的配置，同时检查模型 ID、容量、档位和重复项；
/// 不读写文件，调用方拿到完整结果后才写入。
fn model_definitions(
    models: Vec<ModelConfigurationInput>,
    provider_protocol: ProviderApiProtocol,
) -> Result<BTreeMap<String, UserConfigModel>, ProviderError> {
    let mut definitions = BTreeMap::new();
    for model in models {
        validate_identifier(&model.model_id, "模型标识")?;
        if definitions.contains_key(&model.model_id) {
            return Err(configuration_error("同一提供方的模型标识不能重复"));
        }
        let (model_id, configured) = model_definition(model, provider_protocol)?;
        definitions.insert(model_id, configured);
    }
    Ok(definitions)
}

/// 转换单个模型并检查配置是否完整可用；Responses 协议不使用的 Chat 字段会被清空。
fn model_definition(
    mut model: ModelConfigurationInput,
    provider_protocol: ProviderApiProtocol,
) -> Result<(String, UserConfigModel), ProviderError> {
    if provider_protocol == ProviderApiProtocol::Responses {
        model.thinking_wire_format = None;
        model.chat_output_tokens_field = None;
    }
    let configured = UserConfigModel {
        max_context_tokens: model
            .max_context_tokens
            .ok_or_else(|| configuration_error("请填写上下文窗口。"))?,
        max_output_tokens: model
            .max_output_tokens
            .ok_or_else(|| configuration_error("请填写最大输出 Token。"))?,
        reasoning_variants: model.reasoning_variants,
        chat_output_tokens_field: model.chat_output_tokens_field.filter(|field| !field.is_empty()),
        thinking_wire_format: model.thinking_wire_format,
    };
    resolve_model_definition(&configured, provider_protocol, &model.model_id, None)?;
    Ok((model.model_id, configured))
}

// 已指定的模型或显式档位被移除后清空默认选择，由用户重新选择。
fn clear_invalid_default_selection(config: &mut UserConfigFile) {
    let valid = config.default_model.as_ref().is_none_or(|selected| {
        let Some(model) = config
            .providers
            .get(&selected.provider)
            .and_then(|provider| provider.models.get(&selected.model))
        else {
            return false;
        };
        selected.reasoning.as_ref().is_none_or(|variant| {
            model.reasoning_variants.as_ref().is_some_and(|variants| variants.contains(variant))
        })
    });
    if !valid {
        config.default_model = None;
    }
}

fn empty_catalog(error: Option<String>) -> RedactedModelCatalog {
    RedactedModelCatalog {
        error,
        default_selection: None,
        providers: Vec::new(),
    }
}

fn catalog_from_data(data: &UserConfigFile) -> RedactedModelCatalog {
    if data.providers.is_empty() {
        return empty_catalog(None);
    }
    let selection = match data.default_model.as_ref() {
        Some(selection) => resolve_model_selection(data, selection).map(|_| ()),
        None => Ok(()),
    };
    let (error, default_selection) = match selection {
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
                    max_context_tokens: Some(model.max_context_tokens),
                    max_output_tokens: Some(model.max_output_tokens),
                    reasoning_variants: model.reasoning_variants.clone(),
                    thinking_wire_format: model.thinking_wire_format.clone(),
                    chat_output_tokens_field: model.chat_output_tokens_field.clone(),
                })
                .collect(),
        })
        .collect();
    RedactedModelCatalog { error, default_selection, providers }
}
