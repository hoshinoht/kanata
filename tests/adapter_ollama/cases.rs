use std::sync::atomic::Ordering;

use axum::{body::to_bytes, http::StatusCode};
use futures_util::StreamExt;
use kanata::{
    adapter::{Adapter, AdapterOutput, ollama::OllamaAdapter},
    config,
    core::{
        ChatContent, ChatRole, ErrorKind, Extensions, FinishReason, ModelAlias, NormalizedEvent,
        Operation, Request as CoreRequest, RequestContext, RouteIdentity, RoutedRequest,
        ToolChoice, TranscriptionRequest, ValidatedFile,
    },
    server::{Readiness, TwoPlaneServer},
};
use serde_json::{Value, json};

use crate::support::{
    MockServer, RequestRecord, ResponseSpec, SEPARATE_USAGE_STREAM, TEXT_RESPONSE, TEXT_STREAM,
    TOOLS_RESPONSE, TOOLS_STREAM, adapter, api_request, capabilities, config_for,
    config_for_with_timeouts, error_kind, routed, text_request, text_request_with_stream,
    tool_request,
};

fn decode(record: &RequestRecord) -> Value {
    serde_json::from_slice(&record.body).unwrap_or_else(|_| panic!("request json"))
}

async fn collect_events(
    output: Result<AdapterOutput, kanata::core::GatewayError>,
) -> Vec<NormalizedEvent> {
    let AdapterOutput::Events(mut events) =
        output.unwrap_or_else(|error| panic!("stream setup error: {error:?}"))
    else {
        panic!("expected event stream")
    };
    let mut collected = Vec::new();
    while let Some(event) = events.next().await {
        match event {
            Ok(event) => collected.push(event),
            Err(error) => panic!("stream error after {} events: {error:?}", collected.len()),
        }
    }
    collected
}

async fn first_stream_error(
    output: Result<AdapterOutput, kanata::core::GatewayError>,
) -> (Vec<NormalizedEvent>, ErrorKind) {
    let AdapterOutput::Events(mut events) =
        output.unwrap_or_else(|error| panic!("stream setup error: {error:?}"))
    else {
        panic!("expected event stream")
    };
    let mut successes = Vec::new();
    loop {
        match events.next().await {
            Some(Ok(event)) => successes.push(event),
            Some(Err(error)) => return (successes, error.kind),
            None => panic!("stream ended without error"),
        }
    }
}

fn request_with_stream(stream: bool) -> CoreRequest {
    let CoreRequest::Chat(mut request) = text_request() else {
        panic!("chat request")
    };
    request.stream = stream;
    CoreRequest::Chat(request)
}

fn streaming_tool_request(choice: ToolChoice) -> CoreRequest {
    let CoreRequest::Chat(mut request) = tool_request(choice) else {
        panic!("tool request")
    };
    request.stream = true;
    CoreRequest::Chat(request)
}

fn request_with_extension() -> CoreRequest {
    let CoreRequest::Chat(mut request) = text_request() else {
        panic!("chat request")
    };
    let mut extensions = Extensions::default();
    extensions
        .insert(
            kanata::core::ExtensionKey::parse("ollama.fixture").unwrap_or_else(|_| panic!("key")),
            json!(true),
        )
        .unwrap_or_else(|_| panic!("extension"));
    request.extensions = extensions;
    CoreRequest::Chat(request)
}

fn transcription_request() -> CoreRequest {
    CoreRequest::Transcription(TranscriptionRequest {
        model: ModelAlias("local-chat".into()),
        file: ValidatedFile::new("voice.wav", "audio/wav", b"fixture".to_vec())
            .unwrap_or_else(|_| panic!("file")),
        language: None,
        prompt: None,
        extensions: Extensions::default(),
    })
}

fn transcription_route() -> RouteIdentity {
    RouteIdentity::new(
        "ollama-transcription",
        "concrete-model",
        ModelAlias("local-chat".into()),
        Operation::Transcription,
    )
}

