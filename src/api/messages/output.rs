use axum::{
    Json,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use super::super::errors::gateway_error_observed;
use crate::{
    core::{
        ChatContent, ChatResponse, ChatRole, ErrorKind, FinishReason, GatewayError, ModelAlias,
        Usage,
    },
    telemetry::Observer,
};

pub(in crate::api) fn complete(
    response: ChatResponse,
    model: &ModelAlias,
    id: String,
    observer: Option<&Observer>,
) -> Response {
    let usage_json = usage(response.usage.as_ref());
    let result = (|| {
        if response.model != *model || response.message.role != ChatRole::Assistant {
            return Err(failure());
        }
        let has_calls = response
            .message
            .content
            .iter()
            .any(|content| matches!(content, ChatContent::ToolCall { .. }));
        let stop_reason = stop_reason(response.finish_reason, has_calls)?;
        let mut content: Vec<Value> = Vec::new();
        let mut calls = std::collections::BTreeSet::new();
        for item in response.message.content {
            match item {
                ChatContent::Text { text } if text.is_empty() => {}
                ChatContent::Text { text } => match content.last_mut() {
                    Some(Value::Object(block)) if block["type"] == "text" => {
                        if let Some(Value::String(current)) = block.get_mut("text") {
                            current.push_str(&text);
                        }
                    }
                    _ => content.push(json!({"type":"text","text":text})),
                },
                ChatContent::ToolCall { call } => {
                    if !valid_call(&call.id, &call.name) || !calls.insert(call.id.clone()) {
                        return Err(failure());
                    }
                    let input = tool_input(&call.arguments)?;
                    content.push(
                        json!({"type":"tool_use","id":call.id,"name":call.name,"input":input}),
                    );
                }
                // Encrypted reasoning is never carried through this endpoint.
                ChatContent::Reasoning { .. } => {}
                _ => return Err(failure()),
            }
        }
        Ok(json!({
            "id":id,"type":"message","role":"assistant","model":model.0,"content":content,
            "stop_reason":stop_reason,"stop_sequence":null,"usage":usage_json
        }))
    })();
    match result {
        Ok(body) => {
            if let Some(observer) = observer {
                observer.first_content();
                observer.record_usage(response.usage);
            }
            Json(body).into_response()
        }
        Err(error) => gateway_error_observed(error, observer),
    }
}

/// `Cancelled` is a gateway error, never a stop reason. A reply with no content blocks is
/// valid for every other reason; a tool-use stop without calls is rejected here.
pub(super) fn stop_reason(
    reason: FinishReason,
    has_tool_calls: bool,
) -> Result<&'static str, GatewayError> {
    if !super::super::serialization::finish_matches_tool_calls(reason, has_tool_calls) {
        return Err(failure());
    }
    match reason {
        FinishReason::Stop => Ok("end_turn"),
        FinishReason::Length => Ok("max_tokens"),
        FinishReason::ToolCalls => Ok("tool_use"),
        FinishReason::ContentFilter => Ok("refusal"),
        FinishReason::Cancelled => Err(GatewayError {
            kind: ErrorKind::Cancelled,
        }),
    }
}

/// `input_tokens` excludes the cached prefix, reported apart as `cache_read_input_tokens`.
/// Counts the upstream did not report are zero, never estimated.
pub(super) fn usage(usage: Option<&Usage>) -> Value {
    let Some(usage) = usage else {
        return json!({"input_tokens":0,"output_tokens":0});
    };
    let cached = usage.cached_input_tokens;
    let mut value = json!({
        "input_tokens":usage.input_tokens.saturating_sub(cached.unwrap_or(0)),
        "output_tokens":usage.output_tokens
    });
    if let Some(cached) = cached {
        value["cache_read_input_tokens"] = json!(cached);
    }
    value
}

/// Tool arguments as a `tool_use.input` object; an empty argument string is `{}`.
pub(super) fn tool_input(arguments: &str) -> Result<Value, GatewayError> {
    if arguments.is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_str(arguments) {
        Ok(value @ Value::Object(_)) => Ok(value),
        _ => Err(failure()),
    }
}

pub(super) fn valid_call(id: &str, name: &str) -> bool {
    (1..=128).contains(&id.len())
        && id.bytes().all(|byte| byte.is_ascii_graphic())
        && (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

pub(super) fn failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}
