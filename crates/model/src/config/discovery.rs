use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{configuration_error, validate_identifier};
use crate::error::{
    ModelErrorKind, ProviderError, apply_provider_error_body, bounded_provider_error_diagnostic,
    provider_error_kind_for_http_status,
};
use crate::transport::http::{
    BoundedBodyReadError, MAX_HTTP_ERROR_BODY_BYTES, apply_provider_auth, client_builder,
    provider_cancelled_error, provider_future_with, read_bounded_body, transport_error_source,
};

const MAX_DISCOVERY_BODY_BYTES: usize = 4 * 1024 * 1024;
const DISCOVERY_TIMEOUT_SECONDS: u64 = 20;

fn discovery_client() -> Result<reqwest::Client, ProviderError> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = client_builder().timeout(Duration::from_secs(DISCOVERY_TIMEOUT_SECONDS)).build().map_err(
        |error| configuration_error(format!("模型查询客户端无法启动（{}）。", transport_error_source(error))),
    )?;
    Ok(CLIENT.get_or_init(|| client).clone())
}

pub(crate) async fn discover(
    base_url: &str,
    api_key: &str,
    protocol: protocol::ProviderApiProtocol,
    cancellation: &CancellationToken,
) -> Result<Vec<String>, ProviderError> {
    let client = discovery_client()?;
    let request =
        apply_provider_auth(client.get(crate::provider::endpoint(base_url, "/models")), protocol, api_key)?;
    let response = provider_future_with(cancellation, request.send(), discovery_transport_error).await?;
    if !response.status().is_success() {
        return Err(discovery_http_error(response, api_key, cancellation).await);
    }
    let bytes = match read_bounded_body(response, MAX_DISCOVERY_BODY_BYTES + 1, Some(cancellation)).await {
        Ok(bytes) => bytes,
        Err(BoundedBodyReadError::Transport(error)) => return Err(discovery_transport_error(error)),
        Err(BoundedBodyReadError::Cancelled) => return Err(provider_cancelled_error()),
    };
    if bytes.len() > MAX_DISCOVERY_BODY_BYTES {
        return Err(ProviderError::new(ModelErrorKind::Other, "模型目录响应过大。"));
    }
    let body: Value = serde_json::from_slice(&bytes).map_err(|error| {
        let detail = bounded_provider_error_diagnostic(&error.to_string());
        ProviderError::new(ModelErrorKind::Other, format!("提供方未返回有效的模型目录（{detail}）。"))
    })?;
    read_listing(&body)
}

fn discovery_transport_error(error: reqwest::Error) -> ProviderError {
    ProviderError::new(
        ModelErrorKind::Temporary,
        format!("无法连接模型目录（{}）。", transport_error_source(error)),
    )
}

fn read_listing(body: &Value) -> Result<Vec<String>, ProviderError> {
    let Some(data) = body.get("data").and_then(Value::as_array) else {
        return Err(ProviderError::new(ModelErrorKind::Other, "提供方未返回 data 模型列表。"));
    };
    let mut models = BTreeSet::new();
    for entry in data {
        if let Some(id) = entry.get("id").and_then(Value::as_str)
            && validate_identifier(id, "model id").is_ok()
        {
            models.insert(id.to_string());
        }
    }
    Ok(models.into_iter().collect())
}

async fn discovery_http_error(
    response: reqwest::Response,
    api_key: &str,
    cancellation: &CancellationToken,
) -> ProviderError {
    let status = response.status().as_u16();
    let mut error = ProviderError::new(
        provider_error_kind_for_http_status(status),
        format!("获取模型失败：HTTP {status}。"),
    );
    match read_bounded_body(response, MAX_HTTP_ERROR_BODY_BYTES, Some(cancellation)).await {
        Ok(body) => apply_provider_error_body(&mut error, &body, api_key),
        Err(BoundedBodyReadError::Transport(read_error)) => {
            let _ =
                write!(error.message, " 无法读取提供方错误响应（{}）。", transport_error_source(read_error));
        }
        Err(BoundedBodyReadError::Cancelled) => return provider_cancelled_error(),
    }
    error
}
