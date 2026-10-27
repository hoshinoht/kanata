use crate::{
    chat_completions::response::{
        self as wire, CompletionTokensDetails, FunctionCallPayload, PromptTokensDetails,
    },
    upstream_failure,
};

use serde::{
    Deserialize,
    de::{DeserializeOwned, IgnoredAny},
};

use crate::core::{
    ChatResponse, FinishReason, GatewayError, ModelAlias, TranscriptionResponse, Usage,
};

/// A completion whose message is `M`: the chat reply or the audio-chat transcript.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionPayload<M> {
    id: String,
    object: String,
    #[serde(rename = "created")]
    _created: u64,
    model: String,
    choices: Vec<ChoicePayload<M>>,
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
struct ChoicePayload<M> {
    index: u64,
    message: M,
    finish_reason: String,
    #[serde(default, rename = "logprobs")]
    _logprobs: Option<IgnoredAny>,
    #[serde(default, rename = "stop_reason")]
    _stop_reason: Option<IgnoredAny>,
    #[serde(default, rename = "token_ids")]
    _token_ids: Option<IgnoredAny>,
    #[serde(default, rename = "routed_experts")]
    _routed_experts: Option<IgnoredAny>,
    // LiteLLM proxy metadata.
    #[serde(default, rename = "provider_specific_fields")]
    _provider_specific_fields: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessagePayload {
    role: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallPayload>>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    // A refusal or audio without text content is not an empty answer; it still fails.
    #[serde(default)]
    refusal: Option<IgnoredAny>,
    #[serde(default, rename = "annotations")]
    _annotations: Option<IgnoredAny>,
    #[serde(default)]
    audio: Option<IgnoredAny>,
    #[serde(default, rename = "function_call")]
    _function_call: Option<IgnoredAny>,
    #[serde(default, rename = "provider_specific_fields")]
    _provider_specific_fields: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsagePayload {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

impl UsagePayload {
    fn normalize(self) -> Result<Usage, GatewayError> {
        wire::checked_usage(
            self.prompt_tokens,
            self.completion_tokens,
            self.total_tokens,
            self.prompt_tokens_details,
            self.completion_tokens_details,
        )
    }
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
    #[serde(default, rename = "provider_specific_fields")]
    _provider_specific_fields: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallPayload {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: FunctionCallPayload,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeTranscriptionPayload {
    text: String,
    #[serde(default, rename = "usage")]
    _usage: Option<IgnoredAny>,
}

/// The completion's only choice, at index 0, and its usage.
fn decode_choice<M: DeserializeOwned>(
    bytes: &[u8],
) -> Result<(ChoicePayload<M>, Option<UsagePayload>), GatewayError> {
    let payload: CompletionPayload<M> =
        serde_json::from_slice(bytes).map_err(|_| upstream_failure())?;
    let choice = wire::only_choice(
        &payload.id,
        &payload.object,
        &payload.model,
        payload.system_fingerprint.as_deref(),
        payload.choices,
    )?;
    if choice.index != 0 {
        return Err(upstream_failure());
    }
    Ok((choice, payload.usage))
}

pub(super) fn decode(bytes: &[u8], public_model: ModelAlias) -> Result<ChatResponse, GatewayError> {
    let (choice, usage) = decode_choice::<MessagePayload>(bytes)?;
    let message = choice.message;
    if message.role != "assistant" {
        return Err(upstream_failure());
    }
    let finish_reason = wire::parse_finish_reason(&choice.finish_reason)?;
    let reasoning = message
        .reasoning
        .or(message.reasoning_content)
        .filter(|text| !text.is_empty());
    let calls = message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|call| call.function.into_call(call.kind, call.id));
    let (content, has_tool_calls) = wire::assistant_content(message.content, calls)?;
    let unrepresented = message.refusal.is_some() || message.audio.is_some();
    wire::check_finish(finish_reason, &content, has_tool_calls, unrepresented)?;

    let usage = usage.map(UsagePayload::normalize).transpose()?;
    Ok(wire::chat_response(
        public_model,
        content,
        finish_reason,
        usage,
        reasoning,
    ))
}

pub(super) fn decode_transcription(bytes: &[u8]) -> Result<TranscriptionResponse, GatewayError> {
    let (choice, usage) = decode_choice::<TranscriptionMessagePayload>(bytes)?;
    if choice.message.role != "assistant"
        || wire::parse_finish_reason(&choice.finish_reason)? != FinishReason::Stop
    {
        return Err(upstream_failure());
    }
    let Some(text) = choice
        .message
        .content
        .filter(|text| !text.trim().is_empty())
    else {
        return Err(upstream_failure());
    };

    usage.map(UsagePayload::normalize).transpose()?;
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

#[cfg(test)]
mod tests {
    use crate::core::{ChatContent, ModelAlias};

    const CHAT: &str = include_str!("../../../../tests/fixtures/vllm/chat-vllm-0.30-response.json");
    const NATIVE: &str =
        include_str!("../../../../tests/fixtures/vllm/native-asr-vllm-0.30-response.json");

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

    #[test]
    fn litellm_tool_call_response_decodes() {
        const TOOL: &str =
            include_str!("../../../../tests/fixtures/vllm/chat-litellm-tool-call-response.json");
        let chat = super::decode(TOOL.as_bytes(), ModelAlias("omni".into())).expect("chat");
        assert_eq!(chat.finish_reason, crate::core::FinishReason::ToolCalls);
        assert!(matches!(
            chat.message.content.as_slice(),
            [ChatContent::ToolCall { call }]
                if call.name == "get_weather" && call.arguments == r#"{"city": "Singapore"}"#
        ));
    }

    #[test]
    fn litellm_proxy_fields_are_accepted() {
        const LITELLM: &str =
            include_str!("../../../../tests/fixtures/vllm/chat-litellm-proxy-response.json");
        let chat = super::decode(LITELLM.as_bytes(), ModelAlias("omni".into())).expect("chat");
        assert_eq!(
            chat.message.content,
            vec![ChatContent::Text {
                text: "fixture litellm reply".into()
            }]
        );
        let bridged = super::decode_transcription(LITELLM.as_bytes()).expect("bridge");
        assert_eq!(bridged.text, "fixture litellm reply");
    }
}
