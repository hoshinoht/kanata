use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::StreamExt;
use http::StatusCode;
use kanata::{
    adapter::{Adapter, AdapterOutput},
    config::{self, ValidatedConfig},
    core::{ErrorKind, FinishReason, NormalizedEvent, Request as CoreRequest, ResponseFormat},
};
use serde_json::Value;

use crate::support::{
    MockServer, ResponseSpec, TEXT_RESPONSE, adapter, error_kind, routed, take_chat, text_request,
    text_request_with_stream,
};

static NEXT_CONFIG: AtomicUsize = AtomicUsize::new(0);

fn apple_config(address: &str) -> ValidatedConfig {
    let contents = include_str!("../fixtures/config/example.toml")
        .replace(
            "http://ollama.invalid:11434",
            &format!("http://{address}/v1"),
        )
        .replacen("kind = \"ollama\"", "kind = \"apple_fm\"", 1)
        .replacen(
            "function_tools = true\n",
            "function_tools = false\nstructured_output = true\nsampling_controls = true\n",
            1,
        )
        .replacen(
            "requires_function_tools = true",
            "requires_function_tools = false",
            1,
        );
    let path = std::env::temp_dir().join(format!(
        "kanata-apple-fm-{}-{}.toml",
        std::process::id(),
        NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, contents).expect("config writes");
    let result = config::load(&path);
    let _ = std::fs::remove_file(path);
    result.expect("apple_fm config loads")
}

fn error_spec(message: &str) -> ResponseSpec {
    ResponseSpec {
        body: format!(
            r#"{{"error":{{"message":"{message}","type":"server_error","code":"500"}}}}"#
        )
        .into_bytes(),
        ..ResponseSpec::status(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

#[tokio::test]
async fn output_cap_is_sent_as_max_completion_tokens() {
    let mut mock = MockServer::once(ResponseSpec::json(TEXT_RESPONSE)).await;
    let config = apple_config(&mock.address);
    let CoreRequest::Chat(mut chat) = text_request() else {
        unreachable!()
    };
    chat.options.max_output_tokens = Some(64);
    take_chat(
        adapter(&config)
            .execute(routed(&config, CoreRequest::Chat(chat)))
            .await,
    );
    let (body, host): (Value, String) = {
        let requests = mock.requests.lock().expect("requests");
        (
            serde_json::from_slice(&requests[0].body).expect("request json"),
            requests[0].headers.get("host").cloned().unwrap_or_default(),
        )
    };
    // fm serve only accepts loopback Host headers.
    let port = mock.address.rsplit(':').next().expect("port");
    assert_eq!(host, format!("localhost:{port}"));
    assert_eq!(body["max_completion_tokens"], 64);
    assert!(body.get("max_tokens").is_none());
    mock.finish().await;
}

#[tokio::test]
async fn json_object_is_rejected_before_the_upstream_call() {
    let mock = MockServer::once(ResponseSpec::json(TEXT_RESPONSE)).await;
    let config = apple_config(&mock.address);
    let CoreRequest::Chat(mut chat) = text_request() else {
        unreachable!()
    };
    chat.options.response_format = Some(ResponseFormat::JsonObject);
    let result = adapter(&config)
        .execute(routed(&config, CoreRequest::Chat(chat)))
        .await;
    assert_eq!(error_kind(result), ErrorKind::InvalidRequest);
    assert!(mock.requests.lock().expect("requests").is_empty());
}

#[tokio::test]
async fn known_fm_errors_map_to_filter_stop_or_client_error() {
    let cases = [
        ("The model's safety guardrails were triggered.", None),
        (
            "The session's transcript exceeded the model's context size.",
            Some(ErrorKind::InvalidRequest),
        ),
        (
            "Something else went wrong.",
            Some(ErrorKind::UpstreamFailure),
        ),
    ];
    for (message, expected) in cases {
        let mut mock = MockServer::once(error_spec(message)).await;
        let config = apple_config(&mock.address);
        let result = adapter(&config)
            .execute(routed(&config, text_request()))
            .await;
        match expected {
            None => {
                let response = take_chat(result);
                assert_eq!(response.finish_reason, FinishReason::ContentFilter);
                assert!(response.message.content.is_empty());
            }
            Some(kind) => assert_eq!(error_kind(result), kind, "{message}"),
        }
        mock.finish().await;
    }
}

#[tokio::test]
async fn stream_guardrail_event_ends_with_a_content_filter_stop() {
    let body = concat!(
        "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        "event: error\n",
        "data: {\"error\":{\"message\":\"The model's safety guardrails were triggered.\",\"type\":\"server_error\",\"code\":\"500\"}}\n\n",
    );
    let mock = MockServer::once(ResponseSpec::event_stream(body, body.len())).await;
    let config = apple_config(&mock.address);
    let Ok(AdapterOutput::Events(mut stream)) = adapter(&config)
        .execute(routed(&config, text_request_with_stream(true)))
        .await
    else {
        panic!("expected event stream")
    };
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.expect("stream event"));
    }
    assert!(matches!(
        events.as_slice(),
        [
            NormalizedEvent::ChatStarted { .. },
            NormalizedEvent::ChatCompleted {
                finish_reason: FinishReason::ContentFilter,
                ..
            }
        ]
    ));
}

async fn stream_result(spec: ResponseSpec) -> Result<Vec<NormalizedEvent>, ErrorKind> {
    let mock = MockServer::once(spec).await;
    let config = apple_config(&mock.address);
    let mut stream = match adapter(&config)
        .execute(routed(&config, text_request_with_stream(true)))
        .await
    {
        Ok(AdapterOutput::Events(stream)) => stream,
        Ok(_) => panic!("expected event stream"),
        Err(error) => return Err(error.kind),
    };
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.map_err(|error| error.kind)?);
    }
    Ok(events)
}

#[tokio::test]
async fn stream_guardrail_before_streaming_is_also_a_content_filter_stop() {
    let events = stream_result(error_spec("The model's safety guardrails were triggered."))
        .await
        .expect("filtered stream");
    assert!(matches!(
        events.as_slice(),
        [
            NormalizedEvent::ChatStarted { .. },
            NormalizedEvent::ChatCompleted {
                finish_reason: FinishReason::ContentFilter,
                ..
            }
        ]
    ));
}

#[tokio::test]
async fn other_stream_error_events_fail() {
    let body = concat!(
        "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        "event: error\n",
        "data: {\"error\":{\"message\":\"Something else.\",\"type\":\"server_error\",\"code\":\"500\"}}\n\n",
    );
    assert_eq!(
        stream_result(ResponseSpec::event_stream(body, body.len())).await,
        Err(ErrorKind::UpstreamFailure)
    );
}
