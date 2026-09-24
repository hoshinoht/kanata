use axum::http::{StatusCode, header};
use kanata::core::{ChatContent, ChatRole, FinishReason, ToolCall, ToolChoice};
use serde_json::{Value, json};

use crate::support::{
    Outcome, adapter_spec, capabilities, chat_outcome, chat_request, chat_request_with, config,
    config_with_public_routes, core_chat, recorded_len, response_body, response_json, server_with,
    take_request,
};

const BASIC: &str = include_str!("../fixtures/openai/chat-basic.json");
const TOOLS: &str = include_str!("../fixtures/openai/chat-tools.json");

fn private_server(
    outcome: Outcome,
) -> (
    kanata::server::TwoPlaneServer,
    std::sync::Arc<std::sync::Mutex<Vec<kanata::core::RoutedRequest>>>,
) {
    let config = config();
    let capabilities = capabilities(&config, "vllm-private");
    server_with(
        &config,
        vec![adapter_spec("vllm-private", capabilities, outcome)],
    )
}

fn assistant_tool_message() -> kanata::core::ChatMessage {
    kanata::core::ChatMessage {
        role: ChatRole::Assistant,
        content: vec![ChatContent::ToolCall {
            call: ToolCall {
                id: "call_tool_marker".into(),
                name: "lookup".into(),
                arguments: "{\"q\":\"tool_marker\"}".into(),
            },
        }],
    }
}

