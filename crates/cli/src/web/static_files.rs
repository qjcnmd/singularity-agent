//! 编译期嵌入的 production 前端；运行时不依赖仓库、Node 或外部静态目录。

use axum::body::Body;
use axum::http::{HeaderValue, Response, StatusCode, header};
use include_dir::{Dir, include_dir};

static WEB_DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/web/dist");

pub fn index() -> Response<Body> {
    embedded("index.html", false)
}

pub fn asset(path: &str) -> Response<Body> {
    embedded(&format!("assets/{path}"), true)
}

fn embedded(path: &str, immutable: bool) -> Response<Body> {
    let Some(file) = WEB_DIST.get_file(path) else {
        let mut response = Response::new(Body::from("not found"));
        *response.status_mut() = StatusCode::NOT_FOUND;
        return response;
    };
    let content_type = match path.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    };
    let cache = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-store"
    };
    let mut response = Response::new(Body::from(file.contents()));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response
}
