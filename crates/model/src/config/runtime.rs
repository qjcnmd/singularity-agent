//! 用户配置保存、脱敏目录与执行快照。文件读取位于 user，模型解析位于 selection。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use singularity_protocol::{
    ModelConfigurationInput, ModelConfigurationStatus, ProviderConfigurationInput,
    ReasoningVariant, RedactedModelCatalog, RedactedProvider,
};

use super::*;

/// 一次 turn 的不可变模型配置快照：逐回合冻结 selector、声明协议与能力合同。
/// 设置变更只产生未来回合的新快照，绝不改写活动快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelConfigurationSnapshot {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_variant: Option<String>,
    pub protocol: ProviderApiProtocol,
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
}

impl ModelConfigurationSnapshot {
    /// 请求前压缩判定使用的已解析上下文窗口。
    pub fn context_window(&self) -> u64 {
        u64::from(self.max_context_tokens)
    }
}

/// 服务级配置快照：冻结一次读取的配置与密钥，按实际 selector 解析。此类型不实现 Debug。
#[derive(Clone)]
pub struct ProviderConfigSnapshot {
    data: Result<Option<std::sync::Arc<UserConfigData>>, ProviderError>,
    runtime_handle: tokio::runtime::Handle,
}

impl ProviderConfigSnapshot {
    fn config(&self) -> Result<&UserConfigData, ProviderError> {
        self.data
            .as_ref()
            .map_err(Clone::clone)?
            .as_deref()
            .ok_or_else(|| missing_provider_config_error(crate::USER_CONFIG_FILE_NAME))
    }

    /// 从进程选定的用户数据目录读取并冻结配置。
    pub fn capture(directory: &std::path::Path, runtime_handle: tokio::runtime::Handle) -> Self {
        Self {
            data: read_user_config_data_from_directory(directory)
                .map(|data| data.map(std::sync::Arc::new)),
            runtime_handle,
        }
    }

    /// 返回用户配置目录解析出的默认 selector（provider/model#effort）；
    /// provider 未配置或无法解析时返回 None（调用方保留 Thread.model 为 NULL）。
    pub fn resolved_default_selector(&self) -> Option<String> {
        let (config, model) = resolve_model_selection(self.config().ok()?, None).ok()?;
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
        let (config, model) = resolve_model_selection(self.config()?, selector)?;
        OpenAiProvider::new(config, model, self.runtime_handle.clone())
    }

    /// 按冻结配置校验 selector，不构造 client。
    pub fn validate_selector(&self, selector: Option<&str>) -> Result<(), ProviderError> {
        resolve_model_selection(self.config()?, selector).map(|_| ())
    }
}

pub struct ModelConfigOwner {
    directory: PathBuf,
    runtime_handle: tokio::runtime::Handle,
}

impl ModelConfigOwner {
    /// 将 provider 从后续模型选择中移除。运行中的 turn 保留其快照。
    pub fn remove_provider(&mut self, provider_id: &str) -> Result<(), ProviderError> {
        let mut data = read_user_config_data_from_directory(&self.directory)?
            .ok_or_else(|| user_config_error("provider configuration is missing"))?;
        let removed = data.config.providers.remove(provider_id).is_some();
        if !removed && !data.auth.providers.contains_key(provider_id) {
            return Err(user_config_error("provider does not exist"));
        }
        if removed {
            repair_default_selection(&mut data.config);
            write_json_file(&self.directory, crate::USER_CONFIG_FILE_NAME, &data.config)?;
        }
        // 仅在 provider 不再可选之后才删除凭据。
        // 重试未完成的删除会补完剩余的凭据写入。
        if data.auth.providers.remove(provider_id).is_some() {
            write_json_file(&self.directory, crate::USER_AUTH_FILE_NAME, &data.auth).map_err(
                |mut error| {
                    error.message = format!(
                        "提供方配置已删除，但 API 密钥删除失败；请重试删除：{}",
                        error.message
                    );
                    // 与保存失败同样标记半成品状态：界面据此给出重试该操作的引导，
                    // 而不是泛化的配置错误。
                    error.with_code(crate::CREDENTIAL_DELETE_FAILED_CODE)
                },
            )?;
        }
        Ok(())
    }

