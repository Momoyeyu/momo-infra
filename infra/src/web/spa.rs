//! include_dir! 编译期内嵌 SPA（`infra/web/` 构建产物/占位页）。
//! 非 `/api` 的未知路径一律回退 `index.html`（前端路由）；
//! `/api/*` 未命中由 api 子树自己的 JSON 404 处理，不会落到这里。

use axum::body::Body;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use include_dir::{include_dir, Dir};

/// 内嵌目录：include_dir 内部路径恒以 `/` 分隔（Windows 亦如此），
/// 查找用字符串不做磁盘 IO，天然跨平台、无 `..` 逃逸面。
static SPA: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/web");

/// SPA fallback：内嵌命中按扩展名给 content-type 直出；
/// 未命中（前端路由路径）给 `index.html`。
pub async fn fallback(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty() {
        if let Some(file) = SPA.get_file(path) {
            return respond(file.path(), file.contents());
        }
    }
    index()
}

fn index() -> Response {
    match SPA.get_file("index.html") {
        Some(file) => respond(file.path(), file.contents()),
        // 内嵌目录在编译期固定；index.html 缺失属于打包事故。
        None => (
            StatusCode::NOT_FOUND,
            "index.html missing from embedded SPA",
        )
            .into_response(),
    }
}

fn respond(path: &std::path::Path, body: &'static [u8]) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type(path))
        .body(Body::from(body))
        .expect("static response is always constructible")
}

/// 按扩展名给 content-type；未识别的二进制资产退回 octet-stream。
/// `include_dir` 路径不含分隔歧义，`Path::extension` 在 Windows 上同样接受 `/`。
fn content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("webp") => "image/webp",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内嵌资产在编译期就位：占位页与样式都能命中。
    #[test]
    fn embedded_assets_present() {
        assert!(SPA.get_file("index.html").is_some());
        assert!(SPA.get_file("assets/style.css").is_some());
    }

    /// content-type 覆盖 SPA 常见扩展名。
    #[test]
    fn content_type_maps_common_extensions() {
        use std::path::Path;
        assert_eq!(
            content_type(Path::new("index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("assets/style.css")),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("app.mjs")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("blob.bin")),
            "application/octet-stream"
        );
    }
}
