use serde::{Deserialize, de::IgnoredAny};

use crate::core::{
    ChatContent, ChatMessage, ChatResponse, ChatRole, ErrorKind, FinishReason, GatewayError,
    ModelAlias, Usage,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionPayload {
    id: String,
    object: String,
    #[serde(rename = "created")]
    _created: u64,
    model: String,
    choices: Vec<ChoicePayload>,
    #[serde(default)]
    usage: Option<UsagePayload>,
    #[serde(default)]
    system_fingerprint: Option<String>,
    #[serde(default, rename = "openrouter_metadata")]
    _openrouter_metadata: Option<IgnoredAny>,
    #[serde(default, rename = "service_tier")]
    _service_tier: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChoicePayload {
    index: u64,
    message: MessagePayload,
    finish_reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessagePayload {
    role: String,
    content: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UsagePayload {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    #[serde(default, rename = "cost")]
    _cost: Option<IgnoredAny>,
    #[serde(default, rename = "cost_details")]
    _cost_details: Option<IgnoredAny>,
    #[serde(default, rename = "prompt_tokens_details")]
    _prompt_tokens_details: Option<IgnoredAny>,
    #[serde(default, rename = "completion_tokens_details")]
    _completion_tokens_details: Option<IgnoredAny>,
    #[serde(default, rename = "is_byok")]
    _is_byok: Option<IgnoredAny>,
    #[serde(default, rename = "server_tool_use_details")]
    _server_tool_use_details: Option<IgnoredAny>,
    #[serde(default, rename = "server_tool_use")]
    _server_tool_use: Option<IgnoredAny>,
}

pub(super) fn decode(bytes: &[u8], public_model: ModelAlias) -> Result<ChatResponse, GatewayError> {
    let payload: CompletionPayload =
        serde_json::from_slice(bytes).map_err(|_| upstream_failure())?;
    if payload.id.is_empty()
        || payload.object != "chat.completion"
        || payload.model.trim().is_empty()
        || payload
            .system_fingerprint
            .as_ref()
            .is_some_and(String::is_empty)
        || payload.choices.len() != 1
    {
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
    if finish_reason == FinishReason::ToolCalls {
        return Err(upstream_failure());
    }
    let Some(text) = choice.message.content.filter(|text| !text.is_empty()) else {
        return Err(upstream_failure());
    };
    let usage = payload.usage.map(normalize_usage).transpose()?;

    Ok(ChatResponse {
        model: public_model,
        message: ChatMessage {
            role: ChatRole::Assistant,
            content: vec![ChatContent::Text { text }],
        },
        finish_reason,
        usage,
    })
}

pub(super) fn normalize_usage(usage: UsagePayload) -> Result<Usage, GatewayError> {
    if usage.prompt_tokens.checked_add(usage.completion_tokens) != Some(usage.total_tokens) {
        return Err(upstream_failure());
    }
    Ok(Usage {
        input_tokens: usage.prompt_tokens,
        output_tokens: usage.completion_tokens,
        total_tokens: usage.total_tokens,
    })
}

fn parse_finish_reason(value: &str) -> Result<FinishReason, GatewayError> {
    match value {
        "stop" => Ok(FinishReason::Stop),
        "length" => Ok(FinishReason::Length),
        "content_filter" => Ok(FinishReason::ContentFilter),
        _ => Err(upstream_failure()),
    }
}

fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}
