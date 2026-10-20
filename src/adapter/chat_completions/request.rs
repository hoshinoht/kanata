use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::{Number, Value};

use crate::{
    adapter::{invalid_request, unsupported_operation},
    core::{
        ChatContent, ChatMessage, ChatRequest, ChatRole, FunctionTool, GatewayError,
        InputAudioFormat, SamplingOptions, ToolCall, ToolChoice,
    },
};

#[derive(Serialize)]
pub(crate) struct StreamOptions {
    include_usage: bool,
}

impl StreamOptions {
    /// Usage on the final chunk, requested for every stream.
    pub(crate) fn for_stream(stream: bool) -> Option<Self> {
        stream.then_some(Self {
            include_usage: true,
        })
    }
}

/// Sampling controls; payloads flatten it, so its field order is the wire order.
#[derive(Default, Serialize)]
pub(crate) struct SamplingPayload {
    // Number keeps an integer temperature byte-identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f64>,
}

impl SamplingPayload {
    pub(crate) fn new(sampling: &SamplingOptions) -> Self {
        Self {
            temperature: sampling
                .temperature
                .and_then(|value| Number::from_f64(value.get())),
            top_p: sampling.top_p.map(|value| value.get()),
            seed: sampling.seed,
            stop: sampling.stop.clone(),
            presence_penalty: sampling.presence_penalty.map(|value| value.get()),
            frequency_penalty: sampling.frequency_penalty.map(|value| value.get()),
        }
    }

    pub(crate) fn temperature(temperature: Number) -> Self {
        Self {
            temperature: Some(temperature),
            ..Self::default()
        }
    }
}

#[derive(Serialize)]
pub(crate) struct MessagePayload {
    role: &'static str,
    // Absent only on an assistant turn that carries just tool calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallPayload>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    // Replayed assistant reasoning text.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<String>,
}

impl MessagePayload {
    pub(crate) fn user_parts(parts: Vec<ContentPart>) -> Self {
        Self::content("user", MessageContent::Parts(parts))
    }

