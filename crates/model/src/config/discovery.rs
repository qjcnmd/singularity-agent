use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

use super::{ProviderError, configuration_error, validate_api_key, validate_base_url, validate_identifier};
use crate::ModelErrorKind;
use crate::error::{
    apply_provider_error_body, bounded_provider_error_diagnostic, provider_error_kind_for_http_status,
};
use crate::transport::http::{
    BoundedBodyReadError, MAX_HTTP_ERROR_BODY_BYTES, apply_provider_auth, client_builder, read_bounded_body,
    transport_error_source,
};

const MAX_DISCOVERY_BODY_BYTES: usize = 4 * 1024 * 1024;
const DISCOVERY_TIMEOUT_SECONDS: u64 = 20;

fn discovery_client() -> Result<reqwest::Client, ProviderError> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = client_builder()
        .timeout(Duration::from_secs(DISCOVERY_TIMEOUT_SECONDS))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(app_core::identity::USER_AGENT)
        .build()
        .map_err(|error| {
            configuration_error(format!("模型查询客户端无法启动（{}）。", transport_error_source(error)))
        })?;
    Ok(CLIENT.get_or_init(|| client).clone())
}

pub async fn discover(
    base_url: &str,
    api_key: &str,
    protocol: protocol::ProviderApiProtocol,
) -> Result<Vec<String>, ProviderError> {
    let base_url = crate::provider::canonical_base_url(base_url);
    validate_base_url(base_url)?;
    validate_api_key(api_key)?;
    let client = discovery_client()?;
    let request =
        apply_provider_auth(client.get(crate::provider::endpoint(base_url, "/models")), protocol, api_key)?;
    let response = request.send().await.map_err(discovery_transport_error)?;
    if !response.status().is_success() {
        return Err(discovery_http_error(response).await);
    }
    let body: Value = read_response_body(response).await?;
    read_listing(&body)
}

async fn read_response_body(mut response: reqwest::Response) -> Result<Value, ProviderError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(discovery_transport_error)? {
        if bytes.len().saturating_add(chunk.len()) > MAX_DISCOVERY_BODY_BYTES {
            return Err(ProviderError::new(ModelErrorKind::Other, "模型目录响应过大。"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        let detail = bounded_provider_error_diagnostic(&error.to_string());
        ProviderError::new(ModelErrorKind::Other, format!("提供方未返回有效的模型目录（{detail}）。"))
    })
}

fn discovery_transport_error(error: reqwest::Error) -> ProviderError {
    ProviderError::new(
        ModelErrorKind::Temporary,
        format!("无法连接模型目录（{}）。", transport_error_source(error)),
    )
}

fn read_listing(body: &Value) -> Result<Vec<String>, ProviderError> {
    let mut models = BTreeSet::new();
    let mut add_model = |id: &str, entry: &Value| {
        if entry.is_object() && validate_identifier(id, "model id").is_ok() {
            models.insert(id.to_string());
        }
    };
    if let Some(data) = body.get("data").and_then(Value::as_array) {
        for entry in data {
            if let Some(id) = entry.get("id").and_then(Value::as_str) {
                add_model(id, entry);
            }
        }
    } else if let Some(directory) = body.get("models").and_then(Value::as_object) {
        for (id, entry) in directory {
            add_model(id, entry);
        }
    } else {
        return Err(ProviderError::new(ModelErrorKind::Other, "提供方未返回 data 模型列表或 models 目录。"));
    }
    Ok(models.into_iter().collect())
}

async fn discovery_http_error(response: reqwest::Response) -> ProviderError {
    let status = response.status().as_u16();
    let mut error = ProviderError::new(
        provider_error_kind_for_http_status(status),
        format!("获取模型失败：HTTP {status}。"),
    );
    match read_bounded_body(response, MAX_HTTP_ERROR_BODY_BYTES, None).await {
        Ok(body) => apply_provider_error_body(&mut error, &body),
        Err(BoundedBodyReadError::Transport(read_error)) => {
            let _ =
                write!(error.message, " 无法读取提供方错误响应（{}）。", transport_error_source(read_error));
        }
        Err(BoundedBodyReadError::Cancelled) => unreachable!("un-cancellable body read"),
    }
    error
}
