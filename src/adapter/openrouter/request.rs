use crate::adapter::{
    chat_completions::request::{
        AudioPayload, MessagePayload, MessageRules, SamplingPayload, ToolChoicePayload,
        ToolPayload, encode_messages, encode_tools, optional_tool_choice,
    },
    invalid_request,
};
use serde::Serialize;

use crate::core::{ChatRequest, GatewayError, ResponseFormat, TranscriptionRequest};

#[derive(Serialize)]
pub(super) struct ChatPayload {
    model: String,
    messages: Vec<MessagePayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolPayload>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoicePayload>,
    stream: bool,
    provider: ProviderOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(flatten)]
    sampling: SamplingPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningOptions>,
}

#[derive(Serialize)]
struct ReasoningOptions {
    effort: &'static str,
}

#[derive(Serialize)]
pub(super) struct TranscriptionPayload {
    model: String,
    input_audio: AudioPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    response_format: &'static str,
}

#[derive(Serialize)]
struct ProviderOptions {
    allow_fallbacks: bool,
}

/// OpenRouter serves only the requested provider, never a fallback.
const NO_FALLBACKS: ProviderOptions = ProviderOptions {
    allow_fallbacks: false,
};

/// Extra `embeddings` body members.
#[derive(Serialize)]
pub(super) struct EmbeddingOptions {
    provider: ProviderOptions,
}

pub(super) const EMBEDDING_OPTIONS: EmbeddingOptions = EmbeddingOptions {
    provider: NO_FALLBACKS,
};

/// Assistant reasoning text is always replayed (OpenRouter "Preserving Reasoning":
/// `message.reasoning`); encrypted reasoning is dropped.
pub(super) fn encode(
    chat: &ChatRequest,
    upstream_model: &str,
    stream: bool,
) -> Result<ChatPayload, GatewayError> {
    if upstream_model.trim().is_empty() {
        return Err(invalid_request());
    }
    let messages = encode_messages(
        chat,
        MessageRules {
            replay_reasoning: true,
            reject_empty_text_segments: true,
        },
    )?;
    Ok(ChatPayload {
        model: upstream_model.to_owned(),
        messages,
        tools: encode_tools(&chat.tools),
        tool_choice: optional_tool_choice(chat),
        stream,
        provider: NO_FALLBACKS,
        response_format: chat
            .options
            .response_format
            .clone()
            .filter(|format| !matches!(format, ResponseFormat::Text)),
        sampling: SamplingPayload::new(&chat.options.sampling),
        max_tokens: chat.options.max_output_tokens,
        reasoning: chat
            .options
            .reasoning_effort
            .map(|effort| ReasoningOptions {
                effort: effort.as_str(),
            }),
    })
}

/// Maps an accepted upload media type to an OpenRouter STT audio format.
pub(super) fn transcription_format(media_type: &str) -> Option<&'static str> {
    match media_type {
        "audio/wav" | "audio/x-wav" => Some("wav"),
        "audio/mpeg" => Some("mp3"),
        "audio/flac" => Some("flac"),
        "audio/mp4" => Some("m4a"),
        "audio/ogg" => Some("ogg"),
        "audio/webm" => Some("webm"),
        _ => None,
    }
}

pub(super) fn encode_transcription(
    transcription: &TranscriptionRequest,
    upstream_model: &str,
) -> Result<TranscriptionPayload, GatewayError> {
    if upstream_model.trim().is_empty() || transcription.file.bytes().is_empty() {
        return Err(invalid_request());
    }
    let format =
        transcription_format(transcription.file.media_type()).ok_or_else(invalid_request)?;
    Ok(TranscriptionPayload {
        model: upstream_model.to_owned(),
        input_audio: AudioPayload::new(transcription.file.bytes(), format),
        language: transcription.language.clone(),
        response_format: "json",
    })
}
