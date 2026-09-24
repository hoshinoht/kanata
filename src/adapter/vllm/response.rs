use serde::Deserialize;

use crate::core::{
    ChatContent, ChatMessage, ChatResponse, ChatRole, ErrorKind, FinishReason, GatewayError,
    ModelAlias, TranscriptionResponse, Usage,
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
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsagePayload {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TranscriptionCompletionPayload {
    id: String,
    object: String,
    #[serde(rename = "created")]
    _created: u64,
    model: String,
    choices: Vec<TranscriptionChoicePayload>,
    #[serde(default)]
    usage: Option<UsagePayload>,
    #[serde(default)]
    system_fingerprint: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TranscriptionChoicePayload {
    index: u64,
    message: TranscriptionMessagePayload,
    finish_reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TranscriptionMessagePayload {
    role: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default, rename = "reasoning")]
    _reasoning: Option<String>,
    #[serde(default, rename = "reasoning_content")]
    _reasoning_content: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeTranscriptionPayload {
    text: String,
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

pub(super) fn decode_transcription(bytes: &[u8]) -> Result<TranscriptionResponse, GatewayError> {
    let payload: TranscriptionCompletionPayload =
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
    if parse_finish_reason(&choice.finish_reason)? != FinishReason::Stop {
        return Err(upstream_failure());
    }
    let Some(text) = choice
        .message
        .content
        .filter(|text| !text.trim().is_empty())
    else {
        return Err(upstream_failure());
    };

    payload.usage.map(normalize_usage).transpose()?;
    Ok(TranscriptionResponse {
        text: text.trim().to_owned(),
    })
}

pub(super) fn decode_native_transcription(
    bytes: &[u8],
) -> Result<TranscriptionResponse, GatewayError> {
    let payload: NativeTranscriptionPayload =
        serde_json::from_slice(bytes).map_err(|_| upstream_failure())?;
    let text = payload.text.trim();
    if text.is_empty() {
        return Err(upstream_failure());
    }
    Ok(TranscriptionResponse {
        text: text.to_owned(),
    })
}

fn normalize_usage(usage: UsagePayload) -> Result<Usage, GatewayError> {
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
