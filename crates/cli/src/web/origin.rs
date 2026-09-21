//! 本机工作台的 Host 和请求来源校验；它不维护浏览器登录状态。

use axum::http::{HeaderMap, header};

pub struct WebOrigin {
    authority: String,
    origin: String,
}

impl WebOrigin {
    pub fn new(authority: String) -> Self {
        let origin = format!("http://{authority}");
        Self { authority, origin }
    }

    pub fn entry_url(&self) -> String {
        format!("{}/", self.origin)
    }

    pub fn authority(&self) -> &str {
        &self.authority
    }

    pub fn validate_host(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            == Some(self.authority.as_str())
    }

    pub fn validate_api_source(&self, headers: &HeaderMap, require_json: bool) -> bool {
        // 任何网页都能访问本机端口，所以要确认请求真的来自工作台页面。
        if !self.validate_host(headers)
            || headers
                .get(header::ORIGIN)
                .and_then(|value| value.to_str().ok())
                != Some(self.origin.as_str())
        {
            return false;
        }
        if headers
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value != "same-origin" && value != "none")
        {
            return false;
        }
        !require_json
            || headers
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| {
                    value
                        .split(';')
                        .next()
                        .is_some_and(|kind| kind.trim() == "application/json")
                })
    }
}
