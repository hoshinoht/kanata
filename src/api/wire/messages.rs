use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;

use crate::core::{
    AudioValidationError, ChatContent, ChatMessage, ChatRole, ImageBudget, ImageValidationError,
    InputAudioFormat, MAX_INPUT_AUDIO_BYTES, MAX_REASONING_BYTES, ToolCall, ValidatedAudio,
    ValidatedImage,
};

use super::{ChatWireError, valid_name};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MessageWire {
    role: String,
    #[serde(default)]
    content: OptionalField<ContentWire>,
    #[serde(default)]
    tool_calls: OptionalField<Vec<ToolCallWire>>,
    #[serde(default)]
    tool_call_id: OptionalField<String>,
    /// Plaintext reasoning replayed on an assistant turn (OMP replaying gpt-oss turns).
    #[serde(default)]
    reasoning_content: OptionalField<String>,
}

impl MessageWire {
    /// `index` is the message's position in `messages`, used to name failures. An assistant
    /// turn with empty content (`""`, `null`, `[]` or only empty text) and no tool calls is
    /// a replayed empty answer and maps to no message.
    pub(super) fn into_core(
        self,
        index: usize,
        max_audio_bytes: usize,
        total_audio_bytes: &mut usize,
        image_budget: &mut ImageBudget,
    ) -> Result<Option<ChatMessage>, ChatWireError> {
        let invalid = |member: &str| ChatWireError::param(format!("messages[{index}].{member}"));
        let role = match self.role.as_str() {
            "system" => ChatRole::System,
            "developer" => ChatRole::Developer,
            "user" => ChatRole::User,
            "assistant" => ChatRole::Assistant,
            "tool" => ChatRole::Tool,
            _ => return Err(invalid("role")),
        };
        let content_present = matches!(&self.content, OptionalField::Value(_));
        let content_given = !matches!(&self.content, OptionalField::Missing);
        let mut content = match self.content {
            OptionalField::Missing | OptionalField::Null => Vec::new(),
            OptionalField::Value(ContentWire::Text(text)) => {
                vec![ChatContent::Text { text }]
            }
            OptionalField::Value(ContentWire::Parts(parts)) => parts
                .into_iter()
                .enumerate()
                .map(|(part_index, part)| {
                    part.into_core(role, max_audio_bytes, total_audio_bytes, image_budget)
                        .map_err(|error| match error {
                            PartError::TooLarge => ChatWireError::TooLarge,
                            PartError::Invalid => invalid(&format!("content[{part_index}]")),
                        })
                })
                .collect::<Result<Vec<_>, _>>()?,
        };

        let tool_calls_field = self.tool_calls;
        let tool_calls_present = !matches!(&tool_calls_field, OptionalField::Missing);
        let tool_calls = match tool_calls_field {
            OptionalField::Missing => Vec::new(),
            OptionalField::Null => {
                if role != ChatRole::Assistant {
                    return Err(invalid("tool_calls"));
                }
                Vec::new()
            }
            OptionalField::Value(tool_calls) => tool_calls,
        };
        let tool_call_id = self.tool_call_id;
        let tool_call_id_present = !matches!(&tool_call_id, OptionalField::Missing);
        let reasoning_content = self.reasoning_content;
        if role != ChatRole::Assistant && !matches!(&reasoning_content, OptionalField::Missing) {
            return Err(invalid("reasoning_content"));
        }

        match role {
            ChatRole::Assistant => {
                if tool_call_id_present {
                    return Err(invalid("tool_call_id"));
                }
                // Same per-message cap as reasoning Kanata emits.
                if matches!(&reasoning_content, OptionalField::Value(text) if text.len() > MAX_REASONING_BYTES)
                {
                    return Err(invalid("reasoning_content"));
                }
                for (call_index, call) in tool_calls.into_iter().enumerate() {
                    if call.kind != "function"
                        || !valid_name(&call.function.name)
                        || call.id.is_empty()
                    {
                        return Err(invalid(&format!("tool_calls[{call_index}]")));
                    }
                    content.push(ChatContent::ToolCall {
                        call: ToolCall {
                            id: call.id,
                            name: call.function.name,
                            arguments: call.function.arguments,
                        },
                    });
                }
                if !has_nonempty_text(&content)
                    && !content
                        .iter()
                        .any(|item| matches!(item, ChatContent::ToolCall { .. }))
                {
                    if content_given {
                        return Ok(None);
                    }
                    return Err(invalid("content"));
                }
                // An empty replay carries nothing; reasoning leads the turn's other parts.
                if let OptionalField::Value(text) = reasoning_content
                    && !text.is_empty()
                {
                    content.insert(0, ChatContent::ReasoningText { text });
                }
            }
            ChatRole::Tool => {
                if tool_calls_present {
                    return Err(invalid("tool_calls"));
                }
                let OptionalField::Value(call_id) = tool_call_id else {
                    return Err(invalid("tool_call_id"));
                };
                if call_id.is_empty() {
                    return Err(invalid("tool_call_id"));
                }
                // An empty result (`""` or `[]`) is a valid tool outcome; absent content is not.
                if !content_present {
                    return Err(invalid("content"));
                }
                content = vec![ChatContent::ToolResult {
                    call_id,
                    content: text_content(&content),
                }];
            }
            ChatRole::User => {
                if tool_calls_present {
                    return Err(invalid("tool_calls"));
                }
                if tool_call_id_present {
                    return Err(invalid("tool_call_id"));
                }
                if !has_user_content(&content) {
                    return Err(invalid("content"));
                }
            }
            ChatRole::System | ChatRole::Developer => {
                if tool_calls_present {
                    return Err(invalid("tool_calls"));
                }
                if tool_call_id_present {
                    return Err(invalid("tool_call_id"));
                }
                if !has_nonempty_text(&content) {
                    return Err(invalid("content"));
                }
            }
        }

        if content.is_empty() {
            return Err(invalid("content"));
        }
        Ok(Some(ChatMessage { role, content }))
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ContentWire {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentPart {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: OptionalField<String>,
    #[serde(default)]
    input_audio: OptionalField<InputAudioWire>,
    #[serde(default)]
    image_url: OptionalField<ImageUrlWire>,
}

/// Content-part failure; the caller names the part's path.
enum PartError {
    Invalid,
    TooLarge,
}

impl ContentPart {
    fn into_core(
        self,
        role: ChatRole,
        max_audio_bytes: usize,
        total_audio_bytes: &mut usize,
        image_budget: &mut ImageBudget,
    ) -> Result<ChatContent, PartError> {
        let Self {
            kind,
            text,
            input_audio,
            image_url,
        } = self;
        match kind.as_str() {
            "text" => match (text, input_audio, image_url) {
                (OptionalField::Value(text), OptionalField::Missing, OptionalField::Missing) => {
                    Ok(ChatContent::Text { text })
                }
                _ => Err(PartError::Invalid),
            },
            "input_audio" if role == ChatRole::User => match (text, input_audio, image_url) {
                (OptionalField::Missing, OptionalField::Value(audio), OptionalField::Missing) => {
                    audio.into_core(max_audio_bytes, total_audio_bytes)
                }
                _ => Err(PartError::Invalid),
            },
            "image_url" if role == ChatRole::User => match (text, input_audio, image_url) {
                (OptionalField::Missing, OptionalField::Missing, OptionalField::Value(wire)) => {
                    let image = ValidatedImage::from_data_url(wire.url).map_err(image_error)?;
                    image_budget.add(&image).map_err(image_error)?;
                    Ok(ChatContent::InputImage { image })
                }
                _ => Err(PartError::Invalid),
            },
            _ => Err(PartError::Invalid),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageUrlWire {
    url: String,
}

fn image_error(error: ImageValidationError) -> PartError {
    match error {
        ImageValidationError::Invalid => PartError::Invalid,
        ImageValidationError::TooLarge => PartError::TooLarge,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputAudioWire {
    data: String,
    format: String,
}

impl InputAudioWire {
    fn into_core(
        self,
        max_audio_bytes: usize,
        total_audio_bytes: &mut usize,
    ) -> Result<ChatContent, PartError> {
        let format = match self.format.as_str() {
            "wav" => InputAudioFormat::Wav,
            "mp3" => InputAudioFormat::Mp3,
            _ => return Err(PartError::Invalid),
        };
        let max_audio_bytes = max_audio_bytes.min(MAX_INPUT_AUDIO_BYTES);
        let decoded_len = decoded_base64_len(&self.data)?;
        if decoded_len > max_audio_bytes.saturating_sub(*total_audio_bytes) {
            return Err(PartError::TooLarge);
        }
        let mut bytes = vec![0; decoded_len];
        let written = STANDARD
            .decode_slice(self.data.as_bytes(), &mut bytes)
            .map_err(|_| PartError::Invalid)?;
        if written != decoded_len {
            return Err(PartError::Invalid);
        }
        let audio =
            ValidatedAudio::with_max_bytes(format, bytes, max_audio_bytes).map_err(|error| {
                match error {
                    AudioValidationError::EmptyBytes => PartError::Invalid,
                    AudioValidationError::TooLarge => PartError::TooLarge,
                }
            })?;
        *total_audio_bytes = total_audio_bytes
            .checked_add(audio.bytes().len())
            .ok_or(PartError::TooLarge)?;
        Ok(ChatContent::InputAudio { audio })
    }
}

fn decoded_base64_len(data: &str) -> Result<usize, PartError> {
    if !data.len().is_multiple_of(4) {
        return Err(PartError::Invalid);
    }
    let padding = data
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    if padding > 2 || data.as_bytes()[..data.len() - padding].contains(&b'=') {
        return Err(PartError::Invalid);
    }
    (data.len() / 4)
        .checked_mul(3)
        .and_then(|length| length.checked_sub(padding))
        .ok_or(PartError::Invalid)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallWire {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: CallFunction,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallFunction {
    name: String,
    arguments: String,
}

#[derive(Default)]
pub(super) enum OptionalField<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<'de, T> Deserialize<'de> for OptionalField<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Self::Value(value),
            None => Self::Null,
        })
    }
}

fn has_nonempty_text(content: &[ChatContent]) -> bool {
    content.iter().any(|item| match item {
        ChatContent::Text { text } => !text.is_empty(),
        ChatContent::ToolCall { .. }
        | ChatContent::ToolResult { .. }
        | ChatContent::InputAudio { .. }
        | ChatContent::InputImage { .. }
        | ChatContent::Reasoning { .. }
        | ChatContent::ReasoningText { .. } => false,
    })
}

fn has_user_content(content: &[ChatContent]) -> bool {
    content.iter().any(|item| match item {
        ChatContent::Text { text } => !text.is_empty(),
        ChatContent::InputAudio { .. } | ChatContent::InputImage { .. } => true,
        ChatContent::ToolCall { .. }
        | ChatContent::ToolResult { .. }
        | ChatContent::Reasoning { .. }
        | ChatContent::ReasoningText { .. } => false,
    })
}

fn text_content(content: &[ChatContent]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            ChatContent::Text { text } => Some(text.as_str()),
            ChatContent::ToolCall { .. }
            | ChatContent::ToolResult { .. }
            | ChatContent::InputAudio { .. }
            | ChatContent::InputImage { .. }
            | ChatContent::Reasoning { .. }
            | ChatContent::ReasoningText { .. } => None,
        })
        .collect()
}
