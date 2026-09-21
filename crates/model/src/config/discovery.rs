//! 配置编辑器用的只读模型元数据。已保存的配置始终是权威来源。

use std::collections::BTreeMap;

use serde_json::Value;
use singularity_protocol::{DiscoveredModel, ReasoningVariant};

use super::{
    ProviderError, user_config_error, validate_base_url, validate_identifier, validate_model_id,
};
use crate::ModelErrorKind;

/// 查询模型目录并补齐元数据：地址解释、请求构造、发送和结果补全都在这里，调用方只提供
/// 编辑器里的取值和解析好的凭据，不转交 HTTP 的半成品。
pub async fn discover(
    base_url: &str,
    api_key: &str,
) -> Result<Vec<DiscoveredModel>, ProviderError> {
    let base_url = crate::openai::canonical_base_url(base_url);
    validate_base_url(base_url)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            // 客户端构造失败和请求失败是两回事：保留底层原因方便定位。
            user_config_error(format!(
                "模型查询客户端无法启动（{}）。",
                discovery_error_source(error)
            ))
        })?;
    let request = client.get(crate::openai::models_endpoint(base_url));
    // 未保存的密钥是这次查询的纯输入；没提供时用已存的密钥（可能为空）。
    let request = if api_key.is_empty() {
        request
    } else {
        request.bearer_auth(api_key)
    };
    let response = request.send().await.map_err(discovery_transport_error)?;
    if !response.status().is_success() {
        return Err(discovery_http_error(response.status().as_u16()));
    }
    let body: Value = read_response_body(response).await?;
    let mut models = read_listing(&body)?;
    if models.iter().any(|model| {
        model.max_context_tokens.is_none()
            || model.max_output_tokens.is_none()
            || model.reasoning_variants.is_empty()
    }) {
        // 这个公开目录请求不带用户的 endpoint，也不带凭据。
        if let Ok(client) = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .build()
            && let Ok(response) = client.get("https://models.dev/api.json").send().await
            && response.status().is_success()
            && let Ok(directory) = response.json::<Value>().await
        {
            supplement(&mut models, base_url, &directory);
        }
    }
    Ok(models)
}

/// 读响应体分两步：先取字节（传输层面的事实），再自己解码（结构层面的事实），让两类失败各自
/// 保持原来的类别。reqwest 把 body 读取错误也归到 decode 类（0.12 里 body 断流的 is_decode()
/// 同样是 true），只用 Response::json 加一个 map_err 分不开「body 传输超时或断流」和
/// 「提供方返回了无效 JSON」。
async fn read_response_body(response: reqwest::Response) -> Result<Value, ProviderError> {
    let bytes = response.bytes().await.map_err(discovery_transport_error)?;
    serde_json::from_slice(&bytes)
        .map_err(|_| discovery_response_error("提供方未返回有效的模型目录。仍可手动添加模型。"))
}

/// 发现请求的传输失败（发送或读 body）：保留原有类别，附上去掉 URL、截断后的原因文本；这条
/// 路径不重试。
fn discovery_transport_error(error: reqwest::Error) -> ProviderError {
    let kind = crate::error::provider_error_kind_for_transport(&error);
    ProviderError::new(
        kind,
        format!(
            "无法连接模型目录（{}），请检查网络或稍后重试；仍可手动添加模型。",
            discovery_error_source(error)
        ),
    )
    .with_code("model_discovery_transport_failed")
}

/// 有长度上限、已脱敏的原因文本：reqwest 的 Display 只给大类（body 读取失败一律是
/// "error decoding response body"），具体原因在错误来源链的最内层（超时、断流等），所以取
/// 最内层的描述；再去掉 URL（凭据只在请求头，本来也不会进错误文本），按共用上限折行截断。
fn discovery_error_source(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut source: &(dyn std::error::Error + 'static) = &error;
    while let Some(inner) = source.source() {
        source = inner;
    }
    crate::error::bounded_provider_error_diagnostic(&source.to_string())
}

fn read_listing(body: &Value) -> Result<Vec<DiscoveredModel>, ProviderError> {
    let entries: Vec<(&str, &Value)> =
        if let Some(data) = body.get("data").and_then(Value::as_array) {
            data.iter()
                .filter_map(|entry| Some((entry.get("id")?.as_str()?, entry)))
                .collect()
        } else if let Some(models) = body.get("models").and_then(Value::as_object) {
            models
                .iter()
                .map(|(id, entry)| (id.as_str(), entry))
                .collect()
        } else {
            return Err(discovery_response_error(
                "提供方未返回 data 模型列表或 models 目录。仍可手动添加模型。",
            ));
        };
    let mut models = BTreeMap::new();
    for (id, entry) in entries {
        // 形状不对或 id 不合法的条目直接跳过，不让整次发现因此失败。
        if entry.is_object() && validate_model_id(id, "model id").is_ok() {
            models
                .entry(id.to_string())
                .or_insert_with(|| metadata(id, entry));
        }
    }
    Ok(models.into_values().collect())
}

fn discovery_response_error(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ModelErrorKind::JsonSchemaViolation, message)
        .with_code("model_discovery_response_invalid")
}

