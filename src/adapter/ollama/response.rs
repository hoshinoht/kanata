use std::collections::BTreeSet;

use serde::Deserialize;

use crate::core::{
    ChatContent, ChatMessage, ChatResponse, ChatRole, FinishReason, ModelAlias, ToolCall, Usage,
};

use super::super::super::core::{ErrorKind, GatewayError};

const MAX_TOOL_CALLS: usize = 64;
const MAX_TOOL_CALL_ID_BYTES: usize = 128;
const MAX_TOOL_NAME_BYTES: usize = 64;

#[derive(Deserialize)]
struct CompletionPayload {
    choices: Vec<ChoicePayload>,
    #[serde(default)]
    usage: Option<UsagePayload>,
}

#[derive(Deserialize)]
struct ChoicePayload {
    index: u64,
    message: MessagePayload,
    finish_reason: String,
}

#[derive(Deserialize)]
struct MessagePayload {
    role: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallPayload>>,
}

#[derive(Deserialize)]
struct ToolCallPayload {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: FunctionCallPayload,
}

#[derive(Deserialize)]
struct FunctionCallPayload {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct UsagePayload {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

pub(super) fn decode(bytes: &[u8], public_model: ModelAlias) -> Result<ChatResponse, GatewayError> {
    let payload: CompletionPayload =
        serde_json::from_slice(bytes).map_err(|_| upstream_failure())?;
    if payload.choices.len() != 1 {
        return Err(upstream_failure());
    }
    let choice = payload
        .choices
        .into_iter()
        .next()
        .ok_or_else(upstream_failure)?;
    if choice.index != 0 || choice.message.role != "assistant" {
        return Err(upstream_failure());
    }
    let finish_reason = parse_finish_reason(&choice.finish_reason)?;
    let mut content = Vec::new();
    if let Some(text) = choice.message.content
        && !text.is_empty()
    {
        content.push(ChatContent::Text { text });
    }
    let calls = choice.message.tool_calls.unwrap_or_default();
    if calls.len() > MAX_TOOL_CALLS {
        return Err(upstream_failure());
    }
    let mut ids = BTreeSet::new();
    for call in calls {
        if call.kind != "function"
            || !valid_call_id(&call.id)
            || !ids.insert(call.id.clone())
            || !valid_tool_name(&call.function.name)
        {
            return Err(upstream_failure());
        }
        content.push(ChatContent::ToolCall {
            call: ToolCall {
                id: call.id,
                name: call.function.name,
                arguments: call.function.arguments,
            },
        });
    }
    if content.is_empty() {
        return Err(upstream_failure());
    }
    let has_tool_calls = content
        .iter()
        .any(|item| matches!(item, ChatContent::ToolCall { .. }));
    if !finish_matches_tool_calls(finish_reason, has_tool_calls) {
        return Err(upstream_failure());
    }
    let usage = payload.usage.map(|usage| Usage {
        input_tokens: usage.prompt_tokens,
        output_tokens: usage.completion_tokens,
        total_tokens: usage.total_tokens,
    });
    Ok(ChatResponse {
        model: public_model,
        message: ChatMessage {
            role: ChatRole::Assistant,
            content,
        },
        finish_reason,
        usage,
    })
}

fn parse_finish_reason(value: &str) -> Result<FinishReason, GatewayError> {
    match value {
        "stop" => Ok(FinishReason::Stop),
        "length" => Ok(FinishReason::Length),
        "tool_calls" => Ok(FinishReason::ToolCalls),
        "content_filter" => Ok(FinishReason::ContentFilter),
        _ => Err(upstream_failure()),
    }
}

fn finish_matches_tool_calls(reason: FinishReason, has_tool_calls: bool) -> bool {
    match reason {
        FinishReason::Stop => !has_tool_calls,
        FinishReason::ToolCalls => has_tool_calls,
        FinishReason::Length | FinishReason::ContentFilter => true,
        FinishReason::Cancelled => false,
    }
}

fn valid_call_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOOL_CALL_ID_BYTES
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn valid_tool_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOOL_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}
