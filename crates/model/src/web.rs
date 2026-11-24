use std::fmt;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use crate::error::{ModelErrorKind, ProviderError, bounded_provider_error_diagnostic};
use crate::transport::http::{
    BoundedBodyReadError, client_builder, provider_cancelled_error, provider_future,
    provider_transport_error, read_bounded_body, transport_error_source,
};

pub const DEFAULT_WEB_SEARCH_BASE_URL: &str = "https://api.deepseek.com";

const MAX_FETCH_URL_CHARS: usize = 8192;
const WEB_CONNECT_TIMEOUT_SECONDS: u64 = 15;
const WEB_READ_TIMEOUT_SECONDS: u64 = 60;
const MAX_WEB_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_WEB_ERROR_BYTES: usize = 64 * 1024;

pub fn validate_fetch_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    let count = trimmed.chars().count();
    if count == 0 || count > MAX_FETCH_URL_CHARS {
        return Err(format!("url must be between 1 and {MAX_FETCH_URL_CHARS} characters"));
    }
    if trimmed.chars().any(char::is_control) {
        return Err("url must not contain control characters".into());
    }
    let parsed = reqwest::Url::parse(trimmed).map_err(|error| format!("invalid url: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("url must be an http or https URL with a host".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("url must not contain a username or password".into());
    }
    Ok(trimmed.to_string())
}

#[derive(Clone)]
pub struct WebSearchAccess {
    base_url: String,
    api_key: String,
}

impl fmt::Debug for WebSearchAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebSearchAccess")
            .field("base_url", &self.base_url)
            .field("api_key", &"[redacted]")
            .finish()
    }
}

impl WebSearchAccess {
    pub(crate) fn new(base_url: String, api_key: String) -> Self {
        Self { base_url, api_key }
    }

    pub async fn post_json<Req: Serialize, Res: DeserializeOwned>(
        &self,
        path: &str,
        body: &Req,
        cancellation: &CancellationToken,
    ) -> Result<Res, ProviderError> {
        let client = web_client()?;
        let url = format!("{}{path}", self.base_url.trim_end_matches('/'));
        let request = client
            .post(url)
            .bearer_auth(&self.api_key)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(body);
        let response = provider_future(cancellation, request.send()).await?;
        let status = response.status();
        let limit = if status.is_success() {
            MAX_WEB_RESPONSE_BYTES + 1
        } else {
            MAX_WEB_ERROR_BYTES
        };
        let bytes = match read_bounded_body(response, limit, Some(cancellation)).await {
            Ok(bytes) => bytes,
            Err(BoundedBodyReadError::Cancelled) => return Err(provider_cancelled_error()),
            Err(BoundedBodyReadError::Transport(error)) => return Err(provider_transport_error(error)),
        };
        if !status.is_success() {
            let detail = String::from_utf8_lossy(&bytes);
            let sanitized = redact_web_diagnostic(detail.trim(), &self.api_key);
            let message = bounded_provider_error_diagnostic(&sanitized);
            let suffix = if message.is_empty() {
                String::new()
            } else {
                format!(": {message}")
            };
            return Err(ProviderError::new(
                ModelErrorKind::Other,
                format!("web tool request failed (HTTP {}){suffix}", status.as_u16()),
            ));
        }
        if bytes.len() > MAX_WEB_RESPONSE_BYTES {
            return Err(ProviderError::new(
                ModelErrorKind::Other,
                format!("web tool response exceeded {} MiB limit", MAX_WEB_RESPONSE_BYTES / (1024 * 1024)),
            ));
        }
        serde_json::from_slice(&bytes).map_err(|error| {
            ProviderError::new(ModelErrorKind::Other, format!("invalid web tool JSON response: {error}"))
        })
    }
}

fn web_client() -> Result<reqwest::Client, ProviderError> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(WEB_CONNECT_TIMEOUT_SECONDS))
        .read_timeout(Duration::from_secs(WEB_READ_TIMEOUT_SECONDS))
        .user_agent(app_core::identity::USER_AGENT)
        .build()
        .map_err(|error| {
            ProviderError::new(
                ModelErrorKind::Other,
                format!("web client initialization failed: {}", transport_error_source(error)),
            )
        })?;
    Ok(CLIENT.get_or_init(|| client).clone())
}

fn redact_web_diagnostic(text: &str, api_key: &str) -> String {
    let mut cleaned = if api_key.is_empty() {
        text.to_string()
    } else {
        text.replace(api_key, "[redacted]")
    };
    let lower = cleaned.to_ascii_lowercase();
    if let Some(pos) = lower.find("bearer ") {
        let token_start = pos + "bearer ".len();
        let token_end = cleaned[token_start..]
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | ',' | '}' | ']'))
            .map_or(cleaned.len(), |offset| token_start + offset);
        if token_end > token_start {
            cleaned.replace_range(token_start..token_end, "[redacted]");
        }
    }
    cleaned
}
