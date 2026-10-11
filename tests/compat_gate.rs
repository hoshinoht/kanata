//! Wire-to-upstream gate for prompt caching and reasoning continuity: HTTP requests on
//! each listener reach a recorded replaying route as the IR the upstream payload is built
//! from, and reported cached-token counts leave only through the private listener.

#![allow(dead_code)]

#[path = "support/gateway.rs"]
mod gateway;
#[path = "messages_contract/support.rs"]
mod support;

use axum::http::StatusCode;
use kanata::core::{ChatContent, ChatRole};
use serde_json::{Value, json};

const CACHE_KEY: &str = "00000000-0000-4000-8000-00000000c0de";
const PRIOR_ENCRYPTED: &str = "PRIOR_ENCRYPTED_FIXTURE";

#[tokio::test]
async fn private_requests_reach_a_replaying_route_with_cache_key_include_and_reasoning() {
    let (server, recorded) = support::server();

    let chat = json!({"model":"codex-chat","messages":[{"role":"user","content":"hi"}],
        "prompt_cache_key":CACHE_KEY});
    let response = server
        .client_oneshot(support::post("/v1/chat/completions", &chat))
        .await
        .expect("chat response");
    assert_eq!(response.status(), StatusCode::OK);
    let request = support::last_request(&recorded);
    assert_eq!(
        request.prompt_cache_key.as_ref().map(|key| key.as_str()),
        Some(CACHE_KEY)
    );
    assert!(!request.include_encrypted_reasoning);

    let responses = json!({
        "model":"codex-chat","stream":false,"prompt_cache_key":CACHE_KEY,
        "include":["reasoning.encrypted_content"],
        "tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{}}}],
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"question"}]},
            {"type":"reasoning","id":"rs_fixture_prior","summary":[],"encrypted_content":PRIOR_ENCRYPTED},
            {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_1","output":""}
        ]
    });
    let response = server
        .client_oneshot(support::post("/v1/responses", &responses))
        .await
        .expect("responses response");
    assert_eq!(response.status(), StatusCode::OK);
    let request = support::last_request(&recorded);
    assert_eq!(
        request.prompt_cache_key.as_ref().map(|key| key.as_str()),
        Some(CACHE_KEY)
    );
    assert!(request.include_encrypted_reasoning);
    let roles: Vec<_> = request
        .messages
        .iter()
        .map(|message| message.role)
        .collect();
    assert_eq!(roles, [ChatRole::User, ChatRole::Assistant, ChatRole::Tool]);
    match &request.messages[1].content[..] {
        [
            ChatContent::Reasoning { item },
            ChatContent::ToolCall { call },
        ] => {
            assert_eq!(item.id(), Some("rs_fixture_prior"));
            assert_eq!(item.encrypted_content(), PRIOR_ENCRYPTED);
            assert_eq!(call.id, "call_1");
        }
        other => panic!("replayed assistant turn: {other:?}"),
    }
    assert_eq!(
        request.messages[2].content,
        [ChatContent::ToolResult {
            call_id: "call_1".into(),
            content: String::new(),
        }]
    );

    // The returned item is what the client replays next turn.
    let body = gateway::response_json(response).await;
    let reasoning = body["output"]
        .as_array()
        .expect("output items")
        .iter()
        .find(|item| item["type"] == "reasoning")
        .expect("reasoning item");
    assert_eq!(reasoning["id"], "rs_fixture_next");
    assert_eq!(reasoning["encrypted_content"], support::NEXT_ENCRYPTED);
    assert_eq!(
        body["usage"]["input_tokens_details"]["cached_tokens"],
        support::CACHED_TOKENS
    );
}

fn body(path: &str, stream: bool) -> Value {
    match path {
        "/v1/chat/completions" if stream => json!({"model":"private-chat","stream":true,
            "stream_options":{"include_usage":true},"messages":[{"role":"user","content":"hi"}]}),
        "/v1/chat/completions" => {
            json!({"model":"private-chat","messages":[{"role":"user","content":"hi"}]})
        }
        "/v1/responses" => json!({"model":"private-chat","input":"hi","stream":stream}),
        _ => json!({"model":"private-chat","max_tokens":64,"stream":stream,
            "messages":[{"role":"user","content":"hi"}]}),
    }
}

#[tokio::test]
async fn cached_token_counts_leave_only_through_the_private_listener() {
    let (server, _) = support::server();
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        for stream in [false, true] {
            for public in [false, true] {
                let request = support::post(path, &body(path, stream));
                let response = if public {
                    server.public_oneshot(request).await.expect("public router")
                } else {
                    server
                        .client_oneshot(request)
                        .await
                        .expect("private router")
                };
                let case = format!("{path} stream={stream} public={public}");
                assert_eq!(response.status(), StatusCode::OK, "{case}");
                let text = String::from_utf8(gateway::response_body(response).await.to_vec())
                    .expect("utf8 body");
                let (field, input) = if path == "/v1/messages" {
                    (
                        "cache_read_input_tokens",
                        support::INPUT_TOKENS - support::CACHED_TOKENS,
                    )
                } else {
                    ("cached_tokens", support::INPUT_TOKENS)
                };
                let cached = format!("\"{field}\":{}", support::CACHED_TOKENS);
                if public {
                    assert!(!text.contains("cached_tokens"), "{case}: {text}");
                    assert!(!text.contains("cache_read"), "{case}: {text}");
                    if path == "/v1/messages" {
                        let uncached = format!("\"input_tokens\":{}", support::INPUT_TOKENS);
                        assert!(text.contains(&uncached), "{case}: {text}");
                    }
                } else {
                    assert!(text.contains(&cached), "{case}: {text}");
                    if path == "/v1/messages" {
                        assert!(
                            text.contains(&format!("\"input_tokens\":{input}")),
                            "{case}"
                        );
                    }
                }
            }
        }
    }
}
