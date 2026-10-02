use std::sync::OnceLock;

use axum::{
    http::{HeaderValue, Method, header},
    response::{Html, IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

const TEMPLATE: &str = include_str!("guide/index.html");
const STYLE: &str = include_str!("guide/style.css");
const SCRIPT: &str = include_str!("guide/app.js");
const EXAMPLES: &str = include_str!("guide/examples.js");
const HIGHLIGHT: &str = include_str!("guide/highlight.js");
const FONT: &[u8] = include_bytes!("guide/fonts/MapleMono-Regular.woff2");
const FONT_LICENSE: &str = include_str!("guide/fonts/OFL.txt");

pub(super) fn is_page(method: &Method, path: &str) -> bool {
    matches!(*method, Method::GET | Method::HEAD) && matches!(path, "/v1" | "/v1/")
}

pub(super) async fn page() -> Response {
    static PAGE: OnceLock<(String, HeaderValue)> = OnceLock::new();
    let (html, policy) = PAGE.get_or_init(|| {
        let style = STYLE.replace("{{FONT}}", &STANDARD.encode(FONT));
        let script = format!("(() => {{\n{HIGHLIGHT}\n{EXAMPLES}\n{SCRIPT}\n}})();");
        let html = TEMPLATE
            .replace("{{STYLE}}", &style)
            .replace("{{SCRIPT}}", &script)
            .replace("{{FONT_LICENSE}}", FONT_LICENSE);
        let script_hash = STANDARD.encode(Sha256::digest(&script));
        let style_hash = STANDARD.encode(Sha256::digest(&style));
        let policy = format!(
            "default-src 'none'; script-src 'sha256-{script_hash}'; style-src 'sha256-{style_hash}'; font-src data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
        );
        (html, HeaderValue::from_str(&policy).expect("static guide policy"))
    });
    let mut response = Html(html.as_str()).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, policy.clone());
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

pub(super) async fn protect_discovery(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let discovery = matches!(request.uri().path(), "/v1" | "/v1/" | "/v1/models");
    let mut response = next.run(request).await;
    if discovery {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
            .headers_mut()
            .append(header::VARY, HeaderValue::from_static("Authorization"));
    }
    response
}
