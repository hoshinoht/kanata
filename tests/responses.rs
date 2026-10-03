#[path = "support/gateway.rs"]
mod gateway;
#[allow(dead_code)]
#[path = "sse/support.rs"]
mod sse;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use kanata::core::{
    ChatContent, ChatMessage, ChatRole, ErrorKind, FinishReason, GatewayError, ModelAlias,
    NormalizedEvent, Operation, Request as CoreRequest, ToolCall, Usage,
};
use serde_json::{Value, json};

fn request(value: Value) -> Request<Body> {
    let mut request = gateway::chat_request(&value.to_string());
    *request.uri_mut() = "/v1/responses".parse().expect("uri");
    request
}

fn text_server() -> gateway::ServerWithRequests {
    let config = gateway::config();
    gateway::server_with(
        &config,
        vec![gateway::adapter_spec(
            "vllm-private",
            gateway::capabilities(&config, "vllm-private"),
            gateway::Outcome::Chat {
                model: None,
                message: gateway::assistant_text("Hello"),
                finish_reason: FinishReason::Stop,
                usage: Some(Usage {
                    input_tokens: 2,
                    output_tokens: 1,
                    total_tokens: 3,
                    reasoning_tokens: None,
                }),
            },
        )],
    )
}

#[tokio::test]
async fn stateless_text_uses_existing_chat_authorization_and_usage() {
    let (server, requests) = text_server();
    let response = server
        .client_oneshot(request(
            json!({"model":"private-chat","input":"Hello","instructions":"Be brief","store":false}),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let response = gateway::response_json(response).await;
    assert_eq!(response["object"], "response");
    assert_eq!(response["status"], "completed");
    assert_eq!(response["output"][0]["content"][0]["text"], "Hello");
    assert_eq!(response["usage"]["input_tokens"], 2);
    assert_eq!(response["store"], false);
    assert_eq!(response["model"], "private-chat");
    let routed = gateway::take_request(&requests);
    assert_eq!(routed.context().route.selector.operation, Operation::Chat);
    let CoreRequest::Chat(chat) = routed.request() else {
        panic!("chat request");
    };
    assert_eq!(chat.messages[0].role, ChatRole::System);
    assert_eq!(chat.messages[1].role, ChatRole::User);
    let response = server
        .client_oneshot(request(json!({"model":"unknown","input":"Hello"})))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(gateway::recorded_len(&requests), 0);
    let mut unauthenticated = request(json!({"model":"private-chat","input":"Hello"}));
    unauthenticated.headers_mut().remove("authorization");
    assert_eq!(
        server
            .client_oneshot(unauthenticated)
            .await
            .expect("response")
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn unsupported_state_and_fields_fail_before_dispatch() {
    let (server, requests) = text_server();
    for (name, value) in [
        ("store", json!(true)),
        ("background", json!(true)),
        ("previous_response_id", json!("resp_old")),
        ("conversation", json!("conv_old")),
        ("include", json!(["reasoning.encrypted_content"])),
        ("reasoning", json!({"summary":"auto"})),
        ("parallel_tool_calls", json!(false)),
        ("tools", json!([{"type":"web_search"}])),
        (
            "input",
            json!([{"role":"user","content":[{"type":"input_image","image_url":"https://example.invalid/image.png"}]}]),
        ),
    ] {
        let mut body = json!({"model":"private-chat","input":"Hello"});
        body[name] = value;
        let response = server
            .client_oneshot(request(body))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
        let body = gateway::response_json(response).await;
        assert_eq!(body["error"]["param"], name);
    }
    assert_eq!(gateway::recorded_len(&requests), 0);
}

#[tokio::test]
async fn function_outputs_can_be_replayed_with_complete_history() {
    let config = gateway::config();
    let (server, _) = gateway::server_with(
        &config,
        vec![gateway::adapter_spec(
            "vllm-private",
            gateway::capabilities(&config, "vllm-private"),
            gateway::Outcome::Chat {
                model: None,
                message: ChatMessage {
                    role: ChatRole::Assistant,
                    content: vec![ChatContent::ToolCall {
                        call: ToolCall {
                            id: "call_weather".into(),
                            name: "weather".into(),
                            arguments: "{\"city\":\"Singapore\"}".into(),
                        },
                    }],
                },
                finish_reason: FinishReason::ToolCalls,
                usage: None,
            },
        )],
    );
    let response = gateway::response_json(server.client_oneshot(request(json!({"model":"private-chat","input":"Weather?","tools":[{"type":"function","name":"weather","parameters":{"type":"object"},"strict":false}]}))).await.expect("response")).await;
    assert_eq!(response["status"], "completed");
    assert_eq!(response["output"][0]["type"], "function_call");
    let (server, requests) = text_server();
    let replay = json!({"model":"private-chat","input":[{"role":"user","content":"Weather?"},response["output"][0],{"type":"function_call_output","call_id":"call_weather","output":"Sunny"}]});
    assert_eq!(
        server
            .client_oneshot(request(replay))
            .await
            .expect("response")
            .status(),
        StatusCode::OK
    );
    let routed = gateway::take_request(&requests);
    let CoreRequest::Chat(chat) = routed.request() else {
        panic!("chat");
    };
    assert_eq!(chat.messages[1].role, ChatRole::Assistant);
    assert!(
        matches!(&chat.messages[2].content[0], ChatContent::ToolResult { call_id, content } if call_id == "call_weather" && content == "Sunny")
    );
    assert_eq!(server.client_oneshot(request(json!({"model":"private-chat","input":[{"type":"function_call_output","call_id":"orphan","output":"Sunny"}]}))).await.expect("response").status(), StatusCode::BAD_REQUEST);
}

fn start() -> Result<NormalizedEvent, GatewayError> {
    Ok(NormalizedEvent::ChatStarted {
        model: ModelAlias("private-chat".into()),
    })
}
fn completed(reason: FinishReason) -> Result<NormalizedEvent, GatewayError> {
    Ok(NormalizedEvent::ChatCompleted {
        finish_reason: reason,
        usage: Some(Usage {
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
            reasoning_tokens: Some(1),
        }),
    })
}
async fn events(response: axum::response::Response) -> Vec<Value> {
    let bytes = gateway::response_body(response).await;
    let text = std::str::from_utf8(&bytes).expect("utf8");
    assert!(!text.contains("[DONE]"));
    text.split("\n\n")
        .filter(|frame| !frame.is_empty())
        .enumerate()
        .map(|(index, frame)| {
            let (kind, data) = frame.split_once("\ndata: ").expect("typed frame");
            let value: Value = serde_json::from_str(data).expect("event json");
            assert_eq!(
                kind,
                format!("event: {}", value["type"].as_str().expect("event type"))
            );
            assert_eq!(value["sequence_number"], index);
            value
        })
        .collect()
}

#[tokio::test]
async fn streaming_emits_typed_text_and_function_events_and_final_output() {
    let (server, probe) = sse::server(vec![
        start(),
        Ok(NormalizedEvent::ChatTextDelta {
            text: "Checking ".into(),
        }),
        Ok(NormalizedEvent::ChatTextDelta {
            text: "weather".into(),
        }),
        Ok(NormalizedEvent::ChatToolCallDelta {
            call_id: "call_1".into(),
            name: Some("weather".into()),
            arguments_delta: "{\"city\":".into(),
        }),
        Ok(NormalizedEvent::ChatToolCallDelta {
            call_id: "call_1".into(),
            name: None,
            arguments_delta: "\"Singapore\"}".into(),
        }),
        completed(FinishReason::ToolCalls),
    ]);
    let response = server
        .client_oneshot(request(
            json!({"model":"private-chat","input":"Weather?","stream":true}),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let frames = events(response).await;
    assert_eq!(frames[0]["type"], "response.created");
    assert!(frames.iter().any(
        |frame| frame["type"] == "response.function_call_arguments.done"
            && frame["arguments"] == "{\"city\":\"Singapore\"}"
    ));
    let last = frames.last().expect("completed");
    assert_eq!(last["type"], "response.completed");
    assert_eq!(
        last["response"]["output"][0]["content"][0]["text"],
        "Checking weather"
    );
    assert_eq!(last["response"]["output"][1]["call_id"], "call_1");
    assert_eq!(
        last["response"]["usage"]["output_tokens_details"]["reasoning_tokens"],
        1
    );
    assert_eq!(probe.drops(), 1);
}

#[tokio::test]
async fn failures_length_stops_and_client_cancellation_preserve_lifecycle() {
    for (terminal, expected) in [
        (completed(FinishReason::Length), "response.incomplete"),
        (
            Err(GatewayError {
                kind: ErrorKind::UpstreamUnavailable,
            }),
            "response.failed",
        ),
    ] {
        let (server, _) = sse::server(vec![
            start(),
            Ok(NormalizedEvent::ChatTextDelta {
                text: "partial".into(),
            }),
            terminal,
        ]);
        let frames = events(
            server
                .client_oneshot(request(
                    json!({"model":"private-chat","input":"Hello","stream":true}),
                ))
                .await
                .expect("response"),
        )
        .await;
        assert_eq!(frames.last().expect("terminal")["type"], expected);
        assert!(
            !frames
                .iter()
                .any(|frame| frame["type"] == "response.completed")
        );
    }
    let (server, probe) = sse::pending_after_first_server(vec![start()]);
    let response = server
        .client_oneshot(request(
            json!({"model":"private-chat","input":"Hello","stream":true}),
        ))
        .await
        .expect("response");
    assert_eq!(probe.drops(), 0);
    drop(response);
    assert_eq!(probe.drops(), 1);
}

#[tokio::test]
async fn complete_arguments_use_total_output_bound_and_reject_empty_success() {
    let config = gateway::config();
    let arguments = json!({"text":"x".repeat(20 * 1024)}).to_string();
    for (message, reason, status) in [
        (
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![ChatContent::ToolCall {
                    call: ToolCall {
                        id: "call_long".into(),
                        name: "echo".into(),
                        arguments: arguments.clone(),
                    },
                }],
            },
            FinishReason::ToolCalls,
            StatusCode::OK,
        ),
        (
            gateway::assistant_text(""),
            FinishReason::Stop,
            StatusCode::BAD_GATEWAY,
        ),
        (
            gateway::assistant_text(""),
            FinishReason::Length,
            StatusCode::OK,
        ),
    ] {
        let (server, _) = gateway::server_with(
            &config,
            vec![gateway::adapter_spec(
                "vllm-private",
                gateway::capabilities(&config, "vllm-private"),
                gateway::Outcome::Chat {
                    model: None,
                    message,
                    finish_reason: reason,
                    usage: None,
                },
            )],
        );
        let response = server
            .client_oneshot(request(json!({"model":"private-chat","input":"Hello"})))
            .await
            .expect("response");
        assert_eq!(response.status(), status);
        if reason == FinishReason::ToolCalls {
            assert_eq!(
                gateway::response_json(response).await["output"][0]["arguments"],
                arguments
            );
        }
    }
    let (server, requests) = text_server();
    let call = |id| json!({"type":"function_call","call_id":id,"name":"echo","arguments":"{}"});
    let result = |id| json!({"type":"function_call_output","call_id":id,"output":"ok"});
    let response = server.client_oneshot(request(json!({"model":"private-chat","input":[{"role":"user","content":"Hello"},call("a"),call("b"),result("a"),call("c"),result("b"),result("c")]}))).await.expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(gateway::recorded_len(&requests), 0);
}

#[tokio::test(start_paused = true)]
async fn overall_deadline_applies_to_buffered_terminal_events() {
    use http_body_util::BodyExt;
    let (server, _) = sse::server(vec![
        start(),
        Ok(NormalizedEvent::ChatTextDelta {
            text: "Hello".into(),
        }),
        completed(FinishReason::Stop),
    ]);
    let response = server
        .client_oneshot(request(
            json!({"model":"private-chat","input":"Hello","stream":true}),
        ))
        .await
        .expect("response");
    let mut body = response.into_body();
    loop {
        let frame = body.frame().await.expect("frame").expect("valid frame");
        let bytes = frame.into_data().expect("data");
        if String::from_utf8_lossy(&bytes).contains("event: response.output_text.done") {
            break;
        }
    }
    tokio::time::advance(std::time::Duration::from_millis(60_001)).await;
    let frame = body
        .frame()
        .await
        .expect("failure frame")
        .expect("valid frame")
        .into_data()
        .expect("data");
    let frame = String::from_utf8_lossy(&frame);
    assert!(frame.contains("event: response.failed"));
    assert!(frame.contains("upstream_timeout"));
    assert!(body.frame().await.is_none());
}