fn route_request(
    _config: &kanata::config::ValidatedConfig,
    route: RouteIdentity,
    request: CoreRequest,
) -> RoutedRequest {
    RoutedRequest::new(
        RequestContext {
            request_id: "fixture-request".into(),
            route,
            trust_zone: kanata::core::TrustZone::Local,
            extensions: Extensions::default(),
        },
        request,
    )
    .unwrap_or_else(|_| panic!("routed request"))
}

#[tokio::test]
async fn constructor_reports_only_configured_nonstream_chat_capabilities() {
    let mock = MockServer::once(ResponseSpec::json(TEXT_RESPONSE)).await;
    let config = config_for(&mock.address, false, false);
    let adapter = adapter(&config);
    assert_eq!(adapter.id(), "ollama-local");
    assert_eq!(adapter.configured_id(), "ollama-local");
    assert_eq!(adapter.capabilities(), &capabilities(&config));
    assert!(!adapter.capabilities().streaming_chat);
    assert!(!adapter.capabilities().function_tools);
    assert_eq!(adapter.capabilities().operations.len(), 1);
    assert!(adapter.capabilities().operations.contains(&Operation::Chat));
    drop(mock);
}

#[tokio::test]
async fn constructor_retains_configured_streaming_capability() {
    let mock = MockServer::once(ResponseSpec::json(TEXT_RESPONSE)).await;
    let config = config_for(&mock.address, true, true);
    let adapter = adapter(&config);
    assert!(adapter.capabilities().streaming_chat);
    assert!(adapter.capabilities().function_tools);
    assert_eq!(mock.once.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn route_constructor_requires_the_exact_chat_route_binding() {
    let mock = MockServer::once(ResponseSpec::json(TEXT_RESPONSE)).await;
    let config = config_for(&mock.address, false, true);
    let adapter = OllamaAdapter::new_for_route(
        &config.adapters()[0],
        &config.routes()[0],
        config.timeouts(),
        config.limits(),
    )
    .unwrap_or_else(|_| panic!("route adapter"));
    assert_eq!(adapter.id(), "ollama-local");
    assert!(
        OllamaAdapter::new_for_route(
            &config.adapters()[0],
            &config.routes()[1],
            config.timeouts(),
            config.limits(),
        )
        .is_err()
    );
    assert!(
        OllamaAdapter::new_for_route(
            &config.adapters()[0],
            &config.routes()[2],
            config.timeouts(),
            config.limits(),
        )
        .is_err()
    );
    assert!(OllamaAdapter::from_config(&config, "ollama-local", "ollama-chat").is_ok());
}

#[tokio::test]
async fn nonstream_text_uses_upstream_model_and_returns_public_alias() {
    let mut mock = MockServer::once(ResponseSpec::json(TEXT_RESPONSE)).await;
    let config = config_for(&mock.address, false, true);
    let adapter = adapter(&config);
    let response =
        crate::support::take_chat(adapter.execute(routed(&config, text_request())).await);
    assert_eq!(response.model, ModelAlias("local-chat".into()));
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(
        response.usage.as_ref().map(|usage| usage.total_tokens),
        Some(18)
    );
    assert_eq!(response.message.role, ChatRole::Assistant);
    assert_eq!(
        response.message.content,
        vec![ChatContent::Text {
            text: "fixture text response".into()
        }]
    );
    assert_eq!(mock.once.load(Ordering::SeqCst), 1);
    mock.finish().await;
    let record = mock
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request lock"))[0]
        .clone();
    assert_eq!(record.path, "/v1/chat/completions");
    let body = decode(&record);
    assert_eq!(body["model"], "llama3.2:latest");
    assert_eq!(body["stream"], false);
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"], "Say hello");
    assert_eq!(body["tool_choice"], "auto");
    assert!(!record.headers.contains_key("authorization"));
}

#[tokio::test]
async fn stream_text_is_demand_driven_and_normalizes_fragmented_utf8_sse() {
    let mut mock = MockServer::once(ResponseSpec::event_stream(TEXT_STREAM, 1)).await;
    let config = config_for(&mock.address, true, true);
    let adapter = adapter(&config);
    let events = collect_events(
        adapter
            .execute(routed(&config, text_request_with_stream(true)))
            .await,
    )
    .await;
    assert_eq!(
        events,
        vec![
            NormalizedEvent::ChatStarted {
                model: ModelAlias("local-chat".into())
            },
            NormalizedEvent::ChatTextDelta {
                text: "hello ".into()
            },
            NormalizedEvent::ChatTextDelta {
                text: "🌍".into()
            },
            NormalizedEvent::ChatCompleted {
                finish_reason: FinishReason::Stop,
                usage: Some(kanata::core::Usage {
                    input_tokens: 11,
                    output_tokens: 7,
                    total_tokens: 18,
                })
            }
        ]
    );
    assert_eq!(mock.once.load(Ordering::SeqCst), 1);
    mock.finish().await;
    let record = mock
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request lock"))[0]
        .clone();
    let body = decode(&record);
    assert_eq!(body["model"], "llama3.2:latest");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(record.headers["accept"], "text/event-stream");
    assert!(!record.headers.contains_key("authorization"));
}

#[tokio::test]
async fn stream_separate_usage_is_emitted_only_at_done() {
    let mut mock = MockServer::once(ResponseSpec::event_stream(SEPARATE_USAGE_STREAM, 3)).await;
    let config = config_for(&mock.address, true, false);
    let adapter = adapter(&config);
    let events = collect_events(
        adapter
            .execute(routed(&config, text_request_with_stream(true)))
            .await,
    )
    .await;
    assert!(matches!(
        events.as_slice(),
        [
            NormalizedEvent::ChatStarted { .. },
            NormalizedEvent::ChatTextDelta { text },
            NormalizedEvent::ChatCompleted { finish_reason: FinishReason::Stop, usage: Some(usage) }
        ] if text == "separate usage" && usage.total_tokens == 5
    ));
    mock.finish().await;
}

#[tokio::test]
async fn stream_interleaves_tool_calls_by_provider_index_without_mutating_identity() {
    let mut mock = MockServer::once(ResponseSpec::event_stream(TOOLS_STREAM, 2)).await;
    let config = config_for(&mock.address, true, true);
    let adapter = adapter(&config);
    let events = collect_events(
        adapter
            .execute(routed(&config, streaming_tool_request(ToolChoice::Auto)))
            .await,
    )
    .await;
    assert!(matches!(
        events.as_slice(),
        [
            NormalizedEvent::ChatStarted { .. },
            NormalizedEvent::ChatToolCallDelta { call_id, name: Some(name), arguments_delta: first },
            NormalizedEvent::ChatToolCallDelta { call_id: second_id, name: Some(second_name), arguments_delta: second },
            NormalizedEvent::ChatTextDelta { text },
            NormalizedEvent::ChatToolCallDelta { call_id: continued_id, name: None, arguments_delta: continued },
            NormalizedEvent::ChatToolCallDelta { call_id: continued_second_id, name: None, arguments_delta: continued_second },
            NormalizedEvent::ChatCompleted { finish_reason: FinishReason::ToolCalls, .. }
        ] if call_id == "call_stream_one"
            && name == "lookup"
            && first == "{\"q\":\"one"
            && second_id == "call_stream_two"
            && second_name == "lookup"
            && second == "{\"q\":\"two"
            && text == "interleaved"
            && continued_id == "call_stream_one"
            && continued == "\"}"
            && continued_second_id == "call_stream_two"
            && continued_second == "\"}"
    ));
    mock.finish().await;
}

#[tokio::test]
async fn stream_completion_wires_through_two_plane_server_with_public_model_and_done() {
    let mut mock = MockServer::once(ResponseSpec::event_stream(TEXT_STREAM, 1)).await;
    let config = config_for(&mock.address, true, true);
    let adapter = adapter(&config);
    let server = TwoPlaneServer::from_validated_with_adapters(
        &config,
        &crate::support::Resolver,
        Readiness::new(true),
        vec![std::sync::Arc::new(adapter)],
    )
    .unwrap_or_else(|_| panic!("server"));
    let response = api_request(
        &server,
        r#"{"model":"local-chat","messages":[{"role":"user","content":"Say hello"}],"stream":true,"stream_options":{"include_usage":true}}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|_| panic!("stream body"));
    let body = String::from_utf8(body.to_vec()).unwrap_or_else(|_| panic!("stream utf8"));
    assert!(body.contains("\"model\":\"local-chat\""));
    assert!(body.contains("hello"));
    assert!(body.contains("\"prompt_tokens\":11"));
    assert!(body.ends_with("data: [DONE]\n\n"));
    mock.finish().await;
}

#[tokio::test]
async fn dropping_stream_before_first_event_closes_upstream_without_eager_collection() {
    let mut mock = MockServer::once(ResponseSpec {
        status: StatusCode::OK,
        content_type: Some("text/event-stream"),
        body: Vec::new(),
        chunks: Vec::new(),
        wait_for_close: true,
        write_body_before_close: false,
    })
    .await;
    let config = config_for(&mock.address, true, false);
    let adapter = adapter(&config);
    let output = adapter
        .execute(routed(&config, text_request_with_stream(true)))
        .await
        .unwrap_or_else(|error| panic!("stream setup: {error:?}"));
    let AdapterOutput::Events(events) = output else {
        panic!("expected event stream")
    };
    mock.wait_for_response_headers().await;
    drop(events);
    mock.wait_for_close().await;
    assert_eq!(mock.once.load(Ordering::SeqCst), 1);
    mock.finish().await;
}

#[tokio::test]
async fn partial_provider_record_times_out_before_api_stream_headers() {
    let mut mock = MockServer::once(ResponseSpec {
        status: StatusCode::OK,
        content_type: Some("text/event-stream"),
        body: Vec::new(),
        chunks: vec![
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"thinking\"}"
                .to_vec(),
        ],
        wait_for_close: true,
        write_body_before_close: true,
    })
    .await;
    let config = config_for_with_timeouts(&mock.address, true, false, Some(20), Some(20));
    let adapter = adapter(&config);
    let server = TwoPlaneServer::from_validated_with_adapters(
        &config,
        &crate::support::Resolver,
        Readiness::new(true),
        vec![std::sync::Arc::new(adapter)],
    )
    .unwrap_or_else(|_| panic!("server"));
    let response = api_request(
        &server,
        r#"{"model":"local-chat","messages":[{"role":"user","content":"Say hello"}],"stream":true}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|_| panic!("timeout body"));
    assert!(String::from_utf8_lossy(&body).contains("upstream_timeout"));
    mock.wait_for_close().await;
    mock.finish().await;
}

#[tokio::test]
async fn malformed_stream_lifecycle_is_a_single_upstream_failure_without_completion() {
    let cases = [
        "data: [DONE]\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"user\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null},{\"index\":1,\"delta\":{},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"visible\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"unknown\"}]}\n\ndata: [DONE]\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"visible\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":-1,\"completion_tokens\":1,\"total_tokens\":0}}\n\ndata: [DONE]\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"visible\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n",
    ];
    for body in cases {
        let mut mock = MockServer::once(ResponseSpec::event_stream(body, 2)).await;
        let config = config_for(&mock.address, true, true);
        let adapter = adapter(&config);
        let (events, kind) = first_stream_error(
            adapter
                .execute(routed(&config, text_request_with_stream(true)))
                .await,
        )
        .await;
        assert_eq!(kind, ErrorKind::UpstreamFailure);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, NormalizedEvent::ChatCompleted { .. }))
        );
        mock.finish().await;
    }
}

#[tokio::test]
async fn sse_bom_comments_and_multiline_data_are_normalized() {
    let body = concat!(
        "\u{feff}: keepalive\r\n\r\n",
        "data: {\"choices\":[{\"index\":0,\r\n",
        "data: \"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"hidden\"},\"finish_reason\":null}]}\r\n\r\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"visible\"},\"finish_reason\":null}]}\r\n\r\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\r\n\r\n",
        "data: [DONE]\r\n\r\n",
    );
    let mut mock = MockServer::once(ResponseSpec::event_stream(body, 1)).await;
    let config = config_for(&mock.address, true, false);
    let adapter = adapter(&config);
    let events = collect_events(
        adapter
            .execute(routed(&config, text_request_with_stream(true)))
            .await,
    )
    .await;
    assert!(matches!(
        events.as_slice(),
        [
            NormalizedEvent::ChatStarted { .. },
            NormalizedEvent::ChatTextDelta { text },
            NormalizedEvent::ChatCompleted {
                finish_reason: FinishReason::Stop,
                usage: None
            }
        ] if text == "visible"
    ));
    mock.finish().await;
}

#[tokio::test]
async fn sse_line_and_event_bounds_fail_without_completion() {
    let cases = [
        format!("data: {}\n\n", "x".repeat(65 * 1024)),
        format!(
            "data: {}\ndata: {}\n\n",
            "x".repeat(65_530),
            "x".repeat(65_530)
        ),
    ];
    for body in cases {
        let mut mock = MockServer::once(ResponseSpec::event_stream(&body, 1024)).await;
        let config = config_for(&mock.address, true, false);
        let adapter = adapter(&config);
        let (events, kind) = first_stream_error(
            adapter
                .execute(routed(&config, text_request_with_stream(true)))
                .await,
        )
        .await;
        assert_eq!(kind, ErrorKind::UpstreamFailure);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, NormalizedEvent::ChatCompleted { .. }))
        );
        mock.finish().await;
    }
}