#[tokio::test]
async fn nonstream_response_has_public_model_and_complete_openai_envelope() {
    let (server, requests) = private_server(chat_outcome("hello from the mock"));
    let response = server
        .client_oneshot(chat_request_with(
            BASIC,
            Some("application/json"),
            Some("Bearer test-key"),
            &["REQUEST_ID_MARKER"],
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let value = response_json(response).await;
    assert_eq!(value["object"], "chat.completion");
    assert_eq!(value["model"], "private-chat");
    assert_eq!(value["created"], 0);
    assert_eq!(value["choices"].as_array().map(Vec::len), Some(1));
    assert_eq!(value["choices"][0]["index"], 0);
    assert_eq!(value["choices"][0]["finish_reason"], "stop");
    assert_eq!(value["choices"][0]["message"]["role"], "assistant");
    assert_eq!(
        value["choices"][0]["message"]["content"],
        "hello from the mock"
    );
    assert_eq!(value["choices"][0]["message"]["tool_calls"], Value::Null);
    assert_eq!(value["usage"]["prompt_tokens"], 11);
    assert_eq!(value["usage"]["completion_tokens"], 7);
    assert_eq!(value["usage"]["total_tokens"], 18);
    let first_id = value["id"].as_str().expect("completion id").to_owned();
    assert!(first_id.starts_with("chatcmpl_kanata_"));
    assert!(!first_id.contains("REQUEST_ID_MARKER"));

    let second = server
        .client_oneshot(chat_request(BASIC))
        .await
        .expect("response");
    let second = response_json(second).await;
    assert_ne!(first_id, second["id"]);
    assert_eq!(recorded_len(&requests), 2);
}

#[tokio::test]
async fn nonstream_finish_reason_matches_returned_tool_calls() {
    let cases = [
        ("tool-calls-without-tools", FinishReason::ToolCalls, false),
        ("stop-with-tools", FinishReason::Stop, true),
        ("tool-calls-with-tools", FinishReason::ToolCalls, true),
        ("length-with-tools", FinishReason::Length, true),
        (
            "content-filter-with-tools",
            FinishReason::ContentFilter,
            true,
        ),
    ];
    for (name, finish_reason, has_tool_calls) in cases {
        let message = if has_tool_calls {
            assistant_tool_message()
        } else {
            crate::support::assistant_text("UPSTREAM_CONTENT_MARKER")
        };
        let (server, requests) = private_server(Outcome::Chat {
            model: None,
            message,
            finish_reason,
            usage: None,
        });
        let response = server
            .client_oneshot(chat_request(BASIC))
            .await
            .expect("response");
        let status = if matches!(
            (finish_reason, has_tool_calls),
            (FinishReason::ToolCalls, false) | (FinishReason::Stop, true)
        ) {
            StatusCode::BAD_GATEWAY
        } else {
            StatusCode::OK
        };
        assert_eq!(response.status(), status, "{name}");
        let body = response_body(response).await;
        if status == StatusCode::BAD_GATEWAY {
            assert!(!String::from_utf8_lossy(&body).contains("UPSTREAM_CONTENT_MARKER"));
        }
        assert_eq!(recorded_len(&requests), 1, "{name}");
    }
}

#[tokio::test]
async fn nonstream_empty_output_is_an_empty_reply_only_for_a_length_stop() {
    for (finish_reason, status) in [
        (FinishReason::Length, StatusCode::OK),
        (FinishReason::Stop, StatusCode::BAD_GATEWAY),
    ] {
        let (server, _requests) = private_server(Outcome::Chat {
            model: None,
            message: kanata::core::ChatMessage {
                role: ChatRole::Assistant,
                content: Vec::new(),
            },
            finish_reason,
            usage: None,
        });
        let response = server
            .client_oneshot(chat_request(BASIC))
            .await
            .expect("response");
        assert_eq!(response.status(), status, "{finish_reason:?}");
        if status == StatusCode::OK {
            let body = response_json(response).await;
            assert_eq!(body["choices"][0]["message"]["content"], json!(""));
            assert_eq!(body["choices"][0]["finish_reason"], json!("length"));
        }
    }
}

#[tokio::test]
async fn openai_messages_tools_and_tool_history_convert_to_core_ir() {
    let (server, requests) = private_server(chat_outcome("tool response"));
    let response = server
        .client_oneshot(chat_request(TOOLS))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let routed = take_request(&requests);
    let context = routed.context().clone();
    let chat = core_chat(routed);
    assert_eq!(context.route.selector.model_alias.0, "private-chat");
    assert_eq!(chat.messages.len(), 3);
    assert_eq!(chat.messages[0].role, ChatRole::User);
    assert_eq!(chat.tools.len(), 1);
    assert_eq!(chat.tools[0].name, "lookup");
    assert_eq!(chat.tools[0].description, None);
    assert_eq!(
        chat.tool_choice,
        ToolChoice::Function {
            name: "lookup".into()
        }
    );
    assert!(matches!(
        &chat.messages[1].content[0],
        ChatContent::ToolCall { call } if call.id == "call_lookup_1"
            && call.name == "lookup"
            && call.arguments == "{\"q\":\"kanata\"}"
    ));
    assert_eq!(
        chat.messages[2].content,
        vec![ChatContent::ToolResult {
            call_id: "call_lookup_1".into(),
            content: "result".into(),
        }]
    );
}

#[tokio::test]
async fn tool_history_requires_function_tools_even_without_new_declarations() {
    let config = config();
    let capabilities = capabilities(&config, "openrouter-remote");
    let (server, requests) = server_with(
        &config,
        vec![adapter_spec(
            "openrouter-remote",
            capabilities,
            chat_outcome("unused"),
        )],
    );
    let body = json!({
        "model": "remote-chat",
        "messages": [
            {"role": "user", "content": "look this up"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": {"name": "lookup", "arguments": "{}"}
            }]},
            {"role": "tool", "tool_call_id": "call_1", "content": "result"}
        ]
    })
    .to_string();
    let response = server
        .client_oneshot(chat_request(&body))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(recorded_len(&requests), 0);
}

