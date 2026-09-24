use std::{sync::atomic::Ordering, time::Duration};

use kanata::{
    adapter::{Adapter, AdapterOutput, vllm::VllmAdapter},
    core::{ErrorKind, ModelAlias, Operation, Request as CoreRequest, TrustZone},
};
use serde_json::{Value, json};

use crate::support::{
    INVALID_USAGE_RESPONSE, MockServer, TEXT_RESPONSE, UNEXPECTED_RESPONSE, adapter, config_for,
    config_with, error_kind, expected_capabilities, expected_text, fixture_finish_reason, routed,
    routed_transcription, routed_with_zone, text_request, tool_history_request, tool_request,
    transcription_request,
};

fn decode_request(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or_else(|_| panic!("request JSON"))
}

#[tokio::test]
async fn adapter_declares_only_fixture_backed_nonstream_text_chat() {
    let config = config_for("127.0.0.1:8000");
    let adapter = adapter(&config);
    assert_eq!(adapter.id(), "vllm-fixture");
    assert_eq!(adapter.configured_id(), "vllm-fixture");
    assert_eq!(adapter.capabilities(), &expected_capabilities());
    assert!(!adapter.capabilities().streaming_chat);
    assert!(!adapter.capabilities().function_tools);
    assert_eq!(
        VllmAdapter::from_config(&config, "vllm-fixture", "vllm-chat")
            .unwrap_or_else(|_| panic!("configured adapter"))
            .id(),
        "vllm-fixture"
    );
}

#[tokio::test]
async fn constructor_rejects_unimplemented_capabilities_and_secret_references() {
    for (operation, streaming, tools, secret_ref) in [
        (Operation::Transcription, false, false, false),
        (Operation::Chat, true, false, false),
        (Operation::Chat, false, true, false),
        (Operation::Chat, false, false, true),
    ] {
        let config = config_with("127.0.0.1:8000", operation, streaming, tools, secret_ref);
        assert!(
            VllmAdapter::new(&config.adapters()[0], config.timeouts(), config.limits()).is_err(),
            "accepted operation={operation:?}, streaming={streaming}, tools={tools}, secret={secret_ref}"
        );
    }
}

#[tokio::test]
async fn nonstream_text_posts_explicit_upstream_model_and_returns_public_alias() {
    let mut mock = MockServer::json(TEXT_RESPONSE).await;
    let config = config_for(&mock.address);
    let adapter = adapter(&config);
    let response =
        crate::support::take_chat(adapter.execute(routed(&config, text_request())).await);

    assert_eq!(response.model, ModelAlias("vllm-public".into()));
    assert_eq!(response.finish_reason, fixture_finish_reason());
    assert_eq!(
        response.usage.as_ref().map(|usage| usage.input_tokens),
        Some(11)
    );
    assert_eq!(
        response.usage.as_ref().map(|usage| usage.output_tokens),
        Some(7)
    );
    assert_eq!(
        response.usage.as_ref().map(|usage| usage.total_tokens),
        Some(18)
    );
    assert_eq!(response.message.content, expected_text());
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    mock.finish().await;

    let record = mock
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request lock"))[0]
        .clone();
    assert_eq!(record.path, "/v1/chat/completions");
    assert_eq!(record.headers["accept"], "application/json");
    assert!(!record.headers.contains_key("authorization"));
    let body = decode_request(&record.body);
    assert_eq!(body["model"], "served-checkpoint-alias");
    assert_eq!(body["stream"], false);
    assert_eq!(
        body["messages"],
        json!([{"role":"user", "content":"Say hello"}])
    );
    assert_eq!(body.as_object().map(|object| object.len()), Some(3));
}

#[tokio::test]
async fn unsupported_operations_and_features_are_rejected_before_dispatch() {
    let cases = [
        (
            "transcription",
            routed_transcription(transcription_request()),
        ),
        ("streaming", {
            let CoreRequest::Chat(mut request) = text_request() else {
                panic!("chat request")
            };
            request.stream = true;
            routed(&config_for("127.0.0.1:8000"), CoreRequest::Chat(request))
        }),
        (
            "function tools",
            routed(&config_for("127.0.0.1:8000"), tool_request()),
        ),
        (
            "tool history",
            routed(&config_for("127.0.0.1:8000"), tool_history_request()),
        ),
    ];

    for (name, request) in cases {
        let mock = MockServer::json(TEXT_RESPONSE).await;
        let config = config_for(&mock.address);
        let adapter = adapter(&config);
        assert_eq!(
            error_kind(adapter.execute(request).await),
            ErrorKind::UnsupportedOperation,
            "{name}"
        );
        assert_eq!(mock.calls.load(Ordering::SeqCst), 0, "{name}");
        drop(mock);
    }
}

