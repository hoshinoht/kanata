use crate::adapter::{
    chat_completions::response::{
        CompletionTokensDetails, assistant_content, chat_response, parse_finish_reason, usage,
    },
    upstream_failure,
};

use serde::Deserialize;

use crate::core::{ChatResponse, FinishReason, GatewayError, ModelAlias, ToolCall, Usage};

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
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
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

/// Lenient usage, in replies and on the final stream chunk; cached prompt tokens are not
/// read.
#[derive(Deserialize)]
pub(super) struct UsagePayload {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

impl From<UsagePayload> for Usage {
    fn from(payload: UsagePayload) -> Self {
        usage(
            payload.prompt_tokens,
            payload.completion_tokens,
            payload.total_tokens,
            None,
            payload.completion_tokens_details,
        )
    }
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
    let reasoning = choice
        .message
        .reasoning
        .or(choice.message.reasoning_content)
        .filter(|text| !text.is_empty());
    let calls = choice
        .message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|call| {
            (
                call.kind,
                ToolCall {
                    id: call.id,
                    name: call.function.name,
                    arguments: call.function.arguments,
                },
            )
        });
    let (content, has_tool_calls) = assistant_content(choice.message.content, calls)?;
    if content.is_empty() && finish_reason == FinishReason::ContentFilter {
        return Err(upstream_failure());
    }
    if !finish_matches_tool_calls(finish_reason, has_tool_calls) {
        return Err(upstream_failure());
    }
    let usage = payload.usage.map(Usage::from);
    Ok(chat_response(
        public_model,
        content,
        finish_reason,
        usage,
        reasoning,
    ))
}

fn finish_matches_tool_calls(reason: FinishReason, has_tool_calls: bool) -> bool {
    match reason {
        FinishReason::Stop => !has_tool_calls,
        FinishReason::ToolCalls => has_tool_calls,
        FinishReason::Length | FinishReason::ContentFilter => true,
        FinishReason::Cancelled => false,
    }
}
