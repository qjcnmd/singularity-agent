//! 本机工作台的 Host 与请求来源校验，不维护浏览器登录状态。

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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn api_source_requires_current_host_origin_fetch_site_and_json() {
        let origin = WebOrigin::new("127.0.0.1:3080".to_string());
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:3080"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:3080"),
        );
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        assert!(origin.validate_api_source(&headers, true));

        headers.remove(header::CONTENT_TYPE);
        assert!(origin.validate_api_source(&headers, false));
        assert!(!origin.validate_api_source(&headers, true));
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.remove(header::ORIGIN);
        assert!(!origin.validate_api_source(&headers, false));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:3080"),
        );
        headers.insert(
            header::HOST,
            HeaderValue::from_static("external.example:3080"),
        );
        assert!(!origin.validate_host(&headers));
        assert!(!origin.validate_api_source(&headers, false));
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:3080"));

        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:3081"),
        );
        assert!(!origin.validate_api_source(&headers, true));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:3080"),
        );
        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!origin.validate_api_source(&headers, true));
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        assert!(!origin.validate_api_source(&headers, true));
    }
}