#[tokio::test]
async fn strict_wire_rejections_are_zero_dispatch() {
    let cases = [
        (
            "named-choice-nested-unknown",
            r#"{"model":"private-chat","messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"lookup","parameters":{}}}],"tool_choice":{"type":"function","function":{"name":"lookup","future":true}}}"#,
        ),
        (
            "named-choice-unknown",
            r#"{"model":"private-chat","messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"lookup","parameters":{}}}],"tool_choice":{"type":"function","function":{"name":"lookup"},"future":true}}"#,
        ),
        (
            "image-content",
            r#"{"model":"private-chat","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/IMAGE_MARKER"}}]}]}"#,
        ),
        (
            "audio-content",
            r#"{"model":"private-chat","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"AUDIO_MARKER"}}]}]}"#,
        ),
        (
            "unsupported-sampling",
            r#"{"model":"private-chat","messages":[{"role":"user","content":"x"}],"temperature":0.2}"#,
        ),
        (
            "role-tool-field",
            r#"{"model":"private-chat","messages":[{"role":"user","content":"x","tool_calls":[]}] }"#,
        ),
        (
            "empty-message",
            r#"{"model":"private-chat","messages":[{"role":"user","content":""}]}"#,
        ),
        (
            "duplicate-tools",
            r#"{"model":"private-chat","messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"lookup","parameters":{}}},{"type":"function","function":{"name":"lookup","parameters":{}}}]}"#,
        ),
        (
            "invalid-tool-choice-reference",
            r#"{"model":"private-chat","messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"lookup","parameters":{}}}],"tool_choice":{"type":"function","function":{"name":"other"}}}"#,
        ),
        (
            "stream-options-on-nonstream",
            r#"{"model":"private-chat","messages":[{"role":"user","content":"x"}],"stream_options":{"include_usage":false}}"#,
        ),
        (
            "stream-options-null",
            r#"{"model":"private-chat","messages":[{"role":"user","content":"x"}],"stream_options":null}"#,
        ),
        (
            "tool-choice-null",
            r#"{"model":"private-chat","messages":[{"role":"user","content":"x"}],"tool_choice":null}"#,
        ),
    ];
    for (name, body) in cases {
        let (server, requests) = private_server(chat_outcome("unused"));
        let response = server
            .client_oneshot(chat_request(body))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
        let body = response_body(response).await;
        let body = String::from_utf8_lossy(&body);
        assert!(!body.contains("MARKER"), "{name} leaked request data");
        assert_eq!(recorded_len(&requests), 0, "{name} dispatched");
    }
}

#[tokio::test]
async fn chat_requires_json_content_type_and_rejects_duplicate_request_ids() {
    for content_type in [None, Some("text/plain"), Some("application/jsonish")] {
        let (server, requests) = private_server(chat_outcome("unused"));
        let response = server
            .client_oneshot(chat_request_with(
                BASIC,
                content_type,
                Some("Bearer test-key"),
                &[],
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(recorded_len(&requests), 0);
    }

    let (server, requests) = private_server(chat_outcome("unused"));
    let response = server
        .client_oneshot(chat_request_with(
            BASIC,
            Some("application/json"),
            Some("Bearer test-key"),
            &["first", "second"],
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(recorded_len(&requests), 0);

    let (server, requests) = private_server(chat_outcome("unused"));
    let response = server
        .client_oneshot(chat_request_with(
            BASIC,
            Some("application/json; charset=utf-8"),
            Some("Bearer test-key"),
            &["client-request-123"],
        ))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let routed = take_request(&requests);
    assert_eq!(routed.context().request_id, "client-request-123");
}

#[tokio::test]
async fn public_chat_dispatch_requires_its_exact_independent_allowlist_entry() {
    let config = config_with_public_routes(&[]);
    let default_capabilities = crate::support::capabilities(&config, "vllm-private");
    let (server, requests) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            default_capabilities,
            chat_outcome("unused"),
        )],
    );
    let denied = server
        .public_oneshot(chat_request(BASIC))
        .await
        .expect("configured public router");
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(recorded_len(&requests), 0);

    let config = config_with_public_routes(&[("private-chat", "chat")]);
    let capabilities = capabilities(&config, "vllm-private");
    let (server, requests) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            capabilities,
            chat_outcome("public chat"),
        )],
    );

    let allowed = server
        .public_oneshot(chat_request(BASIC))
        .await
        .expect("configured public router");
    assert_eq!(allowed.status(), StatusCode::OK);
    assert_eq!(recorded_len(&requests), 1);

    let not_allowlisted = json!({
        "model": "local-chat",
        "messages": [{ "role": "user", "content": "not public" }]
    })
    .to_string();
    let denied = server
        .public_oneshot(chat_request(&not_allowlisted))
        .await
        .expect("configured public router");
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(recorded_len(&requests), 1);

    let config = config_with_public_routes(&[("private-transcribe", "transcription")]);
    let asr_capabilities = crate::support::capabilities(&config, "vllm-private");
    let (server, requests) = server_with(
        &config,
        vec![adapter_spec(
            "vllm-private",
            asr_capabilities,
            chat_outcome("unused"),
        )],
    );
    let denied = server
        .public_oneshot(chat_request(BASIC))
        .await
        .expect("configured public router");
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(recorded_len(&requests), 0);
}