    fn content(role: &'static str, content: MessageContent) -> Self {
        Self {
            role,
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
            reasoning: None,
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ContentPart {
    Text { text: String },
    InputAudio { input_audio: AudioPayload },
    ImageUrl { image_url: ImageUrl },
}

impl ContentPart {
    pub(crate) fn audio(bytes: &[u8], format: InputAudioFormat) -> Self {
        Self::InputAudio {
            input_audio: AudioPayload::new(bytes, audio_format(format)),
        }
    }

    pub(crate) fn image(url: &str) -> Self {
        Self::ImageUrl {
            image_url: ImageUrl {
                url: url.to_owned(),
            },
        }
    }
}

#[derive(Serialize)]
pub(crate) struct ImageUrl {
    url: String,
}

#[derive(Serialize)]
pub(crate) struct AudioPayload {
    data: String,
    format: &'static str,
}

impl AudioPayload {
    pub(crate) fn new(bytes: &[u8], format: &'static str) -> Self {
        Self {
            data: STANDARD.encode(bytes),
            format,
        }
    }
}

fn audio_format(format: InputAudioFormat) -> &'static str {
    match format {
        InputAudioFormat::Wav => "wav",
        InputAudioFormat::Mp3 => "mp3",
    }
}

#[derive(Serialize)]
pub(crate) struct ToolPayload {
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionPayload,
}

#[derive(Serialize)]
struct FunctionPayload {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: Value,
}

#[derive(Serialize)]
pub(crate) struct ToolCallPayload {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: CallFunctionPayload,
}

impl ToolCallPayload {
    pub(crate) fn new(call: &ToolCall) -> Self {
        Self {
            id: call.id.clone(),
            kind: "function",
            function: CallFunctionPayload {
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            },
        }
    }
}

#[derive(Serialize)]
struct CallFunctionPayload {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum ToolChoicePayload {
    Simple(&'static str),
    Named {
        #[serde(rename = "type")]
        kind: &'static str,
        function: NamedFunctionPayload,
    },
}

#[derive(Serialize)]
pub(crate) struct NamedFunctionPayload {
    name: String,
}

/// `None` when the request declares no tools.
pub(crate) fn encode_tools(tools: &[FunctionTool]) -> Option<Vec<ToolPayload>> {
    (!tools.is_empty()).then(|| {
        tools
            .iter()
            .map(|tool| ToolPayload {
                kind: "function",
                function: FunctionPayload {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: tool.parameters.clone(),
                },
            })
            .collect()
    })
}

pub(crate) fn encode_tool_choice(choice: &ToolChoice) -> ToolChoicePayload {
    match choice {
        ToolChoice::None => ToolChoicePayload::Simple("none"),
        ToolChoice::Auto => ToolChoicePayload::Simple("auto"),
        ToolChoice::Required => ToolChoicePayload::Simple("required"),
        ToolChoice::Function { name } => ToolChoicePayload::Named {
            kind: "function",
            function: NamedFunctionPayload { name: name.clone() },
        },
    }
}

/// Omitted for the default so tool-free payloads stay unchanged.
pub(crate) fn optional_tool_choice(chat: &ChatRequest) -> Option<ToolChoicePayload> {
    (!chat.tools.is_empty() || !matches!(chat.tool_choice, ToolChoice::Auto))
        .then(|| encode_tool_choice(&chat.tool_choice))
}

#[derive(Clone, Copy)]
pub(crate) struct MessageRules {
    /// Forward assistant reasoning text; otherwise it is dropped.
    pub(crate) replay_reasoning: bool,
    /// Reject each empty text segment of a text-only system, developer or user turn, not
    /// only an empty turn.
    pub(crate) reject_empty_text_segments: bool,
}

pub(crate) fn encode_messages(
    chat: &ChatRequest,
    rules: MessageRules,
) -> Result<Vec<MessagePayload>, GatewayError> {
    chat.messages_without_reasoning()
        .map(|message| encode_message(message, rules))
        .collect()
}

fn encode_message(
    message: &ChatMessage,
    rules: MessageRules,
) -> Result<MessagePayload, GatewayError> {
    let role = match message.role {
        ChatRole::System => "system",
        ChatRole::Developer => "developer",
        ChatRole::User => "user",
        ChatRole::Assistant => return encode_assistant(message, rules.replay_reasoning),
        ChatRole::Tool => return encode_tool_result(message),
    };

    if message.content.iter().any(|content| {
        matches!(
            content,
            ChatContent::InputAudio { .. } | ChatContent::InputImage { .. }
        )
    }) {
        if message.role != ChatRole::User {
            return Err(unsupported_operation());
        }
        let parts = message
            .content
            .iter()
            .map(|content| match content {
                ChatContent::Text { text } if !text.is_empty() => {
                    Ok(ContentPart::Text { text: text.clone() })
                }
                ChatContent::Text { .. } => Err(invalid_request()),
                ChatContent::InputAudio { audio } => {
                    Ok(ContentPart::audio(audio.bytes(), audio.format()))
                }
                ChatContent::InputImage { image } => Ok(ContentPart::image(image.data_url())),
                // Media messages are user turns, which never carry reasoning.
                ChatContent::ToolCall { .. }
                | ChatContent::ToolResult { .. }
                | ChatContent::Reasoning { .. }
                | ChatContent::ReasoningText { .. } => Err(unsupported_operation()),
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(MessagePayload::content(role, MessageContent::Parts(parts)));
    }

    let mut text = String::new();
    for content in &message.content {
        let ChatContent::Text { text: segment } = content else {
            return Err(unsupported_operation());
        };
        if rules.reject_empty_text_segments && segment.is_empty() {
            return Err(invalid_request());
        }
        text.push_str(segment);
    }
    if text.is_empty() {
        return Err(invalid_request());
    }
    Ok(MessagePayload::content(role, MessageContent::Text(text)))
}

/// Assistant history: text (empty segments skipped), any tool calls, and reasoning text
/// when `replay_reasoning` is set.
fn encode_assistant(
    message: &ChatMessage,
    replay_reasoning: bool,
) -> Result<MessagePayload, GatewayError> {
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut reasoning = String::new();
    for content in &message.content {
        match content {
            ChatContent::Text { text: segment } => text.push_str(segment),
            ChatContent::ToolCall { call } => calls.push(ToolCallPayload::new(call)),
            ChatContent::InputAudio { .. }
            | ChatContent::InputImage { .. }
            | ChatContent::ToolResult { .. } => {
                return Err(unsupported_operation());
            }
            ChatContent::ReasoningText { text: segment } if replay_reasoning => {
                reasoning.push_str(segment);
            }
            // Encrypted reasoning never replays here; reasoning text only where accepted.
            ChatContent::Reasoning { .. } | ChatContent::ReasoningText { .. } => {}
        }
    }
    if text.is_empty() && calls.is_empty() {
        return Err(invalid_request());
    }
    Ok(MessagePayload {
        role: "assistant",
        content: (!text.is_empty()).then_some(MessageContent::Text(text)),
        tool_calls: (!calls.is_empty()).then_some(calls),
        tool_call_id: None,
        reasoning: (!reasoning.is_empty()).then_some(reasoning),
    })
}

fn encode_tool_result(message: &ChatMessage) -> Result<MessagePayload, GatewayError> {
    let [ChatContent::ToolResult { call_id, content }] = message.content.as_slice() else {
        return Err(invalid_request());
    };
    Ok(MessagePayload {
        role: "tool",
        content: Some(MessageContent::Text(content.clone())),
        tool_calls: None,
        tool_call_id: Some(call_id.clone()),
        reasoning: None,
    })
}