#[tokio::test]
async fn tool_history_and_all_tool_choices_are_encoded_without_alias_leakage() {
    for (choice, response_body) in [
        (ToolChoice::None, TEXT_RESPONSE),
        (ToolChoice::Auto, TEXT_RESPONSE),
        (ToolChoice::Required, TOOLS_RESPONSE),
        (
            ToolChoice::Function {
                name: "lookup".into(),
            },
            TOOLS_RESPONSE,
        ),
    ] {
        let mut mock = MockServer::once(ResponseSpec::json(response_body)).await;
        let config = config_for(&mock.address, false, true);
        let adapter = adapter(&config);
        let response = crate::support::take_chat(
            adapter
                .execute(routed(&config, tool_request(choice.clone())))
                .await,
        );
        assert_eq!(response.model, ModelAlias("local-chat".into()));
        assert_eq!(mock.once.load(Ordering::SeqCst), 1);
        mock.finish().await;
        let record = mock
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("request lock"))[0]
            .clone();
        let body = decode(&record);
        assert_eq!(body["model"], "llama3.2:latest");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "lookup");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["messages"][1]["content"], Value::Null);
        assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "call_history_1");
        assert_eq!(body["messages"][1]["tool_calls"][0]["type"], "function");
        assert_eq!(body["messages"][2]["role"], "tool");
        assert_eq!(body["messages"][2]["tool_call_id"], "call_history_1");
        match choice {
            ToolChoice::None => assert_eq!(body["tool_choice"], "none"),
            ToolChoice::Auto => assert_eq!(body["tool_choice"], "auto"),
            ToolChoice::Required => assert_eq!(body["tool_choice"], "required"),
            ToolChoice::Function { .. } => {
                assert_eq!(body["tool_choice"]["type"], "function");
                assert_eq!(body["tool_choice"]["function"]["name"], "lookup");
            }
        }
    }
}

