use std::collections::BTreeSet;

use serde::{Deserialize, de::IgnoredAny};

use crate::core::{
    ChatContent, ChatMessage, ChatResponse, ChatRole, ErrorKind, FinishReason, GatewayError,
    ModelAlias, ToolCall, TranscriptionResponse, Usage,
};

pub(super) const MAX_TOOL_CALLS: usize = 64;
const MAX_TOOL_CALL_ID_BYTES: usize = 128;
const MAX_TOOL_NAME_BYTES: usize = 64;

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
    #[serde(default, rename = "provider")]
    _provider: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChoicePayload {
    index: u64,
    message: MessagePayload,
    finish_reason: String,
    #[serde(default, rename = "native_finish_reason")]
    _native_finish_reason: Option<String>,
    #[serde(default, rename = "logprobs")]
    _logprobs: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessagePayload {
    role: String,
    content: Option<String>,
    // A refusal without content still fails as an empty reply.
    #[serde(default, rename = "refusal")]
    _refusal: Option<String>,
    // Reasoning is accepted and not forwarded.
    #[serde(default, rename = "reasoning")]
    _reasoning: Option<String>,
    #[serde(default, rename = "reasoning_details")]
    _reasoning_details: Option<serde_json::Value>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallPayload>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallPayload {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "index")]
    _index: Option<u64>,
    function: FunctionCallPayload,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FunctionCallPayload {
    name: String,
    arguments: String,
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
    let mut content = Vec::new();
    if let Some(text) = choice.message.content.filter(|text| !text.is_empty()) {
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
    // A length stop may carry no visible output.
    if (content.is_empty() && finish_reason != FinishReason::Length)
        || (finish_reason == FinishReason::ToolCalls) != !ids.is_empty()
    {
        return Err(upstream_failure());
    }
    let usage = payload.usage.map(normalize_usage).transpose()?;

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TranscriptionPayload {
    text: String,
    #[serde(default, rename = "usage")]
    _usage: Option<IgnoredAny>,
}

pub(super) fn decode_transcription(bytes: &[u8]) -> Result<TranscriptionResponse, GatewayError> {
    let payload: TranscriptionPayload =
        serde_json::from_slice(bytes).map_err(|_| upstream_failure())?;
    let text = payload.text.trim();
    if text.is_empty() {
        return Err(upstream_failure());
    }
    Ok(TranscriptionResponse {
        text: text.to_owned(),
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

pub(super) fn parse_finish_reason(value: &str) -> Result<FinishReason, GatewayError> {
    match value {
        "stop" => Ok(FinishReason::Stop),
        "length" => Ok(FinishReason::Length),
        "tool_calls" => Ok(FinishReason::ToolCalls),
        "content_filter" => Ok(FinishReason::ContentFilter),
        _ => Err(upstream_failure()),
    }
}

pub(super) fn valid_call_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOOL_CALL_ID_BYTES
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

pub(super) fn valid_tool_name(value: &str) -> bool {
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
