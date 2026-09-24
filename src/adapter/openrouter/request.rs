use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Serialize;

use crate::core::{
    ChatContent, ChatMessage, ChatRequest, ChatRole, ErrorKind, GatewayError, InputAudioFormat,
    ResponseFormat, TranscriptionRequest,
};

#[derive(Serialize)]
pub(super) struct ChatPayload {
    model: String,
    messages: Vec<MessagePayload>,
    stream: bool,
    provider: ProviderOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<i64>,
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
struct MessagePayload {
    role: &'static str,
    content: MessageContent,
}

#[derive(Serialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart {
    Text { text: String },
    InputAudio { input_audio: AudioPayload },
}

#[derive(Serialize)]
pub(super) struct AudioPayload {
    data: String,
    format: &'static str,
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

pub(super) fn encode(
    chat: &ChatRequest,
    upstream_model: &str,
    stream: bool,
) -> Result<ChatPayload, GatewayError> {
    if upstream_model.trim().is_empty() {
        return Err(invalid_request());
    }
    let messages = chat
        .messages
        .iter()
        .map(encode_message)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ChatPayload {
        model: upstream_model.to_owned(),
        messages,
        stream,
        provider: ProviderOptions {
            allow_fallbacks: false,
        },
        response_format: chat
            .options
            .response_format
            .clone()
            .filter(|format| !matches!(format, ResponseFormat::Text)),
        temperature: chat.options.sampling.temperature.map(|value| value.get()),
        top_p: chat.options.sampling.top_p.map(|value| value.get()),
        seed: chat.options.sampling.seed,
        max_tokens: chat.options.max_output_tokens,
        reasoning: chat
            .options
            .reasoning_effort
            .map(|effort| ReasoningOptions {
                effort: effort.as_str(),
            }),
    })
}

fn encode_message(message: &ChatMessage) -> Result<MessagePayload, GatewayError> {
    let role = match message.role {
        ChatRole::System => "system",
        ChatRole::Developer => "developer",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => return Err(unsupported_operation()),
    };

    let has_audio = message
        .content
        .iter()
        .any(|segment| matches!(segment, ChatContent::InputAudio { .. }));
    if has_audio {
        if message.role != ChatRole::User {
            return Err(unsupported_operation());
        }
        let parts = message
            .content
            .iter()
            .map(|segment| match segment {
                ChatContent::Text { text } if !text.is_empty() => {
                    Ok(ContentPart::Text { text: text.clone() })
                }
                ChatContent::Text { .. } => Err(invalid_request()),
                ChatContent::InputAudio { audio } => Ok(ContentPart::InputAudio {
                    input_audio: AudioPayload {
                        data: STANDARD.encode(audio.bytes()),
                        format: chat_audio_format(audio.format()),
                    },
                }),
                ChatContent::ToolCall { .. } | ChatContent::ToolResult { .. } => {
                    Err(unsupported_operation())
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(MessagePayload {
            role,
            content: MessageContent::Parts(parts),
        });
    }

    let mut content = String::new();
    for segment in &message.content {
        let ChatContent::Text { text } = segment else {
            return Err(unsupported_operation());
        };
        if text.is_empty() {
            return Err(invalid_request());
        }
        content.push_str(text);
    }
    if content.is_empty() {
        return Err(invalid_request());
    }

    Ok(MessagePayload {
        role,
        content: MessageContent::Text(content),
    })
}

fn chat_audio_format(format: InputAudioFormat) -> &'static str {
    match format {
        InputAudioFormat::Wav => "wav",
        InputAudioFormat::Mp3 => "mp3",
    }
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
        input_audio: AudioPayload {
            data: STANDARD.encode(transcription.file.bytes()),
            format,
        },
        language: transcription.language.clone(),
        response_format: "json",
    })
}

fn invalid_request() -> GatewayError {
    GatewayError {
        kind: ErrorKind::InvalidRequest,
    }
}

fn unsupported_operation() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UnsupportedOperation,
    }
}
