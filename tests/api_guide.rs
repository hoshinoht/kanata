#[path = "support/gateway.rs"]
mod support;

use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

use support::{adapter_spec, capabilities, chat_outcome, config_with_public_routes, server_with};

fn request(method: Method, path: &str, key: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(key) = key {
        builder = builder.header("authorization", format!("Bearer {key}"));
    }
    builder.body(Body::empty()).expect("request")
}

fn server() -> kanata::server::TwoPlaneServer {
    let config = config_with_public_routes(&[("private-chat", "chat")]);
    server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities(&config, "vllm-private"),
            chat_outcome("unused"),
        )],
    )
    .0
}

#[tokio::test]
async fn guide_is_identical_static_html_on_both_listeners_without_auth() {
    let server = server();
    let mut expected = None;
    for public in [false, true] {
        for path in ["/v1", "/v1/", "/v1?key=QUERY_SECRET_MARKER"] {
            for key in [None, Some("test-key"), Some("INVALID_SECRET_MARKER")] {
                let request = request(Method::GET, path, key);
                let response = if public {
                    server.public_oneshot(request).await.expect("public")
                } else {
                    server.client_oneshot(request).await.expect("private")
                };
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response.headers()["content-type"],
                    "text/html; charset=utf-8"
                );
                assert_eq!(response.headers()["cache-control"], "no-store");
                assert_eq!(response.headers()["referrer-policy"], "no-referrer");
                assert_eq!(response.headers()["x-content-type-options"], "nosniff");
                assert_eq!(response.headers()["x-frame-options"], "DENY");
                let policy = response.headers()["content-security-policy"]
                    .to_str()
                    .unwrap()
                    .to_owned();
                let body = String::from_utf8(
                    to_bytes(response.into_body(), usize::MAX)
                        .await
                        .unwrap()
                        .to_vec(),
                )
                .unwrap();
                for secret in [
                    "QUERY_SECRET_MARKER",
                    "INVALID_SECRET_MARKER",
                    "test-key",
                    "private-chat",
                    "private-transcribe",
                    "vllm.invalid",
                    "{{SCRIPT}}",
                    "{{STYLE}}",
                    "{{FONT}}",
                    "{{FONT_LICENSE}}",
                    "{{LOGO}}",
                    "{{FAVICON}}",
                ] {
                    assert!(!body.contains(secret), "page contains {secret}");
                }
                for (tag, directive) in [("script", "script-src"), ("style", "style-src")] {
                    let content = body
                        .split_once(&format!("<{tag}>"))
                        .unwrap()
                        .1
                        .split_once(&format!("</{tag}>"))
                        .unwrap()
                        .0;
                    let hash = STANDARD.encode(Sha256::digest(content));
                    assert!(policy.contains(&format!("{directive} 'sha256-{hash}'")));
                }
                assert!(policy.contains("font-src data:"));
                assert!(policy.contains("img-src data:"));
                assert!(body.contains("data:image/svg+xml;base64,"));
                assert!(body.contains("data:font/woff2;base64,d09GMg"));
                assert!(body.contains("SIL OPEN FONT LICENSE"));
                assert!(policy.contains("connect-src 'self'"));
                assert!(policy.contains("form-action 'none'"));
                assert!(!policy.contains("unsafe-inline"));
                if let Some(expected) = &expected {
                    assert_eq!(&body, expected);
                } else {
                    expected = Some(body);
                }
            }
        }
    }
    for path in ["/v1", "/v1/"] {
        let response = server
            .public_oneshot(request(Method::HEAD, path, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn public_guide_exception_does_not_extend_to_apis_methods_or_similar_paths() {
    let server = server();
    for (method, path) in [
        (Method::POST, "/v1"),
        (Method::OPTIONS, "/v1/"),
        (Method::DELETE, "/v1"),
        (Method::GET, "/v1/models"),
        (Method::HEAD, "/v1/models"),
        (Method::POST, "/v1/chat/completions"),
        (Method::POST, "/v1/audio/transcriptions"),
        (Method::GET, "/v1/other"),
        (Method::GET, "/v1//"),
        (Method::GET, "/v1%2f"),
        (Method::GET, "/v1/models?key=test-key"),
        (Method::GET, "/live"),
        (Method::GET, "/"),
    ] {
        let response = server
            .public_oneshot(request(method, path, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    let response = server
        .admin_oneshot(request(Method::GET, "/v1", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn personalized_discovery_stays_authorized_and_uncacheable() {
    let server = server();
    for public in [false, true] {
        for key in [None, Some("wrong"), Some("test-key")] {
            let request = request(Method::GET, "/v1/models", key);
            let response = if public {
                server.public_oneshot(request).await.unwrap()
            } else {
                server.client_oneshot(request).await.unwrap()
            };
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert_eq!(response.headers()["vary"], "Authorization");
            if key != Some("test-key") {
                assert_eq!(
                    response.status(),
                    if public {
                        StatusCode::FORBIDDEN
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                );
                continue;
            }
            let body = support::response_json(response).await;
            let models = body["data"].as_array().unwrap();
            assert_eq!(models.len(), if public { 1 } else { 2 });
            assert_eq!(models[0]["id"], "private-chat");
            assert_eq!(
                models[0]["kanata"]["operations"],
                serde_json::json!(["chat"])
            );
            if public {
                assert!(models[0]["kanata"].get("admission").is_none());
            }
        }
    }
}
