use std::{sync::atomic::Ordering, time::Duration};

use kanata::{
    adapter::{Adapter, AdapterOutput, vllm::VllmAdapter},
    auth::{SecretResolutionError, SecretResolver},
    config::SecretReference,
    core::{
        EmbeddingRequest, ErrorKind, ModelAlias, Operation, ReasoningEffort,
        Request as CoreRequest, TrustZone,
    },
};
use serde_json::{Value, json};

use crate::support::{
    EMBEDDINGS_RESPONSE, INVALID_USAGE_RESPONSE, MockServer, TEXT_RESPONSE, UNEXPECTED_RESPONSE,
    adapter, config_at, config_for, config_text_at, config_with, error_kind, expected_capabilities,
    expected_text, fixture_finish_reason, load_config, routed, routed_transcription,
    routed_with_zone, text_request, tool_history_request, tool_request, transcription_request,
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
        (Operation::Chat, false, false, true),
    ] {
        let config = config_with("127.0.0.1:8000", operation, streaming, tools, secret_ref);
        assert!(
            VllmAdapter::new(&config.adapters()[0], config.timeouts(), config.limits()).is_err(),
            "accepted operation={operation:?}, streaming={streaming}, tools={tools}, secret={secret_ref}"
        );
    }
}

struct FixtureResolver;

impl SecretResolver for FixtureResolver {
    fn resolve(&self, reference: &SecretReference) -> Result<Vec<u8>, SecretResolutionError> {
        match reference {
            SecretReference::Env(name) if name == "VLLM_KEY" => Ok(b"fixture-vllm-key".to_vec()),
            _ => Err(SecretResolutionError),
        }
    }
}

#[test]
fn secret_reference_is_resolved_only_through_the_secret_aware_constructor() {
    let config = config_with("vllm.invalid", Operation::Chat, false, false, true);
    assert!(VllmAdapter::from_config(&config, "vllm-fixture", "vllm-chat").is_err());
    let adapter = VllmAdapter::from_config_with_secrets(
        &config,
        "vllm-fixture",
        "vllm-chat",
        &FixtureResolver,
    )
    .unwrap_or_else(|error| panic!("secret-aware adapter: {error:?}"));
    assert!(!format!("{adapter:?}").contains("fixture-vllm-key"));
}