#[tokio::test]
async fn tool_response_normalizes_calls_usage_and_concrete_provider_model() {
    let mut mock = MockServer::once(ResponseSpec::json(TOOLS_RESPONSE)).await;
    let config = config_for(&mock.address, false, true);
    let adapter = adapter(&config);
    let response = crate::support::take_chat(
        adapter
            .execute(routed(&config, tool_request(ToolChoice::Required)))
            .await,
    );
    assert_eq!(response.model, ModelAlias("local-chat".into()));
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        response.usage.as_ref().map(|usage| usage.input_tokens),
        Some(13)
    );
    assert!(matches!(
        response.message.content.as_slice(),
        [ChatContent::ToolCall { call }] if call.id == "call_fixture_1"
            && call.name == "lookup"
            && call.arguments == "{\"q\":\"fixture\"}"
    ));
    mock.finish().await;
}

#[tokio::test]
async fn omitted_usage_remains_none() {
    let body = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"without usage"},"finish_reason":"stop"}]}"#;
    let mut mock = MockServer::once(ResponseSpec::json(body)).await;
    let config = config_for(&mock.address, false, true);
    let adapter = adapter(&config);
    let response =
        crate::support::take_chat(adapter.execute(routed(&config, text_request())).await);
    assert!(response.usage.is_none());
    mock.finish().await;
}

