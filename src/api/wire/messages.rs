use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;

use crate::core::{
    AudioValidationError, ChatContent, ChatMessage, ChatRole, InputAudioFormat,
    MAX_INPUT_AUDIO_BYTES, ToolCall, ValidatedAudio,
};

use super::valid_name;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::api) enum ChatWireError {
    Invalid,
    InvalidParam(&'static str),
    TooLarge,
}

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
}

impl MessageWire {
    pub(super) fn into_core(
        self,
        max_audio_bytes: usize,
        total_audio_bytes: &mut usize,
    ) -> Result<ChatMessage, ChatWireError> {
        let role = match self.role.as_str() {
            "system" => ChatRole::System,
            "developer" => ChatRole::Developer,
            "user" => ChatRole::User,
            "assistant" => ChatRole::Assistant,
            "tool" => ChatRole::Tool,
            _ => return Err(ChatWireError::Invalid),
        };
        let mut content = match self.content {
            OptionalField::Missing | OptionalField::Null => Vec::new(),
            OptionalField::Value(ContentWire::Text(text)) => {
                vec![ChatContent::Text { text }]
            }
            OptionalField::Value(ContentWire::Parts(parts)) => parts
                .into_iter()
                .map(|part| part.into_core(role, max_audio_bytes, total_audio_bytes))
                .collect::<Result<Vec<_>, _>>()?,
        };

        let tool_calls_field = self.tool_calls;
        let tool_calls_present = !matches!(&tool_calls_field, OptionalField::Missing);
        let tool_calls = match tool_calls_field {
            OptionalField::Missing => Vec::new(),
            OptionalField::Null => {
                if role != ChatRole::Assistant {
                    return Err(ChatWireError::Invalid);
                }
                Vec::new()
            }
            OptionalField::Value(tool_calls) => tool_calls,
        };
        let tool_call_id = self.tool_call_id;
        let tool_call_id_present = !matches!(&tool_call_id, OptionalField::Missing);

        match role {
            ChatRole::Assistant => {
                if tool_call_id_present {
                    return Err(ChatWireError::Invalid);
                }
                for call in tool_calls {
                    if call.kind != "function"
                        || !valid_name(&call.function.name)
                        || call.id.is_empty()
                    {
                        return Err(ChatWireError::Invalid);
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
                    return Err(ChatWireError::Invalid);
                }
            }
            ChatRole::Tool => {
                if tool_calls_present {
                    return Err(ChatWireError::Invalid);
                }
                let OptionalField::Value(call_id) = tool_call_id else {
                    return Err(ChatWireError::Invalid);
                };
                if call_id.is_empty() {
                    return Err(ChatWireError::Invalid);
                }
                let text = text_content(&content);
                if text.is_empty() {
                    return Err(ChatWireError::Invalid);
                }
                content = vec![ChatContent::ToolResult {
                    call_id,
                    content: text,
                }];
            }
            ChatRole::User => {
                if tool_calls_present || tool_call_id_present || !has_user_content(&content) {
                    return Err(ChatWireError::Invalid);
                }
            }
            ChatRole::System | ChatRole::Developer => {
                if tool_calls_present || tool_call_id_present || !has_nonempty_text(&content) {
                    return Err(ChatWireError::Invalid);
                }
            }
        }

        if content.is_empty() {
            return Err(ChatWireError::Invalid);
        }
        Ok(ChatMessage { role, content })
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
}

impl ContentPart {
    fn into_core(
        self,
        role: ChatRole,
        max_audio_bytes: usize,
        total_audio_bytes: &mut usize,
    ) -> Result<ChatContent, ChatWireError> {
        let Self {
            kind,
            text,
            input_audio,
        } = self;
        match kind.as_str() {
            "text" => match (text, input_audio) {
                (OptionalField::Value(text), OptionalField::Missing) => {
                    Ok(ChatContent::Text { text })
                }
                _ => Err(ChatWireError::Invalid),
            },
            "input_audio" if role == ChatRole::User => match (text, input_audio) {
                (OptionalField::Missing, OptionalField::Value(audio)) => {
                    audio.into_core(max_audio_bytes, total_audio_bytes)
                }
                _ => Err(ChatWireError::Invalid),
            },
            _ => Err(ChatWireError::Invalid),
        }
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
    ) -> Result<ChatContent, ChatWireError> {
        let format = match self.format.as_str() {
            "wav" => InputAudioFormat::Wav,
            "mp3" => InputAudioFormat::Mp3,
            _ => return Err(ChatWireError::Invalid),
        };
        let max_audio_bytes = max_audio_bytes.min(MAX_INPUT_AUDIO_BYTES);
        let decoded_len = decoded_base64_len(&self.data)?;
        if decoded_len > max_audio_bytes.saturating_sub(*total_audio_bytes) {
            return Err(ChatWireError::TooLarge);
        }
        let mut bytes = vec![0; decoded_len];
        let written = STANDARD
            .decode_slice(self.data.as_bytes(), &mut bytes)
            .map_err(|_| ChatWireError::Invalid)?;
        if written != decoded_len {
            return Err(ChatWireError::Invalid);
        }
        let audio =
            ValidatedAudio::with_max_bytes(format, bytes, max_audio_bytes).map_err(|error| {
                match error {
                    AudioValidationError::EmptyBytes => ChatWireError::Invalid,
                    AudioValidationError::TooLarge => ChatWireError::TooLarge,
                }
            })?;
        *total_audio_bytes = total_audio_bytes
            .checked_add(audio.bytes().len())
            .ok_or(ChatWireError::TooLarge)?;
        Ok(ChatContent::InputAudio { audio })
    }
}

fn decoded_base64_len(data: &str) -> Result<usize, ChatWireError> {
    if !data.len().is_multiple_of(4) {
        return Err(ChatWireError::Invalid);
    }
    let padding = data
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    if padding > 2 || data.as_bytes()[..data.len() - padding].contains(&b'=') {
        return Err(ChatWireError::Invalid);
    }
    (data.len() / 4)
        .checked_mul(3)
        .and_then(|length| length.checked_sub(padding))
        .ok_or(ChatWireError::Invalid)
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
        | ChatContent::InputAudio { .. } => false,
    })
}

fn has_user_content(content: &[ChatContent]) -> bool {
    content.iter().any(|item| match item {
        ChatContent::Text { text } => !text.is_empty(),
        ChatContent::InputAudio { .. } => true,
        ChatContent::ToolCall { .. } | ChatContent::ToolResult { .. } => false,
    })
}

fn text_content(content: &[ChatContent]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            ChatContent::Text { text } => Some(text.as_str()),
            ChatContent::ToolCall { .. }
            | ChatContent::ToolResult { .. }
            | ChatContent::InputAudio { .. } => None,
        })
        .collect()
}
