use std::collections::BTreeSet;

use serde::Deserialize;

use crate::{
    adapter::upstream_failure,
    core::{
        ChatContent, ChatMessage, ChatResponse, ChatRole, FinishReason, GatewayError, ModelAlias,
        ToolCall, Usage,
    },
};

use super::{MAX_TOOL_CALLS, valid_call_id, valid_tool_name};

/// Strict `function` member of a returned tool call.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FunctionCallPayload {
    name: String,
    arguments: String,
}

impl FunctionCallPayload {
    /// The `(type, call)` pair `assistant_content` takes.
    pub(crate) fn into_call(self, kind: String, id: String) -> (String, ToolCall) {
        (
            kind,
            ToolCall {
                id,
                name: self.name,
                arguments: self.arguments,
            },
        )
    }
}

pub(crate) fn chat_response(
    model: ModelAlias,
    content: Vec<ChatContent>,
    finish_reason: FinishReason,
    usage: Option<Usage>,
    reasoning: Option<String>,
) -> ChatResponse {
    ChatResponse {
        model,
        message: ChatMessage {
            role: ChatRole::Assistant,
            content,
        },
        finish_reason,
        usage,
        reasoning,
        reasoning_items: Vec::new(),
    }
}

/// Other members (audio, cache writes) carry nothing Kanata reports.
#[derive(Deserialize)]
pub(crate) struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Deserialize)]
pub(crate) struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

pub(crate) fn usage(
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    prompt_details: Option<PromptTokensDetails>,
    completion_details: Option<CompletionTokensDetails>,
) -> Usage {
    let usage = Usage {
        input_tokens: prompt_tokens,
        output_tokens: completion_tokens,
        total_tokens,
        reasoning_tokens: completion_details.and_then(|details| details.reasoning_tokens),
        cached_input_tokens: None,
    };
    match prompt_details.and_then(|details| details.cached_tokens) {
        Some(cached) => usage.with_cached_input_tokens(cached),
        None => usage,
    }
}

/// Like `usage`, but the total must be the sum of prompt and completion tokens.
pub(crate) fn checked_usage(
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    prompt_details: Option<PromptTokensDetails>,
    completion_details: Option<CompletionTokensDetails>,
) -> Result<Usage, GatewayError> {
    if prompt_tokens.checked_add(completion_tokens) != Some(total_tokens) {
        return Err(upstream_failure());
    }
    Ok(usage(
        prompt_tokens,
        completion_tokens,
        total_tokens,
        prompt_details,
        completion_details,
    ))
}

pub(crate) fn parse_finish_reason(value: &str) -> Result<FinishReason, GatewayError> {
    match value {
        "stop" => Ok(FinishReason::Stop),
        "length" => Ok(FinishReason::Length),
        "tool_calls" => Ok(FinishReason::ToolCalls),
        "content_filter" => Ok(FinishReason::ContentFilter),
        _ => Err(upstream_failure()),
    }
}

/// Checks a `chat.completion` envelope and returns its only choice.
pub(crate) fn only_choice<C>(
    id: &str,
    object: &str,
    model: &str,
    system_fingerprint: Option<&str>,
    choices: Vec<C>,
) -> Result<C, GatewayError> {
    if id.is_empty()
        || object != "chat.completion"
        || model.trim().is_empty()
        || system_fingerprint.is_some_and(str::is_empty)
        || choices.len() != 1
    {
        return Err(upstream_failure());
    }
    choices.into_iter().next().ok_or_else(upstream_failure)
}

/// Non-empty text, then the tool calls given as `(type, call)`; also whether any call
/// was present.
pub(crate) fn assistant_content<I>(
    text: Option<String>,
    calls: I,
) -> Result<(Vec<ChatContent>, bool), GatewayError>
where
    I: ExactSizeIterator<Item = (String, ToolCall)>,
{
    if calls.len() > MAX_TOOL_CALLS {
        return Err(upstream_failure());
    }
    let mut content = Vec::new();
    if let Some(text) = text.filter(|text| !text.is_empty()) {
        content.push(ChatContent::Text { text });
    }
    let mut ids = BTreeSet::new();
    for (kind, call) in calls {
        if kind != "function"
            || !valid_call_id(&call.id)
            || !ids.insert(call.id.clone())
            || !valid_tool_name(&call.name)
        {
            return Err(upstream_failure());
        }
        content.push(ChatContent::ToolCall { call });
    }
    Ok((content, !ids.is_empty()))
}

/// A stop or length stop may carry no visible output, but a stop holding only
/// `unrepresented` output (a refusal, audio) is not an empty answer. `tool_calls` exactly
/// when tool calls are present.
pub(crate) fn check_finish(
    finish_reason: FinishReason,
    content: &[ChatContent],
    has_tool_calls: bool,
    unrepresented: bool,
) -> Result<(), GatewayError> {
    if (content.is_empty()
        && match finish_reason {
            FinishReason::Stop => unrepresented,
            FinishReason::Length => false,
            _ => true,
        })
        || (finish_reason == FinishReason::ToolCalls) != has_tool_calls
    {
        return Err(upstream_failure());
    }
    Ok(())
}
