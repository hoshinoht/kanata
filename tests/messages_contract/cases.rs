use axum::http::{Method, Request, StatusCode, header};
use axum::{body::Body, response::Response};
use kanata::core::{ChatContent, ChatRole, ReasoningEffort, ToolCall};
use serde_json::{Value, json};

use crate::gateway::{response_body, response_json};
use crate::support::{
    self, CACHED_TOKENS, CALL_ID, INPUT_TOKENS, Recorded, last_request, post, post_with,
    sse_records,
};

const MESSAGES: &str = "/v1/messages?beta=true";
const CLAUDE_HEADERS: [(&str, &str); 3] = [
    ("x-api-key", "test-key"),
    ("anthropic-version", "2023-06-01"),
    ("anthropic-beta", "TEST_ONLY-beta"),
];

fn bash_tool() -> Value {
    json!({"name":"Bash","description":"TEST_ONLY tool",
        "input_schema":{"type":"object","properties":{"command":{"type":"string"}}}})
}

async fn send(server: &kanata::server::TwoPlaneServer, request: Request<Body>) -> Response {
    server.client_oneshot(request).await.expect("response")
}

fn event_names(records: &[(String, Value)]) -> Vec<&str> {
    records.iter().map(|(event, _)| event.as_str()).collect()
}

/// The tool call of a streamed or complete reply, as a `tool_use` block.
async fn tool_use(response: Response, stream: bool) -> Value {
    if !stream {
        let body = response_json(response).await;
        assert_eq!(body["stop_reason"], "tool_use");
        return body["content"][0].clone();
    }
    let records = sse_records(&response_body(response).await);
    let mut block = records
        .iter()
        .find(|(event, _)| event == "content_block_start")
        .expect("block start")
        .1["content_block"]
        .clone();
    let arguments: String = records
        .iter()
        .filter_map(|(_, data)| data["delta"]["partial_json"].as_str())
        .collect();
    block["input"] = serde_json::from_str(&arguments).expect("streamed input");
    let delta = records
        .iter()
        .find(|(event, _)| event == "message_delta")
        .expect("message delta");
    assert_eq!(delta.1["delta"]["stop_reason"], "tool_use");
    block
}

#[tokio::test]
async fn claude_fixture_returns_messages_shape_without_reasoning_control() {
    let (server, recorded) = support::server();
    let mut fixture = support::claude_fixture();
    fixture["stream"] = json!(false);
    let response = send(&server, post_with(MESSAGES, &fixture, &CLAUDE_HEADERS)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert!(body["id"].as_str().expect("id").starts_with("msg_req_"));
    assert_eq!(
        body,
        json!({
            "id":body["id"],"type":"message","role":"assistant","model":"private-chat",
            "content":[{"type":"tool_use","id":CALL_ID,"name":"Bash","input":{"command":"ls"}}],
            "stop_reason":"tool_use","stop_sequence":null,
            "usage":{"input_tokens":INPUT_TOKENS - CACHED_TOKENS,"output_tokens":3,
                "cache_read_input_tokens":CACHED_TOKENS}
        })
    );
    let request = last_request(&recorded);
    let roles: Vec<_> = request
        .messages
        .iter()
        .map(|message| message.role)
        .collect();
    assert_eq!(roles, [ChatRole::System, ChatRole::User, ChatRole::System]);
    assert_eq!(request.messages[0].content.len(), 3);
    assert_eq!(request.tools.len(), 3);
    assert_eq!(request.options.max_output_tokens, Some(32000));
    // The route has no reasoning control, so `output_config.effort` is not applied.
    assert_eq!(request.options.reasoning_effort, None);
    assert!(request.prompt_cache_key.is_none());

    fixture["stream"] = json!(true);
    let response = send(&server, post_with(MESSAGES, &fixture, &CLAUDE_HEADERS)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let records = sse_records(&response_body(response).await);
    assert_eq!(
        event_names(&records),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
    for (event, data) in &records {
        assert_eq!(data["type"], *event);
    }
    assert_eq!(
        records[0].1["message"]["usage"],
        json!({"input_tokens":0,"output_tokens":0})
    );
    assert_eq!(
        records[1].1["content_block"],
        json!({"type":"tool_use","id":CALL_ID,"name":"Bash","input":{}})
    );
    assert_eq!(records[2].1["delta"]["type"], "input_json_delta");
    assert_eq!(
        records[5].1,
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},
            "usage":{"input_tokens":INPUT_TOKENS - CACHED_TOKENS,"output_tokens":3,
                "cache_read_input_tokens":CACHED_TOKENS}})
    );
}

