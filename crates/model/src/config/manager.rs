//! 用户配置的保存、脱敏目录和执行快照。文件读取在 user，模型解析在 selection。

use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use singularity_protocol::{
    ModelConfigurationInput, ProviderConfigurationInput, ReasoningVariant, RedactedModelCatalog,
    RedactedProvider,
};

use super::*;

/// 一次轮次的不可变容量快照：在每轮开始时冻结「请求前压缩」和「输出预算」需要的两项容量；
/// 改设置只产生后续轮次的新快照，不会改写正在用的快照。这里只放真正被消费的容量数字，
/// 模型身份和协议由 SelectedModel、OpenAiProviderConfig 和 ProviderAttemptEvent 承载。
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
    data: Result<Option<UserConfigData>, ProviderError>,
}

impl ProviderConfigSnapshot {
    fn read(directory: &Path) -> Self {
        Self {
            data: read_user_config_data_from_directory(directory),
        }
    }

    fn redacted_catalog(&self) -> RedactedModelCatalog {
        match &self.data {
            Ok(Some(data)) => {
                let selection = match data.config.default_model.as_deref() {
                    Some(selector) => self.validate_selector(Some(selector)),
                    None => Ok(()),
                };
                catalog_from_data(data, selection)
            }
            Ok(None) => empty_catalog(None),
            Err(error) => empty_catalog(Some(error.to_string())),
        }
    }