    /// 依据编辑器取值构造只读列表请求；密钥不会离开本机响应。
    pub fn model_discovery_request(
        &self,
        provider_id: &str,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let base_url = crate::openai::canonical_base_url(base_url);
        validate_base_url(base_url)?;
        let key = match api_key.filter(|key| !key.is_empty()) {
            Some(key) => {
                validate_provider_value(key, "api_key")?;
                key.to_string()
            }
            None => read_user_auth_file(&self.directory)?
                .providers
                .get(provider_id)
                .map(|auth| auth.api_key.clone())
                .unwrap_or_default(),
        };
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| user_config_error("模型查询客户端无法启动。"))?;
        let request = client.get(crate::openai::models_endpoint(base_url));
        Ok(if key.is_empty() {
            request
        } else {
            request.bearer_auth(key)
        })
    }

    pub fn open(directory: PathBuf, runtime_handle: tokio::runtime::Handle) -> Self {
        Self {
            directory,
            runtime_handle,
        }
    }

    pub fn snapshot(&self) -> ProviderConfigSnapshot {
        ProviderConfigSnapshot::capture(&self.directory, self.runtime_handle.clone())
    }

    /// 从同次读取派生脱敏目录，不缓存磁盘配置。
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

    /// 保存提供方配置，并按需替换密钥；省略或留空的密钥保留原值。
    /// 配置先写入，密钥写入失败时返回部分保存错误，已保存的配置仍然生效。
    pub fn save_provider(
        &mut self,
        input: ProviderConfigurationInput,
        api_key: Option<&str>,
    ) -> Result<(), ProviderError> {
        validate_identifier(&input.provider_id, "provider id")?;
        // 密钥是本次请求的纯输入：先于任何文件读写校验。否则非法密钥会先落下
        // config.json 写入，再以「配置已保存、密钥保存失败」的部分成功收场，
        // 让本可预先判断的输入错误产生无谓的持久化副作用。
        // 省略或留空仍表示本次不改密钥，因此只校验确实提交的值。
        let api_key = api_key.filter(|key| !key.is_empty());
        if let Some(key) = api_key {
            validate_provider_value(key, "api_key")?;
        }
        // 只规范输入形状（去空白与结尾斜杠）：地址含义留给 openai::wire 一处解释，
        // 已写明的端点原样保留，避免为自定义前缀拼出错误路由。
        let base_url = crate::openai::canonical_base_url(&input.base_url).to_string();
        validate_base_url(&base_url)?;
        let mut config = read_user_config_file(&self.directory)?.unwrap_or_default();
        let previous_models = config
            .providers
            .get(&input.provider_id)
            .map(|provider| &provider.models);
        let mut models = BTreeMap::new();
        for model in input.models {
            validate_model_id(&model.model_id, "model id")?;
            if models.contains_key(&model.model_id) {
                return Err(user_config_error("provider model ids must be unique"));
            }
            let mut variants = BTreeMap::new();
            for variant in model.reasoning_variants {
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
            let previous = previous_models.and_then(|models| models.get(&model.model_id));
            let configured = UserConfigModel {
                display_name: model.display_name.filter(|name| !name.trim().is_empty()),
                api_protocol: model.api_protocol,
                max_context_tokens: model.max_context_tokens,
                max_output_tokens: model.max_output_tokens,
                reasoning_variants: variants,
                default_variant: model.default_variant,
                _legacy_tool_reasoning_history: None,
                supports_developer_role: previous.and_then(|model| model.supports_developer_role),
                supports_tool_choice: previous.and_then(|model| model.supports_tool_choice),
                requires_reasoning_content_for_tool_calls: previous
                    .is_some_and(|model| model.requires_reasoning_content_for_tool_calls),
                requires_assistant_content_for_tool_calls: previous
                    .is_some_and(|model| model.requires_assistant_content_for_tool_calls),
                // 表单不提供该开关的控件；保存时按输入原样往返，既有取值由
                // 设置页从目录读回后带回。
                chat_output_tokens_field: model.chat_output_tokens_field,
                thinking_wire_format: model.thinking_wire_format,
            };
            resolve_model_definition(&configured, &model.model_id, None)?;
            models.insert(model.model_id, configured);
        }
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
        // 密钥写入仍走独立入口：配置与密钥是两个文件，真实 I/O 失败只影响后一个。
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

// 编辑移除所选模型显式的 reasoning 变体时，保留该模型。
// 仅当原模型本身已不存在时，才改选其他模型。
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
    config.default_provider = None;
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
                            enabled: variant.enabled,
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

/// 写入配置文件：把当前配置直接序列化为文件字节。
///
/// 「本层没有该字段的取值」由持久化类型自己声明（`skip_serializing_if`）：所有
/// 可选字段缺省时都不落键，避免一次删除或保存给无关供应商补出 `null`。这里不再
/// 经由 `serde_json::Value` 做二次清洗，写文件只负责序列化与原子替换。
///
/// 不读取文件已有的内容：条目是否存在由类型自身的序列化决定，被删掉的供应商、
/// 模型与推理变体因此随保存消失。结构体字段按声明顺序写出，`Map`（本仓未启用
/// `preserve_order`）仍按键排序；键序与文件里的书写顺序无关，键值语义不变。
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
