#[path = "support/gateway.rs"]
mod gateway;
#[path = "support/scratch.rs"]
mod scratch;
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
                    cached_input_tokens: None,
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
async fn unsupported_state_and_fields_fail_before_dispatch_with_a_param() {
    let (server, requests) = text_server();
    for (name, value, param) in [
        ("store", json!(true), "store"),
        ("background", json!(true), "background"),
        (
            "previous_response_id",
            json!("resp_old"),
            "previous_response_id",
        ),
        ("conversation", json!("conv_old"), "conversation"),
        ("include", json!(["file_search_call.results"]), "include"),
        (
            "reasoning",
            json!({"summary":"verbose"}),
            "reasoning.summary",
        ),
        (
            "reasoning",
            json!({"generate_summary":"auto"}),
            "reasoning.generate_summary",
        ),
        (
            "text",
            json!({"format":{"type":"json_object"}}),
            "text.format",
        ),
        ("text", json!({"verbosity":"loud"}), "text.verbosity"),
        ("client_metadata", json!({"turn":1}), "client_metadata"),
        ("metadata", json!({"k":"v".repeat(513)}), "metadata"),
        ("user", json!(""), "user"),
        ("prompt_cache_key", json!(""), "prompt_cache_key"),
        ("parallel_tool_calls", json!(false), "parallel_tool_calls"),
        ("frobnicate", json!(1), "frobnicate"),
        ("tools", json!([{"type":"web_search"}]), "tools[0]"),
        (
            "tools",
            json!([{"type":"function","name":"f","parameters":{},"strict":"yes"}]),
            "tools[0].strict",
        ),
        (
            "tools",
            json!([{"type":"function","name":"f","parameters":{},"extra":1}]),
            "tools[0].extra",
        ),
        (
            "input",
            json!([{"role":"user","content":[{"type":"input_image","image_url":"https://example.invalid/image.png"}]}]),
            "input[0].content",
        ),
        (
            "input",
            json!([{"role":"user","content":"x","name":"n"}]),
            "input[0].name",
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
        assert_eq!(body["error"]["param"], param, "{name}");
    }
    assert_eq!(gateway::recorded_len(&requests), 0);
}

#[tokio::test]
async fn hint_fields_are_accepted_and_only_the_cache_key_and_include_reach_core() {
    let (server, requests) = text_server();
    let response = server
        .client_oneshot(request(json!({
            "model": "private-chat",
            "input": "Hello",
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "parallel_tool_calls": true,
            "prompt_cache_key": "00000000-0000-4000-8000-000000000003",
            "client_metadata": {"session": "TEST_ONLY"},
            "metadata": {"k": "v"},
            "user": "TEST_ONLY_USER",
            "reasoning": {"summary": "auto"},
            "text": {"verbosity": "low", "format": {"type": "text"}},
            "tools": [{"type":"function","name":"f","parameters":{"type":"object"},"strict":true}]
        })))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let routed = gateway::take_request(&requests);
    let CoreRequest::Chat(chat) = routed.request() else {
        panic!("chat request");
    };
    assert_eq!(
        chat.prompt_cache_key.as_ref().map(|key| key.as_str()),
        Some("00000000-0000-4000-8000-000000000003")
    );
    assert!(chat.include_encrypted_reasoning);
    let serialized = serde_json::to_string(chat).expect("serializes");
    assert!(!serialized.contains("TEST_ONLY"), "{serialized}");
    let response = server
        .client_oneshot(request(
            json!({"model":"private-chat","input":"Hello","include":[],"reasoning":null}),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let routed = gateway::take_request(&requests);
    let CoreRequest::Chat(chat) = routed.request() else {
        panic!("chat request");
    };
    assert!(!chat.include_encrypted_reasoning);
    assert!(chat.prompt_cache_key.is_none());
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
    let orphan = server.client_oneshot(request(json!({"model":"private-chat","input":[{"type":"function_call_output","call_id":"orphan","output":"Sunny"}]}))).await.expect("response");
    assert_eq!(orphan.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        gateway::response_json(orphan).await["error"]["param"],
        "input[0].call_id"
    );

    let call = json!({"type":"function_call","id":"fc_1","status":"completed","call_id":"call_1","name":"weather","arguments":"{}"});
    for (output, expected) in [
        (json!(""), ""),
        (
            json!([{"type":"input_text","text":"Sun"},{"type":"input_text","text":"ny"}]),
            "Sunny",
        ),
    ] {
        let replay = json!({"model":"private-chat","input":[{"role":"user","content":"Weather?"},call,{"type":"function_call_output","id":"fco_1","status":"completed","call_id":"call_1","output":output}]});
        let response = server
            .client_oneshot(request(replay))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK, "{output}");
        let routed = gateway::take_request(&requests);
        let CoreRequest::Chat(chat) = routed.request() else {
            panic!("chat");
        };
        assert_eq!(
            chat.messages[2].content,
            vec![ChatContent::ToolResult {
                call_id: "call_1".into(),
                content: expected.into(),
            }]
        );
    }
    let image = json!([{"type":"input_image","image_url":"data:image/png;base64,AA=="}]);
    let replay = json!({"model":"private-chat","input":[{"role":"user","content":"Weather?"},call,{"type":"function_call_output","call_id":"call_1","output":image}]});
    let response = server
        .client_oneshot(request(replay))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        gateway::response_json(response).await["error"]["param"],
        "input[2].output"
    );
}

#[tokio::test]
async fn encrypted_reasoning_items_join_the_assistant_output_that_follows_them() {
    let (server, requests) = text_server();
    let reasoning = |id: &str, encrypted: &str| json!({"type":"reasoning","id":id,"summary":[],"encrypted_content":encrypted});
    let input = json!([
        {"role":"user","content":"Weather?"},
        reasoning("rs_1", "opaque-1"),
        {"type":"function_call","call_id":"call_1","name":"weather","arguments":"{}"},
        {"type":"function_call_output","call_id":"call_1","output":"Sunny"},
        reasoning("rs_2", "opaque-2"),
        {"type":"message","role":"assistant","content":[{"type":"output_text","text":"It is sunny.","annotations":[]}]},
        {"type":"reasoning","summary":[{"type":"summary_text","text":"Summary only."}],"encrypted_content":null},
        reasoning("rs_3", "opaque-3"),
        {"role":"user","content":"Thanks"},
        reasoning("rs_4", "opaque-4")
    ]);
    let response = server
        .client_oneshot(request(json!({"model":"private-chat","input":input})))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let routed = gateway::take_request(&requests);
    let CoreRequest::Chat(chat) = routed.request() else {
        panic!("chat");
    };
    let reasoning_ids = |message: &ChatMessage| -> Vec<Option<String>> {
        message
            .content
            .iter()
            .filter_map(|content| match content {
                ChatContent::Reasoning { item } => Some(item.id().map(str::to_owned)),
                _ => None,
            })
            .collect()
    };
    assert_eq!(chat.messages.len(), 6);
    let first = &chat.messages[1];
    assert_eq!(first.role, ChatRole::Assistant);
    assert!(
        matches!(&first.content[0], ChatContent::Reasoning { item } if item.encrypted_content() == "opaque-1")
    );
    assert!(matches!(&first.content[1], ChatContent::ToolCall { .. }));
    // Reasoning before the answer leads it; reasoning after it in the same turn trails it.
    let answer = &chat.messages[3];
    assert_eq!(
        reasoning_ids(answer),
        vec![Some("rs_2".to_owned()), Some("rs_3".to_owned())]
    );
    assert!(matches!(&answer.content[0], ChatContent::Reasoning { .. }));
    assert!(matches!(&answer.content[1], ChatContent::Text { .. }));
    assert_eq!(chat.messages[4].role, ChatRole::User);
    // Trailing reasoning with no assistant output becomes a reasoning-only turn.
    assert_eq!(chat.messages[5].role, ChatRole::Assistant);
    assert_eq!(
        reasoning_ids(&chat.messages[5]),
        vec![Some("rs_4".to_owned())]
    );
    assert_eq!(chat.messages[5].content.len(), 1);

    let empty = json!({"model":"private-chat","input":[{"role":"user","content":"x"},{"type":"reasoning","summary":[],"encrypted_content":""}]});
    let response = server
        .client_oneshot(request(empty))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        gateway::response_json(response).await["error"]["param"],
        "input[1].encrypted_content"
    );
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
            cached_input_tokens: None,
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
async fn complete_arguments_use_total_output_bound_and_accept_empty_success() {
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
            StatusCode::OK,
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

#[derive(Clone)]
struct SummaryAdapter {
    capabilities: kanata::core::Capabilities,
    response: kanata::core::ChatResponse,
    events: Vec<Result<NormalizedEvent, GatewayError>>,
}

impl kanata::adapter::Adapter for SummaryAdapter {
    fn id(&self) -> &str {
        "vllm-private"
    }

    fn capabilities(&self) -> &kanata::core::Capabilities {
        &self.capabilities
    }

    fn execute(&self, routed: kanata::core::RoutedRequest) -> kanata::adapter::AdapterFuture {
        let streaming = matches!(routed.request(), CoreRequest::Chat(chat) if chat.stream);
        let response = self.response.clone();
        let events = self.events.clone();
        Box::pin(async move {
            if streaming {
                Ok(kanata::adapter::AdapterOutput::Events(Box::pin(
                    futures_util::stream::iter(events),
                )))
            } else {
                Ok(kanata::adapter::AdapterOutput::Complete(
                    kanata::core::Response::Chat(response),
                ))
            }
        })
    }
}

fn summary_server(
    reasoning: String,
    text: String,
    with_tool: bool,
    reasoning_tokens: Option<u64>,
) -> kanata::server::TwoPlaneServer {
    let config = gateway::config_with_public_routes(&[("private-chat", "chat")]);
    let has_text = !text.is_empty();
    let tool = ToolCall {
        id: "call_summary".into(),
        name: "weather".into(),
        arguments: "{}".into(),
    };
    let mut content = vec![ChatContent::Text { text: text.clone() }];
    if with_tool {
        content.push(ChatContent::ToolCall { call: tool.clone() });
    }
    let finish_reason = if with_tool {
        FinishReason::ToolCalls
    } else {
        FinishReason::Stop
    };
    let usage = Some(Usage {
        input_tokens: 1,
        output_tokens: 2,
        total_tokens: 3,
        reasoning_tokens,
        cached_input_tokens: None,
    });
    let mut events = vec![start(), Ok(NormalizedEvent::ChatTextDelta { text })];
    if with_tool {
        events.push(Ok(NormalizedEvent::ChatToolCallDelta {
            call_id: tool.id.clone(),
            name: Some(tool.name),
            arguments_delta: "{".into(),
        }));
    }
    events.push(Ok(NormalizedEvent::ChatReasoningDelta {
        text: reasoning.clone(),
    }));
    if with_tool {
        events.push(Ok(NormalizedEvent::ChatToolCallDelta {
            call_id: tool.id,
            name: None,
            arguments_delta: "}".into(),
        }));
        events.push(Ok(NormalizedEvent::ChatReasoningDelta {
            text: " More.".into(),
        }));
        if has_text {
            events.push(Ok(NormalizedEvent::ChatTextDelta {
                text: " Ready.".into(),
            }));
        }
    }
    events.push(Ok(NormalizedEvent::ChatCompleted {
        finish_reason,
        usage: usage.clone(),
    }));
    let adapter = SummaryAdapter {
        capabilities: gateway::capabilities(&config, "vllm-private"),
        response: kanata::core::ChatResponse {
            model: ModelAlias("private-chat".into()),
            message: ChatMessage {
                role: ChatRole::Assistant,
                content,
            },
            finish_reason,
            usage,
            reasoning: Some(reasoning),
            reasoning_items: Vec::new(),
        },
        events,
    };
    kanata::server::TwoPlaneServer::from_validated_with_adapters(
        &config,
        &gateway::Resolver,
        kanata::server::Readiness::new(true),
        vec![std::sync::Arc::new(adapter)],
    )
    .expect("summary server")
}

#[tokio::test]
async fn private_complete_includes_summary_and_preserves_zero_or_missing_tokens() {
    for reasoning_tokens in [Some(0), None] {
        let server = summary_server("Summary.".into(), "Answer.".into(), true, reasoning_tokens);
        let response = server
            .client_oneshot(request(json!({"model":"private-chat","input":"Hello"})))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let response = gateway::response_json(response).await;
        assert_eq!(response["output"][0]["type"], "reasoning");
        assert_eq!(response["output"][0]["status"], "completed");
        assert_eq!(
            response["output"][0]["summary"],
            json!([{"type":"summary_text","text":"Summary."}])
        );
        assert_eq!(response["output"][1]["content"][0]["text"], "Answer.");
        assert_eq!(response["output"][2]["arguments"], "{}");
        if reasoning_tokens.is_some() {
            assert_eq!(
                response["usage"]["output_tokens_details"]["reasoning_tokens"],
                0
            );
        } else {
            assert!(response["usage"].get("output_tokens_details").is_none());
        }
    }
}

#[tokio::test]
async fn private_stream_summary_events_keep_interleaved_item_indices() {
    let server = summary_server("Summary.".into(), "Answer.".into(), true, Some(0));
    let response = server
        .client_oneshot(request(
            json!({"model":"private-chat","input":"Hello","stream":true}),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let frames = events(response).await;
    let output = &frames.last().expect("terminal")["response"]["output"];
    assert_eq!(output[0]["content"][0]["text"], "Answer. Ready.");
    assert_eq!(output[1]["arguments"], "{}");
    assert_eq!(output[2]["type"], "reasoning");
    assert_eq!(output[2]["summary"][0]["text"], "Summary. More.");
    let summary_id = &output[2]["id"];
    let summary_frames: Vec<_> = frames
        .iter()
        .filter(|frame| {
            frame["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("response.reasoning_summary_"))
        })
        .collect();
    assert_eq!(
        summary_frames
            .iter()
            .map(|frame| frame["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.done",
            "response.reasoning_summary_part.done",
        ]
    );
    for frame in summary_frames {
        assert_eq!(frame["output_index"], 2);
        assert_eq!(frame["summary_index"], 0);
        assert_eq!(&frame["item_id"], summary_id);
    }
    let item_done = frames
        .iter()
        .position(|frame| {
            frame["type"] == "response.output_item.done" && frame["output_index"] == 2
        })
        .expect("reasoning item done");
    assert_eq!(
        frames[item_done - 1]["type"],
        "response.reasoning_summary_part.done"
    );
    assert_eq!(frames[item_done]["item"], output[2]);
    assert_eq!(
        frames.last().unwrap()["response"]["usage"]["output_tokens_details"]["reasoning_tokens"],
        0
    );
}

#[tokio::test]
async fn public_complete_and_stream_do_not_expose_summary_items_or_events() {
    for stream in [false, true] {
        let server = summary_server(
            "PRIVATE_SUMMARY_MARKER".into(),
            "Answer.".into(),
            true,
            Some(0),
        );
        let response = server
            .public_oneshot(request(
                json!({"model":"private-chat","input":"Hello","stream":stream}),
            ))
            .await
            .expect("public response");
        assert_eq!(response.status(), StatusCode::OK);
        let response = if stream {
            let frames = events(response).await;
            assert!(!frames.iter().any(|frame| {
                frame["type"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("response.reasoning_summary_"))
            }));
            assert!(
                !serde_json::to_string(&frames)
                    .unwrap()
                    .contains("PRIVATE_SUMMARY_MARKER")
            );
            frames.last().unwrap()["response"].clone()
        } else {
            gateway::response_json(response).await
        };
        assert!(!response.to_string().contains("PRIVATE_SUMMARY_MARKER"));
        assert_eq!(response["output"].as_array().unwrap().len(), 2);
        assert_eq!(response["output"][0]["type"], "message");
        assert_eq!(response["output"][1]["type"], "function_call");
        assert!(response["reasoning"]["summary"].is_null());
    }
}

#[tokio::test]
async fn summary_output_obeys_utf8_cap_and_combined_output_limit() {
    const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
    let cap = kanata::core::MAX_REASONING_BYTES;
    let summary = format!("{}éignored", "s".repeat(cap - 1));
    for stream in [false, true] {
        for extra in [0, 1] {
            let server = summary_server(
                summary.clone(),
                "x".repeat(OUTPUT_LIMIT - (cap - 1) + extra),
                false,
                None,
            );
            let response = server
                .client_oneshot(request(
                    json!({"model":"private-chat","input":"Hello","stream":stream}),
                ))
                .await
                .expect("response");
            if !stream && extra == 1 {
                assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
                continue;
            }
            assert_eq!(response.status(), StatusCode::OK);
            let response = if stream {
                let frames = events(response).await;
                assert_eq!(
                    frames.last().unwrap()["type"],
                    if extra == 1 {
                        "response.failed"
                    } else {
                        "response.completed"
                    }
                );
                if extra == 1 {
                    continue;
                }
                frames.last().unwrap()["response"].clone()
            } else {
                gateway::response_json(response).await
            };
            let summary = response["output"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["type"] == "reasoning")
                .expect("reasoning")["summary"][0]["text"]
                .as_str()
                .unwrap();
            assert_eq!(summary.len(), cap - 1);
            assert!(summary.bytes().all(|byte| byte == b's'));
        }
    }
}

#[tokio::test]
async fn returned_summary_items_can_be_replayed_in_stateless_history() {
    let server = summary_server("Summary.".into(), "Answer.".into(), false, None);
    let response = gateway::response_json(
        server
            .client_oneshot(request(json!({"model":"private-chat","input":"Hello"})))
            .await
            .expect("response"),
    )
    .await;
    let mut history = vec![json!({"role":"user","content":"Hello"})];
    history.extend(response["output"].as_array().unwrap().iter().cloned());
    history.push(json!({"role":"user","content":"Continue"}));
    let (server, requests) = text_server();
    assert_eq!(
        server
            .client_oneshot(request(json!({"model":"private-chat","input":history})))
            .await
            .expect("replay")
            .status(),
        StatusCode::OK
    );
    let routed = gateway::take_request(&requests);
    let CoreRequest::Chat(chat) = routed.request() else {
        panic!("chat request");
    };
    assert_eq!(chat.messages.len(), 3);
    assert_eq!(chat.messages[1], gateway::assistant_text("Answer."));
    assert!(
        !serde_json::to_string(&chat.messages)
            .unwrap()
            .contains("Summary.")
    );
    for (item, param) in [
        (
            json!({"type":"reasoning","summary":[{"type":"summary_text","text":"Summary."}],"encrypted_content":7}),
            "input[1].encrypted_content",
        ),
        (
            json!({"type":"reasoning","summary":[{"type":"summary_text","text":7}]}),
            "input[1].summary",
        ),
        (
            json!({"type":"reasoning","summary":[{"type":"reasoning_text","text":"Summary."}]}),
            "input[1].summary",
        ),
    ] {
        let response = server
            .client_oneshot(request(
                json!({"model":"private-chat","input":[{"role":"user","content":"Hello"},item]}),
            ))
            .await
            .expect("malformed replay");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            gateway::response_json(response).await["error"]["param"],
            param
        );
        assert_eq!(gateway::recorded_len(&requests), 0);
    }
}

#[tokio::test]
async fn reasoning_only_length_stop_finishes_summary_as_incomplete() {
    let (server, _) = sse::server(vec![
        start(),
        Ok(NormalizedEvent::ChatReasoningDelta {
            text: "Partial summary.".into(),
        }),
        completed(FinishReason::Length),
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
    let done = frames
        .iter()
        .find(|frame| frame["type"] == "response.reasoning_summary_text.done")
        .expect("summary done");
    assert_eq!(done["text"], "Partial summary.");
    assert_eq!(
        frames
            .iter()
            .find(|frame| frame["type"] == "response.reasoning_summary_part.done")
            .expect("part done")["status"],
        "incomplete"
    );
    let terminal = frames.last().unwrap();
    assert_eq!(terminal["type"], "response.incomplete");
    assert_eq!(terminal["response"]["output"][0]["status"], "incomplete");
    assert_eq!(
        terminal["response"]["incomplete_details"]["reason"],
        "max_output_tokens"
    );
}

#[tokio::test]
async fn summary_is_followed_by_the_answer_or_function_call_even_when_empty() {
    for stream in [false, true] {
        for with_tool in [false, true] {
            let server = summary_server("Summary.".into(), String::new(), with_tool, None);
            let response = server
                .client_oneshot(request(
                    json!({"model":"private-chat","input":"Hello","stream":stream}),
                ))
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            let response = if stream {
                let frames = events(response).await;
                let terminal = frames.last().unwrap();
                assert_eq!(terminal["type"], "response.completed");
                terminal["response"].clone()
            } else {
                gateway::response_json(response).await
            };
            let output = response["output"].as_array().expect("output items");
            let kinds: Vec<_> = output.iter().map(|item| item["type"].clone()).collect();
            assert!(kinds.contains(&json!("reasoning")));
            if with_tool {
                assert!(kinds.contains(&json!("function_call")));
            } else {
                let message = output
                    .iter()
                    .find(|item| item["type"] == "message")
                    .expect("empty answer message");
                assert_eq!(message["content"][0]["text"], "");
            }
        }
    }
}

/// Streams a fixed reply and records every routed request.
struct HarnessAdapter {
    capabilities: kanata::core::Capabilities,
    requests: gateway::Requests,
}

impl kanata::adapter::Adapter for HarnessAdapter {
    fn id(&self) -> &str {
        "vllm-private"
    }

    fn capabilities(&self) -> &kanata::core::Capabilities {
        &self.capabilities
    }

    fn execute(&self, routed: kanata::core::RoutedRequest) -> kanata::adapter::AdapterFuture {
        self.requests.lock().expect("request lock").push(routed);
        Box::pin(async move {
            Ok(kanata::adapter::AdapterOutput::Events(Box::pin(
                futures_util::stream::iter(vec![
                    start(),
                    Ok(NormalizedEvent::ChatTextDelta { text: "ok".into() }),
                    completed(FinishReason::Stop),
                ]),
            )))
        })
    }
}

/// The example config with sampling controls on the private chat route, as harnesses send
/// output-token limits.
fn harness_server() -> gateway::ServerWithRequests {
    let contents = std::fs::read_to_string("tests/fixtures/config/example.toml")
        .expect("example config")
        .replacen(
            "operations = [\"chat\", \"transcription\"]\nstreaming_chat = true\nfunction_tools = true\n",
            "operations = [\"chat\", \"transcription\"]\nstreaming_chat = true\nfunction_tools = true\nsampling_controls = true\n",
            1,
        );
    let config = gateway::load_text(&contents).expect("harness config validates");
    let requests = gateway::Requests::default();
    let adapter = HarnessAdapter {
        capabilities: gateway::capabilities(&config, "vllm-private"),
        requests: requests.clone(),
    };
    assert!(adapter.capabilities.sampling_controls);
    let server = kanata::server::TwoPlaneServer::from_validated_with_adapters(
        &config,
        &gateway::Resolver,
        kanata::server::Readiness::new(true),
        vec![std::sync::Arc::new(adapter)],
    )
    .expect("harness server");
    (server, requests)
}

fn harness_fixture(name: &str) -> Value {
    let path = format!("tests/fixtures/harness/{name}.json");
    serde_json::from_str(&std::fs::read_to_string(&path).expect("fixture reads")).expect(&path)
}

fn harness_request(path: &str, body: &Value) -> Request<Body> {
    let mut request = gateway::chat_request(&body.to_string());
    *request.uri_mut() = path.parse().expect("uri");
    request
}

#[tokio::test]
async fn recorded_harness_request_shapes_are_accepted() {
    let (server, requests) = harness_server();
    for (name, path) in [
        ("omp_chat", "/v1/chat/completions"),
        ("opencode", "/v1/chat/completions"),
        ("omp_responses", "/v1/responses"),
        ("codex_t1", "/v1/responses"),
        ("codex_t2", "/v1/responses"),
    ] {
        let mut body = harness_fixture(name);
        // Hosted and grouped tools are rejected by design; Codex can turn them off.
        if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
            tools.retain(|tool| tool["type"] == "function");
        }
        let response = server
            .client_oneshot(harness_request(path, &body))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        gateway::response_body(response).await;
        let routed = gateway::take_request(&requests);
        let CoreRequest::Chat(chat) = routed.request() else {
            panic!("chat");
        };
        assert_eq!(
            chat.prompt_cache_key.is_some(),
            body.get("prompt_cache_key").is_some(),
            "{name}"
        );
        assert_eq!(
            chat.include_encrypted_reasoning,
            name.starts_with("codex"),
            "{name}"
        );
        if name == "codex_t2" {
            assert!(matches!(
                &chat.messages.last().expect("tool result").content[..],
                [ChatContent::ToolResult { content, .. }] if content == "TEST_ONLY tool output"
            ));
        }
    }

    let response = server
        .client_oneshot(harness_request(
            "/v1/responses",
            &harness_fixture("codex_t1"),
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        gateway::response_json(response).await["error"]["param"],
        "tools[4]"
    );
    assert_eq!(gateway::recorded_len(&requests), 0);
}