#[tokio::test]
async fn unsupported_stream_transcription_and_extensions_are_zero_dispatch() {
    let cases = [
        ("stream", request_with_stream(true), false),
        ("transcription", transcription_request(), true),
        ("extension", request_with_extension(), false),
    ];
    for (name, request, transcription) in cases {
        let mock = MockServer::once(ResponseSpec::json(TEXT_RESPONSE)).await;
        let config = config_for(&mock.address, false, true);
        let adapter = adapter(&config);
        let request = if transcription {
            route_request(&config, transcription_route(), request)
        } else {
            routed(&config, request)
        };
        let kind = error_kind(adapter.execute(request).await);
        assert!(
            matches!(
                kind,
                ErrorKind::UnsupportedOperation | ErrorKind::InvalidRequest
            ),
            "{name}: {kind:?}"
        );
        assert_eq!(mock.once.load(Ordering::SeqCst), 0, "{name}");
        drop(mock);
    }
}

#[tokio::test]
async fn invalid_core_messages_and_tool_references_are_zero_dispatch() {
    let mut cases = Vec::new();
    let CoreRequest::Chat(mut empty) = text_request() else {
        panic!("chat request")
    };
    empty.messages.clear();
    cases.push(CoreRequest::Chat(empty));
    let CoreRequest::Chat(mut empty_content) = text_request() else {
        panic!("chat request")
    };
    empty_content.messages[0].content = vec![ChatContent::Text {
        text: String::new(),
    }];
    cases.push(CoreRequest::Chat(empty_content));
    let CoreRequest::Chat(mut bad_tool) = tool_request(ToolChoice::Auto) else {
        panic!("chat request")
    };
    bad_tool.tools[0].name = "bad.name".into();
    cases.push(CoreRequest::Chat(bad_tool));
    for request in cases {
        let mock = MockServer::once(ResponseSpec::json(TEXT_RESPONSE)).await;
        let config = config_for(&mock.address, false, true);
        let adapter = adapter(&config);
        assert_eq!(
            error_kind(adapter.execute(routed(&config, request)).await),
            ErrorKind::InvalidRequest
        );
        assert_eq!(mock.once.load(Ordering::SeqCst), 0);
        drop(mock);
    }
}

