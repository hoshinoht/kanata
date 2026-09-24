use axum::http::StatusCode;
use kanata::core::{Extensions, Request as CoreRequest};
use serde_json::{Value, json};

use super::support::{EXTENSION_KEY, EXTENSIONS_JSON, chat_request, response_body, server};

#[tokio::test]
async fn defaults_and_one_sided_allowlists_deny_without_dispatch() {
    for (adapter, route, name) in [
        (&[][..], &[][..], "default"),
        (&[EXTENSION_KEY][..], &[][..], "adapter-only"),
        (&[][..], &[EXTENSION_KEY][..], "route-only"),
    ] {
        let (server, requests) = server(adapter, route, &[], 8192);
        let response = server
            .client_oneshot(chat_request(EXTENSIONS_JSON))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
        assert!(
            requests.lock().expect("lock").is_empty(),
            "{name} dispatched"
        );
    }
}

#[tokio::test]
async fn both_allowlists_relay_identical_extensions_to_request_and_context() {
    let (server, requests) = server(&[EXTENSION_KEY], &[EXTENSION_KEY], &[], 8192);
    let response = server
        .client_oneshot(chat_request(EXTENSIONS_JSON))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_body(response).await;
    assert!(
        !body
            .windows(b"extensions".len())
            .any(|window| window == b"extensions")
    );

    let routed = requests.lock().expect("lock").pop().expect("dispatch");
    let (context, request) = routed.into_parts();
    let CoreRequest::Chat(request) = request else {
        panic!("chat request");
    };
    let expected: Extensions = serde_json::from_str(EXTENSIONS_JSON).expect("extensions");
    assert_eq!(request.extensions, expected);
    assert_eq!(context.extensions, expected);
}

#[tokio::test]
async fn wildcard_unknown_and_malformed_keys_are_rejected_before_dispatch() {
    let cases = [
        (r#"{"bad":true}"#, "unknown"),
        (r#"{"io.kanata.*":true}"#, "wildcard"),
        ("not-json", "malformed"),
    ];
    for (extensions, name) in cases {
        let (server, requests) = server(&[EXTENSION_KEY], &[EXTENSION_KEY], &[], 8192);
        let response = server
            .client_oneshot(chat_request(extensions))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
        assert!(
            requests.lock().expect("lock").is_empty(),
            "{name} dispatched"
        );
    }
}

#[tokio::test]
async fn core_bounds_and_configured_smaller_byte_limit_reject_without_dispatch() {
    let mut too_many = serde_json::Map::new();
    for index in 0..17 {
        too_many.insert(format!("io.kanata.key{index}"), Value::Bool(true));
    }
    let too_many = serde_json::to_string(&Value::Object(too_many)).expect("json");
    let too_deep = r#"{"io.kanata.trace":{"a":{"b":{"c":{"d":{"e":true}}}}}}"#;
    let too_large = serde_json::to_string(&json!({
        "io.kanata.trace": "x".repeat(8192)
    }))
    .expect("json");
    for (extensions, name) in [
        (too_many.as_str(), "too-many"),
        (too_deep, "too-deep"),
        (too_large.as_str(), "too-large"),
    ] {
        let (server, requests) = server(&[EXTENSION_KEY], &[EXTENSION_KEY], &[], 8192);
        let response = server
            .client_oneshot(chat_request(extensions))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
        assert!(
            requests.lock().expect("lock").is_empty(),
            "{name} dispatched"
        );
    }

    let (server, requests) = server(&[EXTENSION_KEY], &[EXTENSION_KEY], &[], 8);
    let response = server
        .client_oneshot(chat_request(EXTENSIONS_JSON))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(requests.lock().expect("lock").is_empty());
}
