use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use protocol::{ModelConfigurationInput, ProviderConfigurationInput, RedactedModelCatalog, RedactedProvider};

use super::{
    ModelSelection, ProviderApiProtocol, ProviderConfig, ProviderError, SelectedModel, UserConfigFile,
    UserConfigModel, UserConfigProvider, configuration_error, read_user_config_file,
    resolve_model_definition, resolve_model_selection, validate_api_key, validate_base_url,
    validate_identifier, write_user_config_file,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelConfigurationSnapshot {
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
}

impl ModelConfigurationSnapshot {
    pub fn context_window(&self) -> u64 {
        u64::from(self.max_context_tokens)
    }
}

#[derive(Debug)]
pub struct ModelConfigManager {
    directory: PathBuf,
    access: Mutex<()>,
}

impl ModelConfigManager {
    pub(crate) fn resolve(
        &self,
        selection: &ModelSelection,
    ) -> Result<(ProviderConfig, SelectedModel), ProviderError> {
        let _access = self.lock();
        let config = read_user_config_file(&self.directory)?.ok_or_else(|| {
            configuration_error(format!("缺少提供方配置：{}", crate::USER_CONFIG_FILE_NAME))
        })?;
        resolve_model_selection(&config, selection)
    }

    pub fn validate_selection(&self, selection: &ModelSelection) -> Result<u64, ProviderError> {
        self.resolve(selection).map(|(_, selected)| u64::from(selected.max_context_tokens))
    }

    pub fn context_window(&self, selection: &ModelSelection) -> Option<u64> {
        let _access = self.lock();
        let config = read_user_config_file(&self.directory).ok()??;
        let model = config.providers.get(&selection.provider)?.models.get(&selection.model)?;
        (model.max_context_tokens > 0).then(|| u64::from(model.max_context_tokens))
    }
    pub fn remove_provider(&self, provider_id: &str) -> Result<RedactedModelCatalog, ProviderError> {
        let _access = self.lock();
        let mut data = read_user_config_file(&self.directory)?
            .ok_or_else(|| configuration_error("尚未配置模型提供方"))?;
        if data.providers.remove(provider_id).is_none() {
            return Err(configuration_error(format!("模型提供方不存在：{provider_id}")));
        }
        clear_invalid_default_selection(&mut data);
        write_user_config_file(&self.directory, &data)?;
        Ok(catalog_from_data(&data))
    }

    pub fn discovery_credential(
        &self,
        provider_id: &str,
        api_key: Option<&str>,
    ) -> Result<String, ProviderError> {
        let _access = self.lock();
        let key = match api_key {
            Some(key) => key.to_string(),
            None => read_user_config_file(&self.directory)?
                .and_then(|config| config.providers.get(provider_id).map(|provider| provider.api_key.clone()))
                .unwrap_or_default(),
        };
        validate_api_key(&key)?;
        Ok(key)
    }

    pub fn open(directory: PathBuf) -> Self {
        Self { directory, access: Mutex::new(()) }
    }

    pub fn redacted_catalog(&self) -> RedactedModelCatalog {
        let _access = self.lock();
        match read_user_config_file(&self.directory) {
            Ok(Some(data)) => catalog_from_data(&data),
            Ok(None) => empty_catalog(None),
            Err(error) => empty_catalog(Some(error.to_string())),
        }
    }

    pub fn save_provider(
        &self,
        input: ProviderConfigurationInput,
        api_key: Option<&str>,
    ) -> Result<RedactedModelCatalog, ProviderError> {
        let _access = self.lock();
        validate_identifier(&input.provider_id, "提供方标识")?;
        let base_url = crate::provider::canonical_base_url(&input.base_url).to_string();
        validate_base_url(&base_url)?;
        let mut config = read_user_config_file(&self.directory)?.unwrap_or_default();
        let models = model_definitions(input.models, input.api_protocol)?;
        let key = api_key
            .map(str::to_string)
            .or_else(|| config.providers.get(&input.provider_id).map(|provider| provider.api_key.clone()))
            .unwrap_or_default();
        validate_api_key(&key)?;
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

fn model_definitions(
    models: Vec<ModelConfigurationInput>,
    provider_protocol: ProviderApiProtocol,
) -> Result<BTreeMap<String, UserConfigModel>, ProviderError> {
    let mut definitions = BTreeMap::new();
    for model in models {
        validate_identifier(&model.model_id, "模型标识")?;
        if definitions.contains_key(&model.model_id) {
            return Err(configuration_error(format!("同一提供方的模型标识不能重复：{}", model.model_id)));
        }
        let (model_id, configured) = model_definition(model, provider_protocol)?;
        definitions.insert(model_id, configured);
    }
    Ok(definitions)
}

fn model_definition(
    model: ModelConfigurationInput,
    provider_protocol: ProviderApiProtocol,
) -> Result<(String, UserConfigModel), ProviderError> {
    let configured = UserConfigModel {
        max_context_tokens: model
            .max_context_tokens
            .ok_or_else(|| configuration_error(format!("模型 {} 请填写上下文窗口。", model.model_id)))?,
        max_output_tokens: model
            .max_output_tokens
            .ok_or_else(|| configuration_error(format!("模型 {} 请填写最大输出 Token。", model.model_id)))?,
        reasoning_variants: model.reasoning_variants,
    };
    resolve_model_definition(&configured, provider_protocol, &model.model_id, None)?;
    Ok((model.model_id, configured))
}

fn clear_invalid_default_selection(config: &mut UserConfigFile) {
    if config
        .default_model
        .as_ref()
        .is_some_and(|selected| resolve_model_selection(config, selected).is_err())
    {
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
                })
                .collect(),
        })
        .collect();
    RedactedModelCatalog { error, default_selection, providers }
}