#[tokio::test]
async fn malformed_success_responses_are_static_upstream_failures() {
    let cases = [
        r#"{"choices":[{"index":0,"message":{"role":"assistant","content":7},"finish_reason":"stop"}]}"#,
        r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"one"},"finish_reason":"stop"},{"index":1,"message":{"role":"assistant","content":"two"},"finish_reason":"stop"}]}"#,
        r#"{"choices":[{"index":1,"message":{"role":"assistant","content":"one"},"finish_reason":"stop"}]}"#,
        r#"{"choices":[{"index":0,"message":{"role":"user","content":"one"},"finish_reason":"stop"}]}"#,
        r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"","type":"function","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"one"},"finish_reason":"unknown"}]}"#,
        r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"one"},"finish_reason":"stop"}],"usage":{"prompt_tokens":-1,"completion_tokens":1,"total_tokens":0}}"#,
    ];
    for body in cases {
        let mut mock = MockServer::once(ResponseSpec::json(body)).await;
        let config = config_for(&mock.address, false, true);
        let adapter = adapter(&config);
        let result = adapter.execute(routed(&config, text_request())).await;
        assert_eq!(error_kind(result), ErrorKind::UpstreamFailure);
        assert_eq!(mock.once.load(Ordering::SeqCst), 1);
        mock.finish().await;
    }
}

