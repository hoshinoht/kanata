use std::sync::atomic::{AtomicU64, Ordering};

use axum::{
    Json,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use crate::core::{
    ChatContent, ChatMessage, ChatResponse, ChatRole, ErrorKind, FinishReason, GatewayError,
    MAX_REASONING_BYTES, ModelAlias, Usage,
};
use crate::telemetry::Observer;

use super::errors::{gateway_error_observed, upstream_failure_observed};

static NEXT_CHAT_ID: AtomicU64 = AtomicU64::new(1);

pub(super) fn chat_response(
    response: ChatResponse,
    public_model: &ModelAlias,
    expose_reasoning: bool,
    observer: Option<&Observer>,
) -> Response {
    if response.model != *public_model || response.message.role != ChatRole::Assistant {
        return upstream_failure_observed(observer);
    }
    if !finish_matches_tool_calls(
        response.finish_reason,
        response
            .message
            .content
            .iter()
            .any(|content| matches!(content, ChatContent::ToolCall { .. })),
    ) {
        return upstream_failure_observed(observer);
    }
    let Some(finish_reason) = finish(response.finish_reason) else {
        return gateway_error_observed(
            GatewayError {
                kind: ErrorKind::Cancelled,
            },
            observer,
        );
    };
    let Some(mut message) = response_message(&response.message) else {
        return upstream_failure_observed(observer);
    };
    if expose_reasoning
        && let Some(mut reasoning) = response.reasoning.filter(|text| !text.is_empty())
    {
        truncate_at_char_boundary(&mut reasoning, MAX_REASONING_BYTES);
        message["reasoning_content"] = Value::String(reasoning);
    }
    if let Some(observer) = observer {
        observer.first_content();
        observer.record_usage(response.usage.clone());
    }
    Json(json!({
        "id":next_chat_id(),
        "object":"chat.completion",
        "created":0,
        "model":public_model.0,
        "choices":[{"index":0,"message":message,"finish_reason":finish_reason}],
        "usage":response.usage.map(usage)
    }))
    .into_response()
}

pub(super) fn next_chat_id() -> String {
    format!(
        "chatcmpl_kanata_{:016x}",
        NEXT_CHAT_ID.fetch_add(1, Ordering::Relaxed)
    )
}

/// A message with no output (an empty answer, a length stop spent on reasoning, or a
/// content-filter stop) has `content: ""`; callers reject a tool-call finish without calls.
fn response_message(message: &ChatMessage) -> Option<Value> {
    let mut text = String::new();
    let mut calls = Vec::new();
    for content in &message.content {
        match content {
            ChatContent::Text { text: value } => text.push_str(value),
            ChatContent::ToolCall { call } => {
                if call.id.is_empty() || call.name.is_empty() {
                    return None;
                }
                calls.push(json!({
                    "id":call.id,
                    "type":"function",
                    "function":{"name":call.name,"arguments":call.arguments}
                }));
            }
            ChatContent::ToolResult { .. }
            | ChatContent::InputAudio { .. }
            | ChatContent::InputImage { .. } => return None,
            // Reasoning travels in `ChatResponse::reasoning`/`reasoning_items`, not here.
            ChatContent::Reasoning { .. } | ChatContent::ReasoningText { .. } => {}
        }
    }
    if text.is_empty() && calls.is_empty() {
        return Some(json!({"role":"assistant","content":"","tool_calls":null}));
    }
    Some(json!({
        "role":"assistant",
        "content":if text.is_empty() { Value::Null } else { Value::String(text) },
        "tool_calls":if calls.is_empty() { Value::Null } else { Value::Array(calls) }
    }))
}

pub(super) fn usage(value: Usage) -> Value {
    let mut usage = json!({
        "prompt_tokens":value.input_tokens,
        "completion_tokens":value.output_tokens,
        "total_tokens":value.total_tokens
    });
    if let Some(tokens) = value.reasoning_tokens {
        usage["completion_tokens_details"] = json!({"reasoning_tokens":tokens});
    }
    // Reported, never estimated; listener exposure is decided before serialization.
    if let Some(tokens) = value.cached_input_tokens {
        usage["prompt_tokens_details"] = json!({"cached_tokens":tokens});
    }
    usage
}

pub(super) fn truncate_at_char_boundary(text: &mut String, max_bytes: usize) {
    if text.len() > max_bytes {
        let mut end = max_bytes;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
}

pub(super) fn finish(value: FinishReason) -> Option<&'static str> {
    match value {
        FinishReason::Stop => Some("stop"),
        FinishReason::Length => Some("length"),
        FinishReason::ToolCalls => Some("tool_calls"),
        FinishReason::ContentFilter => Some("content_filter"),
        FinishReason::Cancelled => None,
    }
}

pub(super) fn finish_matches_tool_calls(value: FinishReason, has_tool_calls: bool) -> bool {
    match value {
        FinishReason::Stop => !has_tool_calls,
        FinishReason::ToolCalls => has_tool_calls,
        FinishReason::Length | FinishReason::ContentFilter | FinishReason::Cancelled => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn chat_usage_reports_cached_tokens_only_when_known() {
        for (cached, expected) in [
            (Some(1024), Some(json!({"cached_tokens":1024}))),
            (None, None),
        ] {
            let response = ChatResponse {
                model: ModelAlias("fixture-model".into()),
                message: ChatMessage {
                    role: ChatRole::Assistant,
                    content: vec![ChatContent::Text {
                        text: "answer".into(),
                    }],
                },
                finish_reason: FinishReason::Stop,
                usage: Some(Usage {
                    input_tokens: 2048,
                    output_tokens: 3,
                    total_tokens: 2051,
                    reasoning_tokens: None,
                    cached_input_tokens: cached,
                }),
                reasoning: None,
                reasoning_items: Vec::new(),
            };
            let model = ModelAlias("fixture-model".into());
            let body = chat_response(response, &model, true, None).into_body();
            let value: Value =
                serde_json::from_slice(&axum::body::to_bytes(body, usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(
                value["usage"].get("prompt_tokens_details"),
                expected.as_ref()
            );
        }
    }
}
