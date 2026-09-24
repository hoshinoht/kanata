use axum::http::StatusCode;
use kanata::core::{ErrorKind, TimeoutPhase};
use serde_json::json;

use crate::support::{
    Outcome, adapter_spec, capabilities, chat_request, chat_request_with, config, recorded_len,
    response_body, response_json, server_with, transcription_request,
};

const BODY: &str =
    r#"{"model":"private-chat","messages":[{"role":"user","content":"REQUEST_MARKER"}]}"#;

fn error_server(
    kind: ErrorKind,
) -> (
    kanata::server::TwoPlaneServer,
    std::sync::Arc<std::sync::Mutex<Vec<kanata::core::RoutedRequest>>>,
) {
    let config = config();
    let capabilities = capabilities(&config, "vllm-private");
    server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities,
            Outcome::Error(kind),
        )],
    )
}

#[tokio::test]
async fn adapter_error_kinds_use_core_status_code_type_and_static_message() {
    let cases = [
        (
            ErrorKind::RateLimited,
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_exceeded",
            "rate_limit_error",
            "Rate limit exceeded",
        ),
        (
            ErrorKind::Timeout {
                phase: TimeoutPhase::FirstByte,
            },
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "api_error",
            "Upstream timeout",
        ),
        (
            ErrorKind::UpstreamUnavailable,
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_unavailable",
            "api_error",
            "Upstream unavailable",
        ),
        (
            ErrorKind::UpstreamFailure,
            StatusCode::BAD_GATEWAY,
            "upstream_failure",
            "api_error",
            "Upstream failure",
        ),
        (
            ErrorKind::Internal,
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "api_error",
            "Internal server error",
        ),
        (
            ErrorKind::UnsupportedOperation,
            StatusCode::BAD_REQUEST,
            "unsupported_operation",
            "invalid_request_error",
            "Unsupported operation",
        ),
    ];
    for (kind, status, code, error_type, message) in cases {
        let (server, requests) = error_server(kind);
        let response = server
            .client_oneshot(chat_request(BODY))
            .await
            .expect("response");
        assert_eq!(response.status(), status, "{kind:?}");
        let value = response_json(response).await;
        assert_eq!(
            value,
            json!({
                "error": {
                    "message": message,
                    "type": error_type,
                    "param": null,
                    "code": code
                }
            }),
            "{kind:?}"
        );
        assert!(!value.to_string().contains("REQUEST_MARKER"));
        assert_eq!(recorded_len(&requests), 1, "adapter errors must not retry");
    }
}

#[tokio::test]
async fn invalid_adapter_responses_are_sanitized_as_bad_gateway() {
    for outcome in [
        Outcome::WrongModel("UPSTREAM_MODEL_MARKER".into()),
        Outcome::WrongResponse,
        Outcome::InvalidAssistant,
    ] {
        let config = config();
        let capabilities = capabilities(&config, "vllm-private");
        let (server, requests) = server_with(
            &config,
            vec![adapter_spec("vllm-private", capabilities, outcome)],
        );
        let response = server
            .client_oneshot(chat_request(BODY))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = response_body(response).await;
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("upstream_failure"));
        assert!(!body.contains("UPSTREAM_"));
        assert_eq!(recorded_len(&requests), 1);
    }
}

#[tokio::test]
async fn transcription_adapter_errors_keep_openai_error_compatibility() {
    let (server, requests) = error_server(ErrorKind::RateLimited);
    let response = server
        .client_oneshot(transcription_request("private-transcribe"))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let value = response_json(response).await;
    assert_eq!(value["error"]["code"], "rate_limit_exceeded");
    assert_eq!(value["error"]["type"], "rate_limit_error");
    assert_eq!(recorded_len(&requests), 1);
}

#[tokio::test]
async fn authentication_and_permission_failures_remain_uniform_and_pre_dispatch() {
    let (server, requests) = error_server(ErrorKind::Internal);
    for authorization in [None, Some("Bearer wrong-key")] {
        let response = server
            .client_oneshot(chat_request_with(
                BODY,
                Some("application/json"),
                authorization,
                &[],
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let value = response_json(response).await;
        assert_eq!(value["error"]["code"], "invalid_api_key");
        assert_eq!(value["error"]["type"], "authentication_error");
    }

    let forbidden_body = r#"{"model":"private-transcribe","messages":[{"role":"user","content":"FORBIDDEN_MARKER"}]}"#;
    let response = server
        .client_oneshot(chat_request(forbidden_body))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let value = response_json(response).await;
    assert_eq!(value["error"]["code"], "permission_denied");
    assert_eq!(value["error"]["type"], "permission_error");
    assert!(!value.to_string().contains("FORBIDDEN_MARKER"));
    assert_eq!(recorded_len(&requests), 0);
}

#[tokio::test]
async fn unsafe_request_ids_reject_before_dispatch_without_echo() {
    let (server, requests) = error_server(ErrorKind::Internal);
    let response = server
        .client_oneshot(chat_request_with(
            BODY,
            Some("application/json"),
            Some("Bearer test-key"),
            &["bad/id"],
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_body(response).await;
    assert!(!String::from_utf8_lossy(&body).contains("bad/id"));
    assert_eq!(recorded_len(&requests), 0);

    let (server, requests) = error_server(ErrorKind::Internal);
    let too_long = "x".repeat(129);
    let response = server
        .client_oneshot(chat_request_with(
            BODY,
            Some("application/json"),
            Some("Bearer test-key"),
            &[too_long.as_str()],
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(recorded_len(&requests), 0);

    let (server, requests) = error_server(ErrorKind::Internal);
    let response = server
        .client_oneshot(chat_request_with(
            BODY,
            Some("application/json"),
            Some("Bearer test-key"),
            &["one", "two"],
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(recorded_len(&requests), 0);
}
