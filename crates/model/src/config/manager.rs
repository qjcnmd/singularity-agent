use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use protocol::{
    ModelConfigurationInput, ModelSelection, ProviderApiProtocol, ProviderConfigurationInput,
    RedactedModelCatalog, RedactedProvider, RedactedWebSearchConfig,
};

use super::selection::{ProviderConfig, SelectedModel, resolve_model_definition, resolve_model_selection};
use super::user::{
    UserConfigFile, UserConfigModel, UserConfigProvider, UserConfigWebSearch, read_user_config_file,
    write_user_config_file,
};
use super::{configuration_error, validate_api_key, validate_base_url, validate_identifier};
use crate::error::ProviderError;
use crate::web::{DEFAULT_WEB_SEARCH_BASE_URL, WebSearchAccess};

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

    pub async fn discover_models(
        self: &Arc<Self>,
        provider_id: &str,
        api_protocol: ProviderApiProtocol,
        base_url: &str,
        api_key: Option<&str>,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<Vec<String>, ProviderError> {
        if cancellation.is_cancelled() {
            return Err(crate::transport::http::provider_cancelled_error());
        }
        let normalized_url = crate::provider::canonical_base_url(base_url);
        let target_url = validate_base_url(normalized_url)?;
        let api_key = match api_key {
            Some(key) => resolve_api_key(Some(key), None, &target_url, PROVIDER_ORIGIN_CHANGE_ERROR)?,
            None => {
                let manager = Arc::clone(self);
                let provider_id = provider_id.to_owned();
                tokio::task::spawn_blocking(move || {
                    let _access = manager.lock();
                    let existing = read_user_config_file(&manager.directory)?
                        .and_then(|mut config| config.providers.remove(&provider_id));
                    resolve_api_key(
                        None,
                        existing.as_ref().map(|p| (p.base_url.as_str(), p.api_key.as_str())),
                        &target_url,
                        PROVIDER_ORIGIN_CHANGE_ERROR,
                    )
                })
                .await
                .expect("model credential lookup completes while the backend is running")?
            }
        };
        super::discovery::discover(normalized_url, &api_key, api_protocol, cancellation).await
    }

    pub fn open(directory: PathBuf) -> Self {
        Self { directory, access: Mutex::new(()) }
    }

    pub fn redacted_catalog(&self) -> RedactedModelCatalog {
        let _access = self.lock();
        match read_user_config_file(&self.directory) {
            Ok(Some(data)) => catalog_from_data(&data),
            Ok(None) => empty_catalog(None, redacted_web_search(None)),
            Err(error) => empty_catalog(Some(error.to_string()), redacted_web_search(None)),
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
        let target_url = validate_base_url(&base_url)?;
        let mut config = read_user_config_file(&self.directory)?.unwrap_or_default();
        let models = model_definitions(input.models, input.api_protocol)?;
        let key = resolve_api_key(
            api_key,
            config
                .providers
                .get(&input.provider_id)
                .map(|p| (p.base_url.as_str(), p.api_key.as_str())),
            &target_url,
            PROVIDER_ORIGIN_CHANGE_ERROR,
        )?;
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

    pub fn save_web_search(
        &self,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Result<RedactedModelCatalog, ProviderError> {
        let _access = self.lock();
        let normalized_url = crate::provider::canonical_base_url(web_search_base_url(base_url)).to_string();
        let target_url = validate_base_url(&normalized_url)?;
        let mut config = read_user_config_file(&self.directory)?.unwrap_or_default();
        let key = resolve_api_key(
            api_key,
            config.web_search.as_ref().map(|w| (web_search_base_url(&w.base_url), w.api_key.as_str())),
            &target_url,
            "更改联网搜索 API 地址时，请重新填写或清除 API 密钥。",
        )?;
        config.web_search = Some(UserConfigWebSearch { base_url: normalized_url, api_key: key });
        write_user_config_file(&self.directory, &config)?;
        Ok(catalog_from_data(&config))
    }

    pub fn web_search_access(&self) -> Option<WebSearchAccess> {
        let _access = self.lock();
        let web = read_user_config_file(&self.directory).ok()??.web_search?;
        if web.api_key.is_empty() || validate_api_key(&web.api_key).is_err() {
            return None;
        }
        let base_url = crate::provider::canonical_base_url(web_search_base_url(&web.base_url)).to_string();
        validate_base_url(&base_url).ok()?;
        Some(WebSearchAccess::new(base_url, web.api_key))
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

const PROVIDER_ORIGIN_CHANGE_ERROR: &str = "更改提供方 API 地址时，请重新填写 API 密钥。";

fn resolve_api_key(
    api_key: Option<&str>,
    existing: Option<(&str, &str)>,
    target_url: &reqwest::Url,
    origin_change_error: &str,
) -> Result<String, ProviderError> {
    let key = match api_key {
        Some(value) => value.to_string(),
        None => match existing.filter(|(_, key)| !key.is_empty()) {
            Some((existing_url, existing_key)) => {
                if !same_url_origin(existing_url, target_url) {
                    return Err(configuration_error(origin_change_error));
                }
                existing_key.to_string()
            }
            None => String::new(),
        },
    };
    validate_api_key(&key)?;
    Ok(key)
}

fn web_search_base_url(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        DEFAULT_WEB_SEARCH_BASE_URL
    } else {
        trimmed
    }
}

fn same_url_origin(existing_url: &str, target_url: &reqwest::Url) -> bool {
    reqwest::Url::parse(existing_url).is_ok_and(|existing| existing.origin() == target_url.origin())
}

fn redacted_web_search(web: Option<&UserConfigWebSearch>) -> RedactedWebSearchConfig {
    RedactedWebSearchConfig {
        base_url: web_search_base_url(web.map_or("", |w| w.base_url.as_str())).to_string(),
        credential_configured: web.is_some_and(|w| !w.api_key.is_empty()),
    }
}

fn empty_catalog(error: Option<String>, web_search: RedactedWebSearchConfig) -> RedactedModelCatalog {
    RedactedModelCatalog {
        error,
        default_selection: None,
        providers: Vec::new(),
        web_search,
    }
}

fn catalog_from_data(data: &UserConfigFile) -> RedactedModelCatalog {
    let web_search = redacted_web_search(data.web_search.as_ref());
    if data.providers.is_empty() {
        return empty_catalog(None, web_search);
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
    RedactedModelCatalog {
        error,
        default_selection,
        providers,
        web_search,
    }
}
