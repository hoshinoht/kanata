use serde::{Deserialize, de::IgnoredAny};

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
    // Recent vLLM metadata; accepted and not forwarded.
    #[serde(default, rename = "service_tier")]
    _service_tier: Option<IgnoredAny>,
    #[serde(default, rename = "prompt_logprobs")]
    _prompt_logprobs: Option<IgnoredAny>,
    #[serde(default, rename = "prompt_token_ids")]
    _prompt_token_ids: Option<IgnoredAny>,
    #[serde(default, rename = "prompt_text")]
    _prompt_text: Option<IgnoredAny>,
    #[serde(default, rename = "kv_transfer_params")]
    _kv_transfer_params: Option<IgnoredAny>,
    #[serde(default, rename = "ec_transfer_params")]
    _ec_transfer_params: Option<IgnoredAny>,
    #[serde(default, rename = "metrics")]
    _metrics: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChoicePayload {
    index: u64,
    message: MessagePayload,
    finish_reason: String,
    #[serde(default, rename = "logprobs")]
    _logprobs: Option<IgnoredAny>,
    #[serde(default, rename = "stop_reason")]
    _stop_reason: Option<IgnoredAny>,
    #[serde(default, rename = "token_ids")]
    _token_ids: Option<IgnoredAny>,
    #[serde(default, rename = "routed_experts")]
    _routed_experts: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessagePayload {
    role: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default, rename = "reasoning")]
    _reasoning: Option<String>,
    #[serde(default, rename = "reasoning_content")]
    _reasoning_content: Option<String>,
    // A refusal or audio without text content still fails as an empty reply.
    #[serde(default, rename = "refusal")]
    _refusal: Option<IgnoredAny>,
    #[serde(default, rename = "annotations")]
    _annotations: Option<IgnoredAny>,
    #[serde(default, rename = "audio")]
    _audio: Option<IgnoredAny>,
    #[serde(default, rename = "function_call")]
    _function_call: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsagePayload {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    #[serde(default, rename = "prompt_tokens_details")]
    _prompt_tokens_details: Option<IgnoredAny>,
    #[serde(default, rename = "completion_tokens_details")]
    _completion_tokens_details: Option<IgnoredAny>,
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
    // Recent vLLM metadata; accepted and not forwarded.
    #[serde(default, rename = "service_tier")]
    _service_tier: Option<IgnoredAny>,
    #[serde(default, rename = "prompt_logprobs")]
    _prompt_logprobs: Option<IgnoredAny>,
    #[serde(default, rename = "prompt_token_ids")]
    _prompt_token_ids: Option<IgnoredAny>,
    #[serde(default, rename = "prompt_text")]
    _prompt_text: Option<IgnoredAny>,
    #[serde(default, rename = "kv_transfer_params")]
    _kv_transfer_params: Option<IgnoredAny>,
    #[serde(default, rename = "ec_transfer_params")]
    _ec_transfer_params: Option<IgnoredAny>,
    #[serde(default, rename = "metrics")]
    _metrics: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TranscriptionChoicePayload {
    index: u64,
    message: TranscriptionMessagePayload,
    finish_reason: String,
    #[serde(default, rename = "logprobs")]
    _logprobs: Option<IgnoredAny>,
    #[serde(default, rename = "stop_reason")]
    _stop_reason: Option<IgnoredAny>,
    #[serde(default, rename = "token_ids")]
    _token_ids: Option<IgnoredAny>,
    #[serde(default, rename = "routed_experts")]
    _routed_experts: Option<IgnoredAny>,
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
    // A refusal or audio without text content still fails as an empty reply.
    #[serde(default, rename = "refusal")]
    _refusal: Option<IgnoredAny>,
    #[serde(default, rename = "annotations")]
    _annotations: Option<IgnoredAny>,
    #[serde(default, rename = "audio")]
    _audio: Option<IgnoredAny>,
    #[serde(default, rename = "function_call")]
    _function_call: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeTranscriptionPayload {
    text: String,
    #[serde(default, rename = "usage")]
    _usage: Option<IgnoredAny>,
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
    let content = match choice.message.content.filter(|text| !text.is_empty()) {
        Some(text) => vec![ChatContent::Text { text }],
        // A length stop may carry no visible output.
        None if finish_reason == FinishReason::Length => Vec::new(),
        None => return Err(upstream_failure()),
    };

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

#[cfg(test)]
mod tests {
    use crate::core::{ChatContent, ModelAlias};

    const CHAT: &str = include_str!("../../../tests/fixtures/vllm/chat-vllm-0.30-response.json");
    const NATIVE: &str =
        include_str!("../../../tests/fixtures/vllm/native-asr-vllm-0.30-response.json");

    #[test]
    fn vllm_0_30_metadata_fields_are_accepted() {
        let chat = super::decode(CHAT.as_bytes(), ModelAlias("omni".into())).expect("chat");
        assert_eq!(
            chat.message.content,
            vec![ChatContent::Text {
                text: "fixture audio transcript".into()
            }]
        );
        let bridged = super::decode_transcription(CHAT.as_bytes()).expect("bridge");
        assert_eq!(bridged.text, "fixture audio transcript");
        let native = super::decode_native_transcription(NATIVE.as_bytes()).expect("native");
        assert_eq!(native.text, "fixture native transcript");
    }
}