    pub(crate) fn config(&self) -> Result<&UserConfigData, ProviderError> {
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

/// 一次配置修改的结果及修改后的实际目录；部分保存失败也返回当前磁盘事实。
pub struct ModelConfigUpdate {
    pub result: Result<(), ProviderError>,
    pub catalog: RedactedModelCatalog,
}

/// 数据目录内的模型配置与凭据入口；读取、修改及修改后的目录快照在内部串行化。
pub struct ModelConfigManager {
    directory: PathBuf,
    access: Mutex<()>,
}

impl ModelConfigManager {
    /// 把提供方从后续的模型选择里移除。正在跑的轮次仍用它自己的快照。
    pub fn remove_provider(&self, provider_id: &str) -> ModelConfigUpdate {
        self.update(|manager| manager.delete_provider(provider_id))
    }

    fn delete_provider(&self, provider_id: &str) -> Result<(), ProviderError> {
        let mut data = read_user_config_data_from_directory(&self.directory)?
            .ok_or_else(|| user_config_error("provider configuration is missing"))?;
        let removed = data.config.providers.remove(provider_id).is_some();
        // 配置已删、只剩凭据时仍算这个提供方存在，好让删除可以重试补完。
        if !removed && !data.auth.providers.contains_key(provider_id) {
            return Err(user_config_error("provider does not exist"));
        }
        if removed {
            clear_invalid_default_selection(&mut data.config);
            write_json_file(&self.directory, crate::USER_CONFIG_FILE_NAME, &data.config)?;
        }
        // 顺序上先让提供方不再可选、再删凭据：上次删除没做完时，重试会补上剩下的凭据删除。
        if data.auth.providers.remove(provider_id).is_some() {
            write_json_file(&self.directory, crate::USER_AUTH_FILE_NAME, &data.auth).map_err(
                |mut error| {
                    error.message =
                        format!("提供方配置已删除，但 API 密钥删除失败；请重试删除：{}", error.message);
                    // 与保存失败一样标成半成品状态：界面据此提示重试这一步，而不是笼统的配置错误。
                    error.with_code(crate::CREDENTIAL_DELETE_FAILED_CODE)
                },
            )?;
        }
        Ok(())
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
            None => Ok(read_user_auth_file(&self.directory)?
                .providers
                .get(provider_id)
                .map(|auth| auth.api_key.clone())
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
    /// 先写配置再写密钥：密钥写失败会返回「部分保存」错误，已写入的配置依然生效。
    pub fn save_provider(
        &self,
        input: ProviderConfigurationInput,
        api_key: Option<&str>,
    ) -> ModelConfigUpdate {
        self.update(|manager| manager.write_provider(input, api_key))
    }

    /// 修改和目录读取共用临界区；失败不能跳过读取，因为配置和凭据可能部分提交。
    fn update(&self, change: impl FnOnce(&Self) -> Result<(), ProviderError>) -> ModelConfigUpdate {
        let _access = self.lock();
        let result = change(self);
        let catalog = ProviderConfigSnapshot::read(&self.directory).redacted_catalog();
        ModelConfigUpdate { result, catalog }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.access.lock().expect("model configuration lock poisoned")
    }

    fn write_provider(
        &self,
        input: ProviderConfigurationInput,
        api_key: Option<&str>,
    ) -> Result<(), ProviderError> {
        validate_identifier(&input.provider_id, "provider id")?;
        // 密钥是本次请求的纯输入，所以在任何文件读写之前先校验：否则非法密钥会先写下
        // config.json，再以「配置已保存、密钥保存失败」结束，白白留下持久化改动。
        if let Some(key) = api_key {
            validate_provider_value(key, "api_key")?;
        }
        // 这里只规范输入形状（去掉空白和结尾斜杠）：地址怎么解释统一交给
        // openai::wire，已写明的端点原样保留，免得给自定义前缀拼出错误路由。
        let base_url = crate::openai::canonical_base_url(&input.base_url).to_string();
        validate_base_url(&base_url)?;
        let mut config = read_user_config_file(&self.directory)?.unwrap_or_default();
        // 先只构造模型映射，任何一项校验失败都发生在写配置和凭据之前。
        let models = model_definitions(
            input.models,
            input.api_protocol.as_deref(),
            config.providers.get(&input.provider_id).map(|provider| &provider.models),
        )?;
        config.providers.insert(
            input.provider_id.clone(),
            UserConfigProvider {
                api_protocol: input.api_protocol,
                display_name: input.display_name.filter(|name| !name.trim().is_empty()),
                base_url,
                models,
            },
        );
        clear_invalid_default_selection(&mut config);
        write_json_file(&self.directory, crate::USER_CONFIG_FILE_NAME, &config)?;
        // 输入已经验证完毕；配置和密钥分别提交，保留第二个文件写入失败的反馈。
        if let Some(key) = api_key {
            self.write_api_key(&input.provider_id, key).map_err(|mut error| {
                error.message =
                    format!("提供方配置已保存，但 API 密钥保存失败；请重试保存：{}", error.message);
                error.with_code(crate::CREDENTIAL_SAVE_FAILED_CODE)
            })?;
        }
        Ok(())
    }

    fn write_api_key(&self, provider_id: &str, api_key: &str) -> Result<(), ProviderError> {
        let mut auth = read_user_auth_file(&self.directory)?;
        auth.providers
            .insert(provider_id.to_string(), UserAuthProvider { api_key: api_key.to_string() });
        write_json_file(&self.directory, crate::USER_AUTH_FILE_NAME, &auth)?;
        Ok(())
    }
}

/// 把本次提交的模型输入转成要落盘的模型映射：只做纯转换，不读写配置和凭据；模型 id、容量、
/// 档位和重复项的校验都在返回前完成，调用方拿到完整映射后才提交，失败不会留下写了一半的结果。
///
/// 旧配置里那些表单上没有的能力标记按模型 id 保留；旧配置里没有的模型不参与转换。
fn model_definitions(
    models: Vec<ModelConfigurationInput>,
    provider_protocol: Option<&str>,
    previous_models: Option<&BTreeMap<String, UserConfigModel>>,
) -> Result<BTreeMap<String, UserConfigModel>, ProviderError> {
    let parsed_provider = provider_protocol.map(parse_catalog_protocol).transpose()?;
    let mut definitions = BTreeMap::new();
    for mut model in models {
        let protocol = provider_protocol.or(model.api_protocol.as_deref());
        if let Some(protocol) = parsed_provider {
            schema::normalize_chat_fields(
                protocol,
                &mut model.thinking_wire_format,
                &mut model.chat_output_tokens_field,
                &mut model.requires_reasoning_content_for_tool_calls,
            );
        }
        validate_model_id(&model.model_id, "model id")?;
        if definitions.contains_key(&model.model_id) {
            return Err(user_config_error("provider model ids must be unique"));
        }
        let declared_variants = model.reasoning_variants.is_some();
        let mut variants = BTreeMap::new();
        for variant in model.reasoning_variants.into_iter().flatten() {
            if variants
                .insert(variant.id, ModelsFileReasoningVariant { wire_effort: variant.wire_effort })
                .is_some()
            {
                return Err(user_config_error("reasoning variant ids must be unique"));
            }
        }
        let previous = previous_models.and_then(|models| models.get(&model.model_id));
        let is_chat = protocol == Some("chat");
        let configured = UserConfigModel {
            automatic_fields: model.automatic_fields,
            display_name: model.display_name.filter(|name| !name.trim().is_empty()),
            api_protocol: if provider_protocol.is_some() {
                None
            } else {
                model.api_protocol.clone()
            },
            max_context_tokens: model.max_context_tokens,
            max_output_tokens: model.max_output_tokens,
            reasoning_variants: declared_variants.then_some(variants),
            default_variant: model.default_variant,
            supports_developer_role: previous.and_then(|model| model.supports_developer_role),
            supports_tool_choice: previous.and_then(|model| model.supports_tool_choice),
            requires_reasoning_content_for_tool_calls: model.requires_reasoning_content_for_tool_calls,
            requires_assistant_content_for_tool_calls: previous
                .is_some_and(|previous| previous.requires_assistant_content_for_tool_calls)
                && is_chat,
            chat_output_tokens_field: model.chat_output_tokens_field.filter(|field| !field.is_empty()),
            thinking_wire_format: model.thinking_wire_format,
        };
        resolve_model_definition(&configured, protocol, &model.model_id, None)?;
        definitions.insert(model.model_id, configured);
    }
    Ok(definitions)
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

fn catalog_from_data(data: &UserConfigData, selection: Result<(), ProviderError>) -> RedactedModelCatalog {
    if data.config.providers.is_empty() {
        return empty_catalog(None);
    }
    let (error, default_selector) = match selection {
        _ if data.config.providers.values().all(|provider| provider.models.is_empty()) => (None, None),
        Ok(()) => (None, data.config.default_model.clone()),
        Err(error) => (Some(error.to_string()), data.config.default_model.clone()),
    };
    let providers = data
        .config
        .providers
        .iter()
        .map(|(provider_id, provider)| RedactedProvider {
            api_protocol: provider.api_protocol.clone(),
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
                .map(|(model_id, model)| ModelConfigurationInput {
                    automatic_fields: model.automatic_fields.clone(),
                    model_id: model_id.clone(),
                    display_name: model.display_name.clone(),
                    api_protocol: provider.api_protocol.clone().or_else(|| model.api_protocol.clone()),
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

/// 写配置文件：把当前配置直接序列化成文件字节，只负责序列化和原子替换，不做二次清洗；可选
/// 字段缺省时不落键（由持久化类型的 `skip_serializing_if` 声明），免得给无关供应商补出 `null`。
///
/// 不读文件里已有的内容：条目在不在由类型自身的序列化决定，被删掉的供应商、模型和推理档位会
/// 随保存一起消失。`Map` 按键排序（本仓没启用 `preserve_order`），与文件里原先的书写顺序无关。
fn write_json_file(directory: &Path, file_name: &str, value: &impl Serialize) -> Result<(), ProviderError> {
    singularity_core::create_data_dir(directory).map_err(user_config_error)?;
    let path = directory.join(file_name);
    let mut bytes = serde_json::to_vec_pretty(value).expect("provider configuration is JSON serializable");
    bytes.push(b'\n');
    singularity_core::atomic_replace_bytes(&path, &bytes)
        .map_err(|error| user_config_error(format!("could not update {}: {error}", path.display())))
}
