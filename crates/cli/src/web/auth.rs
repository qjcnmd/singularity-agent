//! DSH 式本机浏览器交接：进程 token 只在根路径交换，长期会话由签名 cookie 表达。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, header};
use cookie::time::{Duration, OffsetDateTime};
use cookie::{Cookie, CookieJar, Key, SameSite};

const SIGNING_KEY_FILE: &str = "browser-session.key";
const COOKIE_LIFETIME_SECONDS: i64 = 30 * 24 * 60 * 60;

#[derive(Clone)]
pub struct BrowserAuth {
    authority: String,
    origin: String,
    launch_token: String,
    cookie_name: String,
    key: Key,
}

impl BrowserAuth {
    pub fn open(home: &Path, authority: String) -> Result<Self, String> {
        let key = load_or_create_key(home)?;
        let mut launch_bytes = [0_u8; 32];
        getrandom::fill(&mut launch_bytes)
            .map_err(|error| format!("browser launch token generation failed: {error}"))?;
        let launch_token = hex(&launch_bytes);
        let cookie_name = format!("singularity-browser-{}", authority.replace(':', "-"));
        let origin = format!("http://{authority}");
        Ok(Self {
            authority,
            origin,
            launch_token,
            cookie_name,
            key,
        })
    }

    pub fn entry_url(&self) -> String {
        format!("{}/?token={}", self.origin, self.launch_token)
    }

    pub fn authority(&self) -> &str {
        &self.authority
    }

    pub fn exchange(&self, token: &str) -> Option<String> {
        if !constant_time_eq(token.as_bytes(), self.launch_token.as_bytes()) {
            return None;
        }
        let expires = now_unix().saturating_add(COOKIE_LIFETIME_SECONDS);
        let payload = format!("{}|{expires}", self.authority);
        let expires_at = OffsetDateTime::from_unix_timestamp(expires).ok()?;
        let cookie = Cookie::build((self.cookie_name.clone(), payload))
            .http_only(true)
            .same_site(SameSite::Strict)
            .path("/")
            .max_age(Duration::seconds(COOKIE_LIFETIME_SECONDS))
            .expires(expires_at)
            .build();
        let mut jar = CookieJar::new();
        jar.signed_mut(&self.key).add(cookie);
        jar.delta().next().map(ToString::to_string)
    }

    pub fn has_valid_cookie(&self, headers: &HeaderMap) -> bool {
        let Some(raw) = headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok())
        else {
            return false;
        };
        let mut jar = CookieJar::new();
        for pair in raw.split(';') {
            if let Ok(cookie) = Cookie::parse(pair.trim().to_string()) {
                jar.add_original(cookie.into_owned());
            }
        }
        let Some(cookie) = jar.signed(&self.key).get(&self.cookie_name) else {
            return false;
        };
        let Some((authority, expires)) = cookie.value().rsplit_once('|') else {
            return false;
        };
        authority == self.authority
            && expires
                .parse::<i64>()
                .is_ok_and(|expires| expires >= now_unix())
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

fn load_or_create_key(home: &Path) -> Result<Key, String> {
    singularity_core::create_owner_only_dir(home)?;
    let path = home.join(SIGNING_KEY_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => {
            singularity_core::ensure_owner_only_file(&path)?;
            bytes
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut bytes = vec![0_u8; 64];
            getrandom::fill(&mut bytes)
                .map_err(|error| format!("browser signing key generation failed: {error}"))?;
            singularity_core::atomic_replace_bytes(&path, &bytes)
                .map_err(|error| format!("browser signing key could not be persisted: {error}"))?;
            singularity_core::ensure_owner_only_file(&path)?;
            bytes
        }
        Err(error) => return Err(format!("browser signing key could not be read: {error}")),
    };
    if bytes.len() != 64 {
        return Err("browser signing key must contain exactly 64 bytes".to_string());
    }
    Ok(Key::from(&bytes))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn launch_exchange_is_clean_signed_authority_bound_and_persistent() {
        let home = tempfile::tempdir().expect("temp home");
        let auth = BrowserAuth::open(home.path(), "127.0.0.1:3080".to_string()).expect("auth");
        assert!(auth.entry_url().contains("?token="));
        assert_eq!(auth.origin, "http://127.0.0.1:3080");
        assert!(auth.exchange("invalid").is_none());
        let header = auth.exchange(&auth.launch_token).expect("exchange");
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("SameSite=Strict"));
        assert!(header.contains("Max-Age=2592000"));

        let cookie = header.split(';').next().expect("cookie pair");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(cookie).expect("cookie value"),
        );
        assert!(auth.has_valid_cookie(&headers));

        let reopened =
            BrowserAuth::open(home.path(), "127.0.0.1:3080".to_string()).expect("reopen");
        assert!(reopened.has_valid_cookie(&headers));
        let other =
            BrowserAuth::open(home.path(), "127.0.0.1:3081".to_string()).expect("other port");
        assert!(!other.has_valid_cookie(&headers));

        let expired = Cookie::build((
            auth.cookie_name.clone(),
            format!("{}|{}", auth.authority, now_unix() - 1),
        ))
        .path("/")
        .build();
        let mut expired_jar = CookieJar::new();
        expired_jar.signed_mut(&auth.key).add(expired);
        let expired_header = expired_jar
            .delta()
            .next()
            .expect("expired cookie")
            .to_string();
        let mut expired_headers = HeaderMap::new();
        expired_headers.insert(
            header::COOKIE,
            HeaderValue::from_str(expired_header.split(';').next().expect("cookie pair"))
                .expect("cookie value"),
        );
        assert!(!auth.has_valid_cookie(&expired_headers));
    }

    #[test]
    fn api_source_requires_current_host_origin_fetch_site_and_json() {
        let home = tempfile::tempdir().expect("temp home");
        let auth = BrowserAuth::open(home.path(), "127.0.0.1:3080".to_string()).expect("auth");
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
        assert!(auth.validate_api_source(&headers, true));

        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:3081"),
        );
        assert!(!auth.validate_api_source(&headers, true));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:3080"),
        );
        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!auth.validate_api_source(&headers, true));
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        assert!(!auth.validate_api_source(&headers, true));
    }
}
