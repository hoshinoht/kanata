use std::sync::atomic::{AtomicU64, Ordering};

use axum::{
    Json,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use crate::core::{
    ChatContent, ChatMessage, ChatResponse, ChatRole, ErrorKind, FinishReason, GatewayError,
    ModelAlias, Usage,
};
use crate::telemetry::Observer;

use super::errors::{gateway_error_observed, upstream_failure_observed};

static NEXT_CHAT_ID: AtomicU64 = AtomicU64::new(1);

pub(super) fn chat_response(
    response: ChatResponse,
    public_model: &ModelAlias,
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
    let allow_empty = matches!(
        response.finish_reason,
        FinishReason::Length | FinishReason::ContentFilter
    );
    let message = match response_message(&response.message, allow_empty) {
        Some(value) => value,
        None => return upstream_failure_observed(observer),
    };
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

/// `allow_empty` accepts a message with no output, e.g. a length stop spent on reasoning
/// or a content-filter stop.
fn response_message(message: &ChatMessage, allow_empty: bool) -> Option<Value> {
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
            ChatContent::ToolResult { .. } | ChatContent::InputAudio { .. } => return None,
        }
    }
    if text.is_empty() && calls.is_empty() {
        if !allow_empty {
            return None;
        }
        return Some(json!({"role":"assistant","content":"","tool_calls":null}));
    }
    Some(json!({
        "role":"assistant",
        "content":if text.is_empty() { Value::Null } else { Value::String(text) },
        "tool_calls":if calls.is_empty() { Value::Null } else { Value::Array(calls) }
    }))
}

pub(super) fn usage(value: Usage) -> Value {
    json!({
        "prompt_tokens":value.input_tokens,
        "completion_tokens":value.output_tokens,
        "total_tokens":value.total_tokens
    })
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