#[tokio::test]
async fn reasoning_content_is_not_exposed_as_assistant_text() {
    let body = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"visible","reasoning_content":"hidden"},"finish_reason":"stop"}]}"#;
    let mut mock = MockServer::once(ResponseSpec::json(body)).await;
    let config = config_for(&mock.address, false, true);
    let adapter = adapter(&config);
    let response =
        crate::support::take_chat(adapter.execute(routed(&config, text_request())).await);
    assert_eq!(
        response.message.content,
        vec![ChatContent::Text {
            text: "visible".into()
        }]
    );
    mock.finish().await;
}

#[tokio::test]
async fn length_stop_without_output_is_an_empty_reply_and_stop_is_not() {
    for (finish_reason, ok) in [("length", true), ("stop", false)] {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": {"role":"assistant","content":"","reasoning":"hidden"},
                "finish_reason": finish_reason
            }]
        })
        .to_string();
        let mut mock = MockServer::once(ResponseSpec::json(&body)).await;
        let config = config_for(&mock.address, false, true);
        let adapter = adapter(&config);
        let output = adapter.execute(routed(&config, text_request())).await;
        if ok {
            let response = crate::support::take_chat(output);
            assert_eq!(response.finish_reason, FinishReason::Length);
            assert!(response.message.content.is_empty());
        } else {
            assert_eq!(error_kind(output), ErrorKind::UpstreamFailure);
        }
        mock.finish().await;
    }
}

#[tokio::test]
async fn known_finish_interruptions_allow_tool_calls_and_stop_does_not() {
    for finish_reason in ["length", "content_filter"] {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_interrupt","type":"function","function":{"name":"lookup","arguments":"{}"}}
                ]},
                "finish_reason": finish_reason
            }]
        })
        .to_string();
        let mut mock = MockServer::once(ResponseSpec::json(&body)).await;
        let config = config_for(&mock.address, false, true);
        let adapter = adapter(&config);
        let response =
            crate::support::take_chat(adapter.execute(routed(&config, text_request())).await);
        assert_eq!(
            response.finish_reason,
            if finish_reason == "length" {
                FinishReason::Length
            } else {
                FinishReason::ContentFilter
            }
        );
        mock.finish().await;
    }
    let body = json!({
        "choices": [{
            "index": 0,
            "message": {"role":"assistant","content":null,"tool_calls":[
                {"id":"call_stop","type":"function","function":{"name":"lookup","arguments":"{}"}}
            ]},
            "finish_reason": "stop"
        }]
    })
    .to_string();
    let mut mock = MockServer::once(ResponseSpec::json(&body)).await;
    let config = config_for(&mock.address, false, true);
    let adapter = adapter(&config);
    assert_eq!(
        error_kind(adapter.execute(routed(&config, text_request())).await),
        ErrorKind::UpstreamFailure
    );
    mock.finish().await;
}

#[tokio::test]
async fn status_matrix_is_redacted_and_never_retried() {
    let cases = [
        (StatusCode::BAD_REQUEST, ErrorKind::InvalidRequest),
        (StatusCode::NOT_FOUND, ErrorKind::NotFound),
        (StatusCode::UNPROCESSABLE_ENTITY, ErrorKind::InvalidRequest),
        (StatusCode::UNAUTHORIZED, ErrorKind::UpstreamUnavailable),
        (StatusCode::FORBIDDEN, ErrorKind::UpstreamUnavailable),
        (StatusCode::TOO_MANY_REQUESTS, ErrorKind::RateLimited),
        (StatusCode::REQUEST_TIMEOUT, ErrorKind::UpstreamUnavailable),
        (StatusCode::BAD_GATEWAY, ErrorKind::UpstreamFailure),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorKind::UpstreamUnavailable,
        ),
        (StatusCode::GATEWAY_TIMEOUT, ErrorKind::UpstreamUnavailable),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorKind::UpstreamFailure,
        ),
        (StatusCode::FOUND, ErrorKind::UpstreamFailure),
    ];
    for (status, expected) in cases {
        let mut mock = MockServer::once(ResponseSpec::status(status)).await;
        let config = config_for(&mock.address, false, true);
        let adapter = adapter(&config);
        let result = adapter.execute(routed(&config, text_request())).await;
        assert_eq!(error_kind(result), expected, "status {status}");
        assert_eq!(mock.once.load(Ordering::SeqCst), 1, "status {status}");
        mock.finish().await;
        let record = mock
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("request lock"))[0]
            .clone();
        assert!(!String::from_utf8_lossy(&record.body).contains("fixture marker"));
    }
}

