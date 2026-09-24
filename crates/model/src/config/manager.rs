//! 用户配置的保存、脱敏目录和执行快照。文件读取在 user，模型解析在 selection。

use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use singularity_protocol::{
    ModelConfigurationInput, ModelConfigurationStatus, ProviderConfigurationInput,
    ReasoningVariant, RedactedModelCatalog, RedactedProvider,
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
    pub(crate) fn config(&self) -> Result<&UserConfigData, ProviderError> {
        self.data
            .as_ref()
            .map_err(Clone::clone)?
            .as_ref()
            .ok_or_else(|| missing_provider_config_error(crate::USER_CONFIG_FILE_NAME))
    }

    /// 从进程选定的用户数据目录读取配置并冻结。
    pub fn capture(directory: &std::path::Path) -> Self {
        Self {
            data: read_user_config_data_from_directory(directory),
        }
    }

    /// 返回从用户配置解析出的默认 selector（provider/model#effort）；
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

pub struct ModelConfigManager {
    directory: PathBuf,
}

impl ModelConfigManager {
    /// 把提供方从后续的模型选择里移除。正在跑的轮次仍用它自己的快照。
    pub fn remove_provider(&mut self, provider_id: &str) -> Result<(), ProviderError> {
        let mut data = read_user_config_data_from_directory(&self.directory)?
            .ok_or_else(|| user_config_error("provider configuration is missing"))?;
        let removed = data.config.providers.remove(provider_id).is_some();
        // 配置已删、只剩凭据时仍算这个提供方存在，好让删除可以重试补完。
        if !removed && !data.auth.providers.contains_key(provider_id) {
            return Err(user_config_error("provider does not exist"));
        }
        if removed {
            repair_default_selection(&mut data.config);
            write_json_file(&self.directory, crate::USER_CONFIG_FILE_NAME, &data.config)?;
        }
        // 顺序上先让提供方不再可选、再删凭据：上次删除没做完时，重试会补上剩下的凭据删除。
        if data.auth.providers.remove(provider_id).is_some() {
            write_json_file(&self.directory, crate::USER_AUTH_FILE_NAME, &data.auth).map_err(
                |mut error| {
                    error.message = format!(
                        "提供方配置已删除，但 API 密钥删除失败；请重试删除：{}",
                        error.message
                    );
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
        match api_key.filter(|key| !key.is_empty()) {
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

    pub fn open(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub fn snapshot(&self) -> ProviderConfigSnapshot {
        ProviderConfigSnapshot::capture(&self.directory)
    }

    /// 用同一次读取的结果生成脱敏目录，不缓存磁盘上的配置。
    pub fn redacted_catalog(&self) -> RedactedModelCatalog {
        let snapshot = self.snapshot();
        match &snapshot.data {
            Ok(Some(data)) => catalog_from_data(data, snapshot.validate_selector(None)),
            Ok(None) => empty_catalog(
                ModelConfigurationStatus::Missing,
                "配置一个模型提供方后即可开始新任务。".to_string(),
            ),
            Err(error) => empty_catalog(ModelConfigurationStatus::Invalid, error.to_string()),
        }
    }

    /// 保存提供方配置，需要时同时替换密钥；密钥省略或留空表示保留原值。
    /// 先写配置再写密钥：密钥写失败会返回「部分保存」错误，已写入的配置依然生效。
    pub fn save_provider(
        &mut self,
        input: ProviderConfigurationInput,
        api_key: Option<&str>,
    ) -> Result<(), ProviderError> {
        validate_identifier(&input.provider_id, "provider id")?;
        // 密钥是本次请求的纯输入，所以在任何文件读写之前先校验：否则非法密钥会先写下
        // config.json，再以「配置已保存、密钥保存失败」结束，白白留下持久化改动。
        let api_key = api_key.filter(|key| !key.is_empty());
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
            config
                .providers
                .get(&input.provider_id)
                .map(|provider| &provider.models),
        )?;
        config.providers.insert(
            input.provider_id.clone(),
            UserConfigProvider {
                display_name: input.display_name.filter(|name| !name.trim().is_empty()),
                base_url,
                models,
            },
        );
        repair_default_selection(&mut config);
        write_json_file(&self.directory, crate::USER_CONFIG_FILE_NAME, &config)?;
        // 密钥仍走单独的写入入口：配置和密钥是两个文件，真正的 I/O 失败只影响后者。
        if let Some(key) = api_key {
            self.set_api_key(&input.provider_id, key)
                .map_err(|mut error| {
                    error.message = format!(
                        "提供方配置已保存，但 API 密钥保存失败；请重试保存：{}",
                        error.message
                    );
                    error.with_code(crate::CREDENTIAL_SAVE_FAILED_CODE)
                })?;
        }
        Ok(())
    }
    pub fn set_api_key(&mut self, provider_id: &str, api_key: &str) -> Result<(), ProviderError> {
        validate_identifier(provider_id, "provider id")?;
        validate_provider_value(api_key, "api_key")?;
        if api_key.is_empty() {
            return Err(user_config_error("API key must not be empty"));
        }
        let mut auth = read_user_auth_file(&self.directory)?;
        auth.providers.insert(
            provider_id.to_string(),
            UserAuthProvider {
                api_key: api_key.to_string(),
            },
        );
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
    previous_models: Option<&BTreeMap<String, UserConfigModel>>,
) -> Result<BTreeMap<String, UserConfigModel>, ProviderError> {
    let mut definitions = BTreeMap::new();
    for model in models {
        validate_model_id(&model.model_id, "model id")?;
        if definitions.contains_key(&model.model_id) {
            return Err(user_config_error("provider model ids must be unique"));
        }
        let mut variants = BTreeMap::new();
        for variant in model.reasoning_variants {
            if variants
                .insert(
                    variant.id,
                    ModelsFileReasoningVariant {
                        wire_effort: variant.wire_effort,
                    },
                )
                .is_some()
            {
                return Err(user_config_error("reasoning variant ids must be unique"));
            }
        }
        let previous = previous_models.and_then(|models| models.get(&model.model_id));
        let configured = UserConfigModel {
            display_name: model.display_name.filter(|name| !name.trim().is_empty()),
            api_protocol: model.api_protocol,
            max_context_tokens: model.max_context_tokens,
            max_output_tokens: model.max_output_tokens,
            reasoning_variants: variants,
            default_variant: model.default_variant,
            supports_developer_role: previous.and_then(|model| model.supports_developer_role),
            supports_tool_choice: previous.and_then(|model| model.supports_tool_choice),
            requires_reasoning_content_for_tool_calls: previous
                .is_some_and(|model| model.requires_reasoning_content_for_tool_calls),
            requires_assistant_content_for_tool_calls: previous
                .is_some_and(|model| model.requires_assistant_content_for_tool_calls),
            // 表单上没有这个开关的控件：保存时按输入原样往返，已有的取值由设置页从目录读回后
            // 一起带回来。
            chat_output_tokens_field: model.chat_output_tokens_field,
            thinking_wire_format: model.thinking_wire_format,
        };
        resolve_model_definition(&configured, &model.model_id, None)?;
        definitions.insert(model.model_id, configured);
    }
    Ok(definitions)
}

// 编辑时删掉所选模型的显式推理档位仍保留该模型；只有原模型本身已不存在，才改选别的模型。
fn repair_default_selection(config: &mut UserConfigFile) {
    let current = config.default_model.as_deref().and_then(|selector| {
        let selected = parse_model_selector(selector).ok()?;
        let model = config
            .providers
            .get(selected.provider_name)?
            .models
            .get(selected.model_name)?;
        let effort = selected
            .reasoning_effort
            .filter(|effort| model.reasoning_variants.contains_key(*effort));
        Some(compose_model_selector(
            selected.provider_name,
            selected.model_name,
            effort,
        ))
    });
    let next = current.or_else(|| {
        config.providers.iter().find_map(|(id, provider)| {
            provider
                .models
                .keys()
                .next()
                .map(|model| compose_model_selector(id, model, None))
        })
    });
    config.default_model = next;
}

fn empty_catalog(configuration: ModelConfigurationStatus, message: String) -> RedactedModelCatalog {
    RedactedModelCatalog {
        configuration,
        message: Some(message),
        default_selector: None,
        providers: Vec::new(),
    }
}

fn catalog_from_data(
    data: &UserConfigData,
    selection: Result<(), ProviderError>,
) -> RedactedModelCatalog {
    if data.config.providers.is_empty() {
        return empty_catalog(
            ModelConfigurationStatus::Missing,
            "添加一个模型提供方即可开始。".to_string(),
        );
    }
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
        Ok(()) => (
            ModelConfigurationStatus::Ready,
            None,
            data.config.default_model.clone(),
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
                .map(|(model_id, model)| ModelConfigurationInput {
                    model_id: model_id.clone(),
                    display_name: model.display_name.clone(),
                    api_protocol: model.api_protocol.clone(),
                    max_context_tokens: model.max_context_tokens,
                    max_output_tokens: model.max_output_tokens,
                    reasoning_variants: model
                        .reasoning_variants
                        .iter()
                        .map(|(id, variant)| ReasoningVariant {
                            id: id.clone(),
                            wire_effort: variant.wire_effort.clone(),
                        })
                        .collect(),
                    default_variant: model.default_variant.clone(),
                    thinking_wire_format: model.thinking_wire_format.clone(),
                    chat_output_tokens_field: model.chat_output_tokens_field.clone(),
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

/// 写配置文件：把当前配置直接序列化成文件字节，只负责序列化和原子替换，不做二次清洗；可选
/// 字段缺省时不落键（由持久化类型的 `skip_serializing_if` 声明），免得给无关供应商补出 `null`。
///
/// 不读文件里已有的内容：条目在不在由类型自身的序列化决定，被删掉的供应商、模型和推理档位会
/// 随保存一起消失。`Map` 按键排序（本仓没启用 `preserve_order`），与文件里原先的书写顺序无关。
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