fn discovery_http_error(status: u16) -> ProviderError {
    // 发现请求不重试，所以直接用共同的状态分类：409 在这里算输入错误，600 以上
    // 算未知；生成路径把 409 当可重试、把 600+ 当过载，那是它自己的例外。
    ProviderError::new(
        crate::error::provider_error_kind_for_http_status(status),
        format!("获取模型失败：HTTP {status}。仍可手动添加模型。"),
    )
    .with_code("model_discovery_http_status")
}

fn metadata(id: &str, entry: &Value) -> DiscoveredModel {
    let label = |paths: &[&str]| {
        paths.iter().find_map(|path| {
            entry
                .pointer(path)?
                .as_str()
                .filter(|text| !text.is_empty())
        })
    };
    let capacity = |paths: &[&str]| {
        paths.iter().find_map(|path| {
            u32::try_from(entry.pointer(path)?.as_u64()?)
                .ok()
                .filter(|value| *value > 0)
        })
    };
    let mut efforts = Vec::new();
    if let Some(values) = entry.get("reasoning_efforts").and_then(Value::as_array) {
        efforts.extend(values.iter().filter_map(Value::as_str));
    }
    if let Some(options) = entry.get("reasoning_options").and_then(Value::as_array) {
        for option in options {
            if option.get("type").and_then(Value::as_str) == Some("effort")
                && let Some(values) = option.get("values").and_then(Value::as_array)
            {
                efforts.extend(values.iter().filter_map(Value::as_str));
            }
        }
    }
    let mut reasoning_variants = Vec::new();
    for effort in efforts {
        // default 只是占位值，off 是本仓约定的关闭档位，两者都不作为可选档位。
        if !matches!(effort, "default" | "off")
            && validate_identifier(effort, "reasoning effort").is_ok()
            && !reasoning_variants
                .iter()
                .any(|variant: &ReasoningVariant| variant.id == effort)
        {
            reasoning_variants.push(ReasoningVariant {
                id: effort.to_string(),
                enabled: true,
                wire_effort: Some(effort.to_string()),
            });
        }
    }
    // 优先用对方声明的 default，其次 medium；导入配置时也按这个默认值。
    let default_variant = label(&["/default_reasoning_effort"])
        .filter(|id| reasoning_variants.iter().any(|variant| variant.id == *id))
        .or_else(|| {
            reasoning_variants
                .iter()
                .find(|variant| variant.id == "medium")
                .map(|variant| variant.id.as_str())
        })
        .or_else(|| {
            reasoning_variants
                .first()
                .map(|variant| variant.id.as_str())
        })
        .map(str::to_string);
    DiscoveredModel {
        model_id: id.to_string(),
        display_name: label(&["/name", "/display_name", "/displayName"]).map(str::to_string),
        max_context_tokens: capacity(&[
            "/context_window",
            "/context_length",
            "/contextWindow",
            "/max_input_tokens",
            "/limit/context",
        ]),
        max_output_tokens: capacity(&[
            "/max_output_tokens",
            "/maxOutputTokens",
            "/max_tokens",
            "/maxTokens",
            "/limit/output",
            "/top_provider/max_completion_tokens",
        ]),
        reasoning_variants,
        default_variant,
    }
}

/// 用目录条目的 `api` 地址去对齐本机配置的地址：地址是用户填的，程序不认识
/// 任何厂商，也不会给缺 `api` 字段的条目硬写一个地址。
fn supplement(models: &mut [DiscoveredModel], base_url: &str, directory: &Value) {
    let Some(providers) = directory.as_object() else {
        return;
    };
    // 与推理共用同一套地址解释：目录补齐要用的根，才是 models.dev 记录的 api 值。
    let endpoint = crate::openai::api_root(base_url);
    let Some((_, provider)) = providers.iter().find(|(_, provider)| {
        provider
            .get("api")
            .and_then(Value::as_str)
            .is_some_and(|api| crate::openai::canonical_base_url(api) == endpoint)
    }) else {
        return;
    };
    for model in models {
        let Some(entry) = provider
            .get("models")
            .and_then(|models| models.get(&model.model_id))
        else {
            continue;
        };
        let known = metadata(&model.model_id, entry);
        model.display_name = model.display_name.take().or(known.display_name);
        model.max_context_tokens = model.max_context_tokens.or(known.max_context_tokens);
        model.max_output_tokens = model.max_output_tokens.or(known.max_output_tokens);
        // 思考的线上字段形式各家不同，目录不提供；这里只补档位，字段形式留给用户声明。
        if model.reasoning_variants.is_empty() && !known.reasoning_variants.is_empty() {
            model.reasoning_variants = known.reasoning_variants;
            model.default_variant = known.default_variant;
        }
    }
}
