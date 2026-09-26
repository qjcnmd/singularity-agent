//! 配置编辑器用的只读模型元数据。已保存的配置始终是权威来源。

use std::collections::BTreeMap;

use super::model_metadata::{metadata, supplement, supplement_documented};
use serde_json::Value;
use singularity_protocol::DiscoveredModel;

use super::{ProviderError, user_config_error, validate_base_url, validate_model_id};
use crate::ModelErrorKind;
use crate::transport::http::{BodyReadError, read_bounded_response_body, transport_error_source};

const MAX_MODEL_DIRECTORY_BYTES: usize = 32 * 1024 * 1024;

/// 查询模型目录并补齐元数据：地址解释、请求构造、发送和结果补全都在这里，调用方只提供
/// 编辑器里的取值和解析好的凭据，不转交 HTTP 的半成品。
pub async fn discover(
    base_url: &str,
    api_key: &str,
    api_protocol: &str,
) -> Result<Vec<DiscoveredModel>, ProviderError> {
    let protocol = super::parse_catalog_protocol(api_protocol)?;
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
                transport_error_source(error)
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
    if protocol == crate::ProviderApiProtocol::Chat {
        supplement_documented(&mut models, base_url);
    }
    if models.iter().any(|model| {
        model.max_context_tokens.is_none()
            || model.max_output_tokens.is_none()
            || model.reasoning_variants.is_empty()
    }) {
        // 这个公开目录请求不带用户的 endpoint，也不带凭据。
        if let Ok(client) = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent("Singularity model discovery")
            .build()
            && let Ok(response) = client.get("https://models.dev/api.json").send().await
            && response.status().is_success()
            && let Ok(directory) = read_response_body(response).await
        {
            supplement(&mut models, base_url, &directory);
        }
    }
    for model in &mut models {
        super::schema::normalize_chat_fields(
            protocol,
            &mut model.thinking_wire_format,
            &mut model.chat_output_tokens_field,
            &mut model.requires_reasoning_content_for_tool_calls,
        );
    }
    Ok(models)
}

/// 读响应体分两步：有界读取字节（传输层面的事实），再自己解码（结构层面的事实），让两类失败各自
/// 保持原来的类别。reqwest 把 body 读取错误也归到 decode 类（0.12 里 body 断流的 is_decode()
/// 同样是 true），只用 Response::json 加一个 map_err 分不开「body 传输超时或断流」和
/// 「提供方返回了无效 JSON」。
async fn read_response_body(response: reqwest::Response) -> Result<Value, ProviderError> {
    let bytes = read_bounded_response_body(response, MAX_MODEL_DIRECTORY_BYTES)
        .await
        .map_err(|error| match error {
            BodyReadError::Transport(error) => discovery_transport_error(error),
            BodyReadError::TooLarge => {
                discovery_response_error("模型目录响应过大。仍可手动添加模型。")
            }
        })?;
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
            transport_error_source(error)
        ),
    )
    .with_code("model_discovery_transport_failed")
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
