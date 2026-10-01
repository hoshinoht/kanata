#![allow(dead_code)]

#[path = "support/gateway.rs"]
mod gateway;

use std::sync::Arc;

use axum::http::StatusCode;
use futures_util::stream;
use kanata::{
    adapter::{Adapter, AdapterFuture, AdapterOutput},
    core::{
        Capabilities, ChatResponse, FinishReason, NormalizedEvent, Request, Response,
        RoutedRequest, Usage,
    },
    server::{Readiness, TwoPlaneServer},
};
use serde_json::Value;

const MODEL: &str = "private-chat";
const REASONING: &str = "SYNTHETIC_REASONING_MARKER";

fn usage() -> Usage {
    Usage {
        input_tokens: 5,
        output_tokens: 9,
        total_tokens: 14,
        reasoning_tokens: Some(6),
    }
}

struct ReasoningAdapter {
    capabilities: Capabilities,
}

impl Adapter for ReasoningAdapter {
    fn id(&self) -> &str {
        "vllm-private"
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn execute(&self, routed: RoutedRequest) -> AdapterFuture {
        let model = routed.request().model_alias().clone();
        let streaming = matches!(routed.request(), Request::Chat(chat) if chat.stream);
        Box::pin(async move {
            if streaming {
                let events = vec![
                    Ok(NormalizedEvent::ChatStarted { model }),
                    Ok(NormalizedEvent::ChatReasoningDelta {
                        text: REASONING.into(),
                    }),
                    Ok(NormalizedEvent::ChatTextDelta {
                        text: "answer".into(),
                    }),
                    Ok(NormalizedEvent::ChatCompleted {
                        finish_reason: FinishReason::Stop,
                        usage: Some(usage()),
                    }),
                ];
                return Ok(AdapterOutput::Events(Box::pin(stream::iter(events))));
            }
            Ok(AdapterOutput::Complete(Response::Chat(ChatResponse {
                model,
                message: gateway::assistant_text("answer"),
                finish_reason: FinishReason::Stop,
                usage: Some(usage()),
                reasoning: Some(REASONING.into()),
            })))
        })
    }
}

fn server() -> TwoPlaneServer {
    let config = gateway::config_with_public_routes(&[(MODEL, "chat")]);
    let adapter = Arc::new(ReasoningAdapter {
        capabilities: gateway::capabilities(&config, "vllm-private"),
    });
    TwoPlaneServer::from_validated_with_adapters(
        &config,
        &gateway::Resolver,
        Readiness::new(true),
        vec![adapter],
    )
    .expect("server")
}

fn body(stream: bool) -> String {
    let options = if stream {
        r#","stream_options":{"include_usage":true}"#
    } else {
        ""
    };
    format!(
        r#"{{"model":"{MODEL}","messages":[{{"role":"user","content":"hi"}}],"stream":{stream}{options}}}"#
    )
}

fn sse_chunks(body: &[u8]) -> Vec<Value> {
    std::str::from_utf8(body)
        .expect("utf8 stream")
        .split("\n\n")
        .filter_map(|record| record.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("json chunk"))
        .collect()
}

#[tokio::test]
async fn private_listener_forwards_reasoning_and_its_token_count() {
    let response = server()
        .client_oneshot(gateway::chat_request(&body(false)))
        .await
        .expect("private response");
    assert_eq!(response.status(), StatusCode::OK);
    let json = gateway::response_json(response).await;
    assert_eq!(
        json["choices"][0]["message"]["reasoning_content"],
        REASONING
    );
    assert_eq!(json["choices"][0]["message"]["content"], "answer");
    assert_eq!(
        json["usage"]["completion_tokens_details"]["reasoning_tokens"],
        6
    );
}

#[tokio::test]
async fn public_listener_drops_reasoning_text() {
    let response = server()
        .public_oneshot(gateway::chat_request(&body(false)))
        .await
        .expect("public router");
    assert_eq!(response.status(), StatusCode::OK);
    let json = gateway::response_json(response).await;
    assert!(
        json["choices"][0]["message"]
            .get("reasoning_content")
            .is_none()
    );
    assert_eq!(json["choices"][0]["message"]["content"], "answer");
    assert!(!json.to_string().contains(REASONING));
}

#[tokio::test]
async fn private_stream_carries_reasoning_deltas_and_public_stream_does_not() {
    let private = server()
        .client_oneshot(gateway::chat_request(&body(true)))
        .await
        .expect("private stream");
    assert_eq!(private.status(), StatusCode::OK);
    let chunks = sse_chunks(&gateway::response_body(private).await);
    let reasoning: Vec<_> = chunks
        .iter()
        .filter_map(|chunk| chunk["choices"][0]["delta"]["reasoning_content"].as_str())
        .collect();
    assert_eq!(reasoning, [REASONING]);
    let usage = chunks
        .iter()
        .find(|chunk| !chunk["usage"].is_null())
        .expect("usage chunk");
    assert_eq!(
        usage["usage"]["completion_tokens_details"]["reasoning_tokens"],
        6
    );

    let public = server()
        .public_oneshot(gateway::chat_request(&body(true)))
        .await
        .expect("public router");
    assert_eq!(public.status(), StatusCode::OK);
    let body = gateway::response_body(public).await;
    assert!(!String::from_utf8_lossy(&body).contains(REASONING));
    let content: String = sse_chunks(&body)
        .iter()
        .filter_map(|chunk| chunk["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(content, "answer");
}