#[tokio::test]
async fn external_loopback_http_upstream_receives_the_bearer_key() {
    let mut mock = MockServer::json(TEXT_RESPONSE).await;
    let config = config_at(
        &format!("http://{}/v1", mock.address),
        "external",
        Operation::Chat,
        false,
        false,
        true,
    );
    let adapter = VllmAdapter::from_config_with_secrets(
        &config,
        "vllm-fixture",
        "vllm-chat",
        &FixtureResolver,
    )
    .unwrap_or_else(|error| panic!("loopback adapter: {error:?}"));
    let response = crate::support::take_chat(
        adapter
            .execute(routed_with_zone(
                &config,
                text_request(),
                TrustZone::External,
            ))
            .await,
    );
    assert_eq!(response.message.content, expected_text());
    mock.finish().await;

    let record = mock
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request lock"))[0]
        .clone();
    assert_eq!(record.headers["authorization"], "Bearer fixture-vllm-key");
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

fn effort_request(effort: Option<ReasoningEffort>, enable_thinking: Option<bool>) -> CoreRequest {
    let CoreRequest::Chat(mut request) = text_request() else {
        panic!("chat request")
    };
    request.options.reasoning_effort = effort;
    request.options.enable_thinking = enable_thinking;
    CoreRequest::Chat(request)
}

#[tokio::test]
async fn reasoning_effort_is_forwarded_only_with_reasoning_control() {
    let mock = MockServer::json(TEXT_RESPONSE).await;
    let plain = config_for(&mock.address);
    assert_eq!(
        error_kind(
            adapter(&plain)
                .execute(routed(
                    &plain,
                    effort_request(Some(ReasoningEffort::High), None)
                ))
                .await
        ),
        ErrorKind::UnsupportedOperation
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
    drop(mock);

    for (effort, expected) in [(Some(ReasoningEffort::High), Some("high")), (None, None)] {
        let mut mock = MockServer::json(TEXT_RESPONSE).await;
        let config = load_config(
            &config_text_at(
                &format!("http://{}/v1", mock.address),
                "local",
                Operation::Chat,
                false,
                false,
                false,
            )
            .replacen(
                "function_tools = false\n",
                "function_tools = false\nreasoning_control = true\n",
                1,
            ),
        );
        let adapter = adapter(&config);
        assert!(adapter.capabilities().reasoning_control);
        // The two thinking switches are alternatives; both together never reach the upstream.
        assert_eq!(
            error_kind(
                adapter
                    .execute(routed(
                        &config,
                        effort_request(Some(ReasoningEffort::High), Some(false))
                    ))
                    .await
            ),
            ErrorKind::InvalidRequest
        );
        crate::support::take_chat(
            adapter
                .execute(routed(&config, effort_request(effort, None)))
                .await,
        );
        mock.finish().await;
        let records = mock
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("request lock"))
            .clone();
        assert_eq!(records.len(), 1);
        let body = decode_request(&records[0].body);
        assert_eq!(
            body.get("reasoning_effort").and_then(Value::as_str),
            expected
        );
        assert!(body.get("chat_template_kwargs").is_none());
    }
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
async fn stop_or_length_stop_without_content_is_an_empty_reply() {
    for (finish, expected) in [
        ("length", kanata::core::FinishReason::Length),
        ("stop", kanata::core::FinishReason::Stop),
    ] {
        let body = format!(
            r#"{{"id":"id","object":"chat.completion","created":1700000000,"model":"served-checkpoint","choices":[{{"index":0,"message":{{"role":"assistant","content":null,"reasoning_content":"hidden"}},"finish_reason":"{finish}"}}]}}"#
        );
        let mut mock = MockServer::json(&body).await;
        let config = config_for(&mock.address);
        let adapter = adapter(&config);
        let Ok(AdapterOutput::Complete(kanata::core::Response::Chat(response))) =
            adapter.execute(routed(&config, text_request())).await
        else {
            panic!("{finish} stop was not a chat response")
        };
        assert_eq!(response.finish_reason, expected);
        assert!(response.message.content.is_empty());
        mock.finish().await;
    }
}

#[tokio::test]
async fn malformed_or_unexpected_success_payloads_are_redacted_failures() {
    let cases = [
        INVALID_USAGE_RESPONSE,
        UNEXPECTED_RESPONSE,
        r#"{"id":"id","object":"chat.completion","created":1700000000,"model":"served-checkpoint","choices":[{"index":0,"message":{"role":"assistant","content":null,"refusal":"I can't help with that."},"finish_reason":"stop"}]}"#,
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

fn embedding_request(dimensions: Option<usize>) -> CoreRequest {
    CoreRequest::Embeddings(EmbeddingRequest {
        model: ModelAlias("vllm-public".into()),
        input: vec!["alpha".into(), "beta".into()],
        dimensions,
    })
}

#[tokio::test]
async fn embeddings_post_the_openai_shape_with_the_configured_credential() {
    for (dimensions, secret_ref) in [(None, false), (Some(3), true)] {
        let mut mock = MockServer::json(EMBEDDINGS_RESPONSE).await;
        let (zone, trust_zone) = if secret_ref {
            ("external", TrustZone::External)
        } else {
            ("local", TrustZone::Local)
        };
        let config = config_at(
            &format!("http://{}/v1", mock.address),
            zone,
            Operation::Embeddings,
            false,
            false,
            secret_ref,
        );
        let adapter = VllmAdapter::from_config_with_secrets(
            &config,
            "vllm-fixture",
            "vllm-chat",
            &FixtureResolver,
        )
        .unwrap_or_else(|error| panic!("embeddings adapter: {error:?}"));
        let request = embedding_request(dimensions);
        let Ok(AdapterOutput::Complete(kanata::core::Response::Embeddings(response))) = adapter
            .execute(routed_with_zone(&config, request, trust_zone))
            .await
        else {
            panic!("embeddings response for {dimensions:?}")
        };
        assert_eq!(response.model, ModelAlias("vllm-public".into()));
        assert_eq!(
            response.vectors,
            vec![vec![0.1, 0.2, 0.3], vec![0.4, 0.5, 0.6]]
        );
        assert_eq!(
            response
                .usage
                .map(|usage| (usage.input_tokens, usage.total_tokens)),
            Some((4, 4))
        );
        mock.finish().await;

        let record = mock
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("request lock"))[0]
            .clone();
        assert_eq!(record.path, "/v1/embeddings");
        assert_eq!(
            record.headers.get("authorization").map(String::as_str),
            secret_ref.then_some("Bearer fixture-vllm-key")
        );
        let mut expected = json!({
            "model": "served-checkpoint-alias",
            "input": ["alpha", "beta"],
            "encoding_format": "float",
        });
        if let Some(dimensions) = dimensions {
            expected["dimensions"] = json!(dimensions);
        }
        assert_eq!(decode_request(&record.body), expected);
    }
}

#[tokio::test]
async fn embeddings_failures_map_to_gateway_errors() {
    let item = |index: u8, embedding: &str| {
        format!(r#"{{"object":"embedding","index":{index},"embedding":{embedding}}}"#)
    };
    let list = |data: &[String]| format!(r#"{{"object":"list","data":[{}]}}"#, data.join(","));
    let wrong_object = list(&[item(0, "[0.1]"), item(1, "[0.2]")]).replace("list", "dict");
    let one_vector = list(&[item(0, "[0.1]")]);
    let duplicate_index = list(&[item(0, "[0.1]"), item(0, "[0.2]")]);
    let ragged = list(&[item(0, "[0.1]"), item(1, "[0.1,0.2]")]);
    let inconsistent_usage =
        EMBEDDINGS_RESPONSE.replace("\"total_tokens\": 4", "\"total_tokens\": 5");
    assert_ne!(inconsistent_usage, EMBEDDINGS_RESPONSE);
    let error = r#"{"detail":"secret-marker"}"#;
    for (status, body, dimensions, expected) in [
        (400, error, None, ErrorKind::InvalidRequest),
        (404, error, None, ErrorKind::NotFound),
        (429, error, None, ErrorKind::RateLimited),
        (500, error, None, ErrorKind::UpstreamFailure),
        (503, error, None, ErrorKind::UpstreamUnavailable),
        (200, "not json", None, ErrorKind::UpstreamFailure),
        (200, &wrong_object, None, ErrorKind::UpstreamFailure),
        (200, &one_vector, None, ErrorKind::UpstreamFailure),
        (200, &duplicate_index, None, ErrorKind::UpstreamFailure),
        (200, &ragged, None, ErrorKind::UpstreamFailure),
        (200, &inconsistent_usage, None, ErrorKind::UpstreamFailure),
        (
            200,
            EMBEDDINGS_RESPONSE,
            Some(2),
            ErrorKind::UpstreamFailure,
        ),
    ] {
        let mut mock = MockServer::response(status, "application/json", body).await;
        let config = config_with(&mock.address, Operation::Embeddings, false, false, false);
        let request = routed(&config, embedding_request(dimensions));
        assert_eq!(
            error_kind(adapter(&config).execute(request).await),
            expected,
            "{status} {body}"
        );
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1, "{status} {body}");
        mock.finish().await;
    }
}