#[tokio::test]
async fn tool_loop_completes_in_both_modes() {
    for stream in [false, true] {
        let (server, recorded) = support::server();
        let mut messages = vec![json!({"role":"user","content":"TEST_ONLY list files"})];
        let first = json!({"model":"private-chat","max_tokens":1024,"stream":stream,
            "tools":[bash_tool()],"messages":messages});
        let response = send(&server, post(MESSAGES, &first)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let call = tool_use(response, stream).await;
        assert_eq!(
            call,
            json!({"type":"tool_use","id":CALL_ID,"name":"Bash","input":{"command":"ls"}})
        );

        messages.push(json!({"role":"assistant","content":[call]}));
        messages.push(json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":CALL_ID,"content":"a.txt","cache_control":{"type":"ephemeral","ttl":"5m"}}
        ]}));
        let second = json!({"model":"private-chat","max_tokens":1024,"stream":stream,
            "tools":[bash_tool()],"messages":messages});
        let response = send(&server, post(MESSAGES, &second)).await;
        assert_eq!(response.status(), StatusCode::OK);
        if stream {
            let records = sse_records(&response_body(response).await);
            assert_eq!(
                event_names(&records),
                [
                    "message_start",
                    "content_block_start",
                    "content_block_delta",
                    "content_block_delta",
                    "content_block_stop",
                    "message_delta",
                    "message_stop"
                ]
            );
            let text: String = records
                .iter()
                .filter_map(|(_, data)| data["delta"]["text"].as_str())
                .collect();
            assert_eq!(text, "answer");
            assert_eq!(records[5].1["delta"]["stop_reason"], "end_turn");
        } else {
            let body = response_json(response).await;
            assert_eq!(body["content"], json!([{"type":"text","text":"answer"}]));
            assert_eq!(body["stop_reason"], "end_turn");
        }
        let request = last_request(&recorded);
        assert_eq!(
            request.messages[1].content,
            [ChatContent::ToolCall {
                call: ToolCall {
                    id: CALL_ID.into(),
                    name: "Bash".into(),
                    arguments: r#"{"command":"ls"}"#.into(),
                },
            }]
        );
        assert_eq!(request.messages[2].role, ChatRole::Tool);
        assert_eq!(
            request.messages[2].content,
            [ChatContent::ToolResult {
                call_id: CALL_ID.into(),
                content: "a.txt".into(),
            }]
        );
    }
}

fn simple(stream: bool) -> Value {
    json!({"model":"private-chat","max_tokens":64,"stream":stream,
        "messages":[{"role":"user","content":"hi"}]})
}