#[tokio::test]
async fn context_trust_mismatch_is_rejected_before_dispatch() {
    let mock = MockServer::json(TEXT_RESPONSE).await;
    let config = config_for(&mock.address);
    let adapter = adapter(&config);
    let request = routed_with_zone(&config, text_request(), TrustZone::PrivateNetwork);
    assert_eq!(
        error_kind(adapter.execute(request).await),
        ErrorKind::InvalidRequest
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn length_stop_without_content_is_an_empty_reply() {
    let body = r#"{"id":"id","object":"chat.completion","created":1700000000,"model":"served-checkpoint","choices":[{"index":0,"message":{"role":"assistant","content":null,"reasoning_content":"hidden"},"finish_reason":"length"}]}"#;
    let mut mock = MockServer::json(body).await;
    let config = config_for(&mock.address);
    let adapter = adapter(&config);
    let Ok(AdapterOutput::Complete(kanata::core::Response::Chat(response))) =
        adapter.execute(routed(&config, text_request())).await
    else {
        panic!("length stop was not a chat response")
    };
    assert_eq!(response.finish_reason, kanata::core::FinishReason::Length);
    assert!(response.message.content.is_empty());
    mock.finish().await;
}

#[tokio::test]
async fn malformed_or_unexpected_success_payloads_are_redacted_failures() {
    let cases = [
        INVALID_USAGE_RESPONSE,
        UNEXPECTED_RESPONSE,
        r#"{"id":"id","object":"chat.completion","created":1700000000,"model":"served-checkpoint","choices":[{"index":0,"message":{"role":"assistant","content":null},"finish_reason":"stop"}]}"#,
        r#"{"id":"id","object":"chat.completion","created":1700000000,"model":"","choices":[{"index":0,"message":{"role":"assistant","content":"text"},"finish_reason":"stop"}]}"#,
    ];
    for body in cases {
        let mut mock = MockServer::json(body).await;
        let config = config_for(&mock.address);
        let adapter = adapter(&config);
        assert_eq!(
            error_kind(adapter.execute(routed(&config, text_request())).await),
            ErrorKind::UpstreamFailure
        );
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
        mock.finish().await;
    }
}

#[tokio::test]
async fn status_errors_are_normalized_redacted_and_never_retried() {
    let cases = [
        (400, ErrorKind::InvalidRequest),
        (401, ErrorKind::UpstreamUnavailable),
        (404, ErrorKind::NotFound),
        (429, ErrorKind::RateLimited),
        (500, ErrorKind::UpstreamFailure),
        (503, ErrorKind::UpstreamUnavailable),
    ];
    for (status, expected) in cases {
        let mut mock = MockServer::status(status, r#"{"detail":"secret-marker"}"#).await;
        let config = config_for(&mock.address);
        let adapter = adapter(&config);
        let error = match adapter.execute(routed(&config, text_request())).await {
            Err(error) => error,
            Ok(AdapterOutput::Complete(_)) | Ok(AdapterOutput::Events(_)) => {
                panic!("unexpected upstream success")
            }
        };
        assert_eq!(error.kind, expected, "status {status}");
        assert!(!format!("{error:?}").contains("secret-marker"));
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1, "status {status}");
        mock.finish().await;
    }
}

#[tokio::test]
async fn dropping_inflight_nonstream_request_cancels_the_upstream_body() {
    let mut mock = MockServer::incomplete_body().await;
    let config = config_for(&mock.address);
    let adapter = adapter(&config);
    let request = routed(&config, text_request());
    let task = tokio::spawn(async move { adapter.execute(request).await });

    mock.wait_for_request().await;
    mock.wait_for_response().await;
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(2), mock.wait_for_client_close())
        .await
        .unwrap_or_else(|_| panic!("upstream connection remained open after cancellation"));
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    mock.finish().await;
}