#[tokio::test]
async fn non_json_success_is_upstream_failure() {
    let mut mock = MockServer::once(ResponseSpec {
        status: StatusCode::OK,
        content_type: Some("text/plain"),
        body: b"provider marker".to_vec(),
        chunks: Vec::new(),
        wait_for_close: false,
        write_body_before_close: false,
    })
    .await;
    let config = config_for(&mock.address, false, true);
    let adapter = adapter(&config);
    assert_eq!(
        error_kind(adapter.execute(routed(&config, text_request())).await),
        ErrorKind::UpstreamFailure
    );
    mock.finish().await;
}

#[tokio::test]
async fn cancellation_drops_transport_and_closes_upstream_socket() {
    let mut mock = MockServer::once(ResponseSpec {
        status: StatusCode::OK,
        content_type: Some("application/json"),
        body: vec![b'x'; 64],
        chunks: Vec::new(),
        wait_for_close: true,
        write_body_before_close: false,
    })
    .await;
    let config = config_for(&mock.address, false, true);
    let adapter = adapter(&config);
    let task = tokio::spawn(adapter.execute(routed(&config, text_request())));
    mock.wait_for_headers().await;
    task.abort();
    assert!(task.await.is_err(), "cancellation task completed");
    mock.wait_for_close().await;
    assert_eq!(mock.once.load(Ordering::SeqCst), 1);
    mock.finish().await;
}

#[tokio::test]
async fn real_adapter_wires_through_both_planes_without_provider_types() {
    let mut mock = MockServer::once(ResponseSpec::json(TEXT_RESPONSE)).await;
    let config = config_for(&mock.address, false, true);
    let adapter = adapter(&config);
    let server = TwoPlaneServer::from_validated_with_adapters(
        &config,
        &crate::support::Resolver,
        Readiness::new(true),
        vec![std::sync::Arc::new(adapter)],
    )
    .unwrap_or_else(|_| panic!("server"));
    let response = api_request(
        &server,
        r#"{"model":"local-chat","messages":[{"role":"user","content":"Say hello"}]}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|_| panic!("api body"));
    let body: Value = serde_json::from_slice(&body).unwrap_or_else(|_| panic!("api json"));
    assert_eq!(body["model"], "local-chat");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "fixture text response"
    );
    assert_eq!(mock.once.load(Ordering::SeqCst), 1);
    mock.finish().await;
}

#[test]
fn constructor_rejects_wrong_provider_and_trust_zone() {
    let contents = include_str!("../../tests/fixtures/config/example.toml");
    let wrong_provider = contents.replacen("kind = \"ollama\"", "kind = \"vllm\"", 1);
    let config = load_contents(&wrong_provider);
    let config = config.unwrap_or_else(|_| panic!("wrong provider config"));
    let result = OllamaAdapter::new(&config.adapters()[0], config.timeouts(), config.limits());
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("wrong provider accepted"),
    };
    assert_eq!(error.kind, ErrorKind::Internal);

    let wrong_zone = contents.replacen("trust_zone = \"local\"", "trust_zone = \"external\"", 1);
    let config = load_contents(&wrong_zone);
    assert!(config.is_err());

    let with_secret = contents.replacen(
        "trust_zone = \"local\"",
        "trust_zone = \"local\"\nsecret_ref = \"env:OLLAMA_FIXTURE_SECRET\"",
        1,
    );
    let config = load_contents(&with_secret).unwrap_or_else(|_| panic!("secret config"));
    assert!(OllamaAdapter::new(&config.adapters()[0], config.timeouts(), config.limits()).is_err());
}

fn load_contents(contents: &str) -> Result<kanata::config::ValidatedConfig, config::ConfigError> {
    let path = std::env::temp_dir().join("kanata-ollama-constructor.toml");
    std::fs::write(&path, contents).unwrap_or_else(|_| panic!("write config"));
    let result = config::load(&path);
    let _ = std::fs::remove_file(path);
    result
}