#[tokio::test]
async fn api_key_header_authenticates_on_both_listeners() {
    let (server, recorded) = support::server();
    for headers in [
        vec![("x-api-key", "test-key")],
        vec![
            ("authorization", "Bearer test-key"),
            ("x-api-key", "test-key"),
        ],
    ] {
        let response = send(&server, post_with(MESSAGES, &simple(false), &headers)).await;
        assert_eq!(response.status(), StatusCode::OK, "{headers:?}");
        let response = server
            .public_oneshot(post_with(MESSAGES, &simple(false), &headers))
            .await
            .expect("public router");
        assert_eq!(response.status(), StatusCode::OK, "{headers:?}");
    }
    // Every endpoint takes the same carrier.
    let chat = json!({"model":"private-chat","messages":[{"role":"user","content":"hi"}]});
    let response = send(
        &server,
        post_with("/v1/chat/completions", &chat, &[("x-api-key", "test-key")]),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let dispatched = recorded.lock().expect("record lock").len();

    for headers in [
        vec![
            ("authorization", "Bearer test-key"),
            ("x-api-key", "other-key"),
        ],
        vec![("x-api-key", "test-key"), ("x-api-key", "test-key")],
    ] {
        let response = send(&server, post_with(MESSAGES, &simple(false), &headers)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{headers:?}");
        assert_eq!(
            response.headers()[header::WWW_AUTHENTICATE],
            "Bearer realm=\"kanata\""
        );
        assert_eq!(
            response_json(response).await,
            json!({"type":"error","error":{"type":"authentication_error",
                "message":"Invalid authentication credentials","code":"invalid_api_key","param":null}})
        );
        // Public authentication runs before routing and keeps the default envelope.
        let response = server
            .public_oneshot(post_with(MESSAGES, &simple(false), &headers))
            .await
            .expect("public router");
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{headers:?}");
        assert_eq!(
            response_json(response).await["error"]["code"],
            "permission_denied"
        );
    }
    assert_eq!(recorded.lock().expect("record lock").len(), dispatched);
}

#[tokio::test]
async fn public_listener_refuses_unpublished_aliases_like_chat() {
    let (server, recorded) = support::server();
    for model in ["codex-chat", "local-chat"] {
        let chat = json!({"model":model,"messages":[{"role":"user","content":"hi"}]});
        let chat = server
            .public_oneshot(post("/v1/chat/completions", &chat))
            .await
            .expect("public router");
        let mut messages = simple(false);
        messages["model"] = json!(model);
        let messages = server
            .public_oneshot(post(MESSAGES, &messages))
            .await
            .expect("public router");
        assert_eq!(chat.status(), StatusCode::FORBIDDEN, "{model}");
        assert_eq!(messages.status(), StatusCode::FORBIDDEN, "{model}");
        assert_eq!(
            response_json(chat).await["error"]["code"],
            "permission_denied"
        );
        assert_eq!(
            response_json(messages).await["error"],
            json!({"type":"permission_error","message":"Permission denied",
                "code":"permission_denied","param":null})
        );
    }
    assert!(recorded.lock().expect("record lock").is_empty());
}

fn recorded_len(recorded: &Recorded) -> usize {
    recorded.lock().expect("record lock").len()
}

#[tokio::test]
async fn errors_use_the_messages_envelope_and_keep_headers() {
    let (server, recorded) = support::server();
    let mut body = simple(false);
    body["top_k"] = json!(5);
    let response = send(&server, post(MESSAGES, &body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_json(response).await,
        json!({"type":"error","error":{"type":"invalid_request_error","message":"Invalid request",
            "code":"invalid_request","param":"top_k"}})
    );

    let mut body = simple(true);
    body["messages"][0]["content"] = json!([{"type":"text","text":"hi","citations":null}]);
    let response = send(&server, post(MESSAGES, &body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_json(response).await["error"]["param"],
        "messages[0].content[0].citations"
    );

    // `GET` is answered by the router: 405 with `Allow`, no body to re-shape.
    let response = send(
        &server,
        Request::builder()
            .method(Method::GET)
            .uri("/v1/messages")
            .header("authorization", "Bearer test-key")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(
        response.headers()[header::ALLOW]
            .to_str()
            .expect("allow")
            .contains("POST")
    );
    assert!(response_body(response).await.is_empty());

    let response = send(&server, post("/v1/messages/count_tokens", &simple(false))).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(recorded_len(&recorded), 0);

    let (server, _) = support::server_with(&support::config_with(&[(
        "owner = true\n",
        "owner = true\nrate_limit = { requests = 1, per_ms = 60000 }\n",
    )]));
    let response = send(&server, post(MESSAGES, &simple(false))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = send(&server, post(MESSAGES, &simple(false))).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().contains_key(header::RETRY_AFTER));
    assert_eq!(
        response_json(response).await["error"],
        json!({"type":"rate_limit_error","message":"Request rate limit exceeded for this key",
            "code":"gateway_key_rate_limited","param":null})
    );
}

#[tokio::test]
async fn effort_applies_only_with_reasoning_control_and_max_tokens_clamps_to_the_cap() {
    let (server, recorded) = support::server_with(&support::config_with(&[
        (
            "function_tools = true\n\n[[adapters]]\nid = \"vllm-private\"",
            "function_tools = true\nsampling_controls = true\nreasoning_control = true\n\n[[adapters]]\nid = \"vllm-private\"",
        ),
        (
            "upstream_id = \"meta-llama/Meta-Llama-3.1-8B-Instruct\"\n",
            "upstream_id = \"meta-llama/Meta-Llama-3.1-8B-Instruct\"\nmax_output_tokens = 4096\n",
        ),
    ]));
    let mut body = simple(false);
    body["model"] = json!("local-chat");
    body["output_config"] = json!({"effort":"high"});
    body["thinking"] = json!({"type":"disabled"});
    let response = send(&server, post(MESSAGES, &body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        last_request(&recorded).options.reasoning_effort,
        Some(ReasoningEffort::High)
    );

    let mut body = simple(false);
    body["max_tokens"] = json!(32000);
    let response = send(&server, post(MESSAGES, &body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        last_request(&recorded).options.max_output_tokens,
        Some(4096)
    );

    // Chat keeps rejecting a limit above the cap.
    let chat = json!({"model":"private-chat","max_tokens":32000,
        "messages":[{"role":"user","content":"hi"}]});
    let response = send(&server, post("/v1/chat/completions", &chat)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_json(response).await["error"]["param"],
        "max_tokens"
    );
}

#[tokio::test]
async fn max_output_tokens_are_dropped_on_a_route_without_sampling_controls() {
    let (server, recorded) = support::server();
    let messages = json!({"model":"codex-chat","max_tokens":32000,
        "messages":[{"role":"user","content":"hi"}]});
    let responses = json!({"model":"codex-chat","input":"hi","max_output_tokens":64});
    for (path, body) in [(MESSAGES, messages), ("/v1/responses", responses)] {
        let response = send(&server, post(path, &body)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            last_request(&recorded).options.max_output_tokens,
            None,
            "{path}"
        );
    }
}

#[tokio::test]
async fn midstream_failure_is_an_error_event() {
    use futures_util::stream;
    use kanata::{
        adapter::{Adapter, AdapterFuture, AdapterOutput},
        core::{Capabilities, ErrorKind, GatewayError, NormalizedEvent, RoutedRequest},
        server::{Readiness, TwoPlaneServer},
    };

    struct Failing(Capabilities);
    impl Adapter for Failing {
        fn id(&self) -> &str {
            "vllm-private"
        }
        fn capabilities(&self) -> &Capabilities {
            &self.0
        }
        fn execute(&self, routed: RoutedRequest) -> AdapterFuture {
            let model = routed.request().model_alias().clone();
            Box::pin(async move {
                let events = vec![
                    Ok(NormalizedEvent::ChatStarted { model }),
                    Ok(NormalizedEvent::ChatTextDelta { text: "par".into() }),
                    Err(GatewayError {
                        kind: ErrorKind::UpstreamFailure,
                    }),
                ];
                Ok(AdapterOutput::Events(Box::pin(stream::iter(events))))
            })
        }
    }

    let config = support::config_with(&[]);
    let server = TwoPlaneServer::from_validated_with_adapters(
        &config,
        &crate::gateway::Resolver,
        Readiness::new(true),
        vec![std::sync::Arc::new(Failing(crate::gateway::capabilities(
            &config,
            "vllm-private",
        )))],
    )
    .expect("server");
    let response = send(&server, post(MESSAGES, &simple(true))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let records = sse_records(&response_body(response).await);
    let (event, data) = records.last().expect("records");
    assert_eq!(event, "error");
    assert_eq!(
        *data,
        json!({"type":"error","error":{"type":"api_error","message":"Upstream failure",
            "code":"upstream_failure","param":null}})
    );
    assert!(!event_names(&records).contains(&"message_stop"));
}

/// A completed reply with no text and no tool calls is an empty answer on every endpoint.
#[tokio::test]
async fn empty_stop_reply_is_an_empty_answer_on_every_endpoint() {
    use futures_util::stream;
    use kanata::{
        adapter::{Adapter, AdapterFuture, AdapterOutput},
        core::{
            Capabilities, ChatMessage, ChatResponse, FinishReason, NormalizedEvent, Request,
            Response as CoreResponse, RoutedRequest,
        },
        server::{Readiness, TwoPlaneServer},
    };

    struct Empty(Capabilities);
    impl Adapter for Empty {
        fn id(&self) -> &str {
            "vllm-private"
        }
        fn capabilities(&self) -> &Capabilities {
            &self.0
        }
        fn execute(&self, routed: RoutedRequest) -> AdapterFuture {
            let model = routed.request().model_alias().clone();
            let streaming = matches!(routed.request(), Request::Chat(chat) if chat.stream);
            Box::pin(async move {
                if streaming {
                    let events = vec![
                        Ok(NormalizedEvent::ChatStarted { model }),
                        Ok(NormalizedEvent::ChatCompleted {
                            finish_reason: FinishReason::Stop,
                            usage: None,
                        }),
                    ];
                    return Ok(AdapterOutput::Events(Box::pin(stream::iter(events))));
                }
                Ok(AdapterOutput::Complete(CoreResponse::Chat(ChatResponse {
                    model,
                    message: ChatMessage {
                        role: ChatRole::Assistant,
                        content: Vec::new(),
                    },
                    finish_reason: FinishReason::Stop,
                    usage: None,
                    reasoning: None,
                    reasoning_items: Vec::new(),
                })))
            })
        }
    }

    let config = support::config_with(&[]);
    let server = TwoPlaneServer::from_validated_with_adapters(
        &config,
        &crate::gateway::Resolver,
        Readiness::new(true),
        vec![std::sync::Arc::new(Empty(crate::gateway::capabilities(
            &config,
            "vllm-private",
        )))],
    )
    .expect("server");
    let empty_message = json!([{"type":"output_text","text":"","annotations":[],"logprobs":[]}]);
    for stream in [false, true] {
        let chat = json!({"model":"private-chat","stream":stream,
            "messages":[{"role":"user","content":"hi"}]});
        let responses = json!({"model":"private-chat","input":"hi","stream":stream});
        for (path, body) in [
            ("/v1/chat/completions", chat),
            ("/v1/responses", responses),
            (MESSAGES, simple(stream)),
        ] {
            let response = send(&server, post(path, &body)).await;
            assert_eq!(response.status(), StatusCode::OK, "{path} stream={stream}");
            let bytes = response_body(response).await;
            let records = sse_records(&bytes);
            let last = if stream {
                records.last().expect("records").1.clone()
            } else {
                serde_json::from_slice::<Value>(&bytes).expect("json body")
            };
            match (path, stream) {
                ("/v1/chat/completions", false) => {
                    assert_eq!(last["choices"][0]["message"]["content"], "");
                    assert_eq!(last["choices"][0]["finish_reason"], "stop");
                }
                ("/v1/chat/completions", true) => {
                    assert_eq!(last["choices"][0]["finish_reason"], "stop");
                    assert!(bytes.ends_with(b"data: [DONE]\n\n"));
                }
                ("/v1/responses", false) => {
                    assert_eq!(last["status"], "completed");
                    assert_eq!(last["output"][0]["content"], empty_message);
                }
                ("/v1/responses", true) => {
                    assert_eq!(last["type"], "response.completed");
                    assert_eq!(last["response"]["output"][0]["content"], empty_message);
                }
                (_, false) => {
                    assert_eq!(last["content"], json!([]));
                    assert_eq!(last["stop_reason"], "end_turn");
                }
                (_, true) => {
                    assert_eq!(
                        event_names(&records),
                        ["message_start", "message_delta", "message_stop"]
                    );
                    assert_eq!(records[1].1["delta"]["stop_reason"], "end_turn");
                }
            }
        }
    }
}

/// Each wire accepts its own empty answer back in history and drops that turn.
#[tokio::test]
async fn replayed_empty_answer_is_dropped_on_every_wire() {
    let (server, recorded) = support::server();
    let user = |text: &str| json!({"role":"user","content":text});
    let chat = |assistant: Value| {
        json!({"model":"private-chat","messages":[user("hi"),
            {"role":"assistant","content":assistant,"tool_calls":null},user("again")]})
    };
    let empty_output_text = json!({"type":"message","id":"resp_TEST_ONLY_msg","role":"assistant",
        "status":"completed",
        "content":[{"type":"output_text","text":"","annotations":[],"logprobs":[]}]});
    let messages = |assistant: Value| {
        json!({"model":"private-chat","max_tokens":64,"messages":[user("hi"),
            {"role":"assistant","content":assistant},user("again")]})
    };
    for (path, body) in [
        ("/v1/chat/completions", chat(json!(""))),
        ("/v1/chat/completions", chat(Value::Null)),
        ("/v1/chat/completions", chat(json!([]))),
        (
            "/v1/responses",
            json!({"model":"private-chat","input":[user("hi"),empty_output_text,user("again")]}),
        ),
        (MESSAGES, messages(json!(""))),
        (MESSAGES, messages(json!([]))),
        (MESSAGES, messages(json!([{"type":"text","text":""}]))),
    ] {
        let response = send(&server, post(path, &body)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path} {body}");
        let roles: Vec<_> = last_request(&recorded)
            .messages
            .iter()
            .map(|message| message.role)
            .collect();
        assert_eq!(roles, [ChatRole::User, ChatRole::User], "{path} {body}");
    }
}
