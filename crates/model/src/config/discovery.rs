//! 从用户配置的提供方查询模型列表与元数据。

use std::collections::BTreeMap;

use super::model_metadata::metadata;
use serde_json::Value;
use singularity_protocol::DiscoveredModel;

use super::{ProviderError, configuration_error, validate_base_url, validate_identifier};
use crate::ModelErrorKind;
use crate::transport::http::{client_builder, transport_error_source};

/// 查询提供方的模型列表与元数据；调用方只提供编辑器里的取值和解析好的凭据。
pub async fn discover(base_url: &str, api_key: &str) -> Result<Vec<DiscoveredModel>, ProviderError> {
    let base_url = crate::openai::canonical_base_url(base_url);
    validate_base_url(base_url)?;
    let client = client_builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            // 客户端构造失败和请求失败是两回事：保留底层原因方便定位。
            configuration_error(format!("模型查询客户端无法启动（{}）。", transport_error_source(error)))
        })?;
    let request = client.get(crate::openai::models_endpoint(base_url));
    // 有密钥时附带 Bearer 认证头，空密钥直接发匿名请求。
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
    read_listing(&body)
}

/// 分两步读取响应：先读字节，再解析 JSON，这样网络中断和无效 JSON 能报成不同的错误。
/// 直接用 Response::json 时，reqwest 把读取中断也算作解码错误（is_decode() 为 true），
/// 两者分不开。
async fn read_response_body(response: reqwest::Response) -> Result<Value, ProviderError> {
    let bytes = response.bytes().await.map_err(discovery_transport_error)?;
    serde_json::from_slice(&bytes)
        .map_err(|_| discovery_response_error("提供方未返回有效的模型目录。仍可手动添加模型。"))
}

/// 发现失败保留去掉 URL、截断后的原因；目录查询由用户手动重试。
fn discovery_transport_error(error: reqwest::Error) -> ProviderError {
    ProviderError::new(
        ModelErrorKind::Temporary,
        format!(
            "无法连接模型目录（{}），请检查网络或稍后重试；仍可手动添加模型。",
            transport_error_source(error)
        ),
    )
}

fn read_listing(body: &Value) -> Result<Vec<DiscoveredModel>, ProviderError> {
    let entries: Vec<(&str, &Value)> = if let Some(data) = body.get("data").and_then(Value::as_array) {
        data.iter().filter_map(|entry| Some((entry.get("id")?.as_str()?, entry))).collect()
    } else if let Some(models) = body.get("models").and_then(Value::as_object) {
        models.iter().map(|(id, entry)| (id.as_str(), entry)).collect()
    } else {
        return Err(discovery_response_error("提供方未返回 data 模型列表或 models 目录。仍可手动添加模型。"));
    };
    let mut models = BTreeMap::new();
    for (id, entry) in entries {
        // 格式不对或 ID 不合法的条目直接跳过，不让整次发现失败。
        if entry.is_object() && validate_identifier(id, "model id").is_ok() {
            models.entry(id.to_string()).or_insert_with(|| metadata(id, entry));
        }
    }
    Ok(models.into_values().collect())
}

fn discovery_response_error(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ModelErrorKind::JsonSchemaViolation, message)
}

fn discovery_http_error(status: u16) -> ProviderError {
    // 模型发现由用户手动触发，不自动重试，只复用 HTTP 状态码的错误分类。
    ProviderError::new(
        crate::error::provider_error_kind_for_http_status(status),
        format!("获取模型失败：HTTP {status}。仍可手动添加模型。"),
    )
}
