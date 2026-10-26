use super::*;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Capabilities {
    pub operations: BTreeSet<Operation>,
    pub streaming_chat: bool,
    pub function_tools: bool,
    #[serde(default)]
    pub input_audio: bool,
    #[serde(default)]
    pub input_images: bool,
    #[serde(default)]
    pub audio_streaming_chat: bool,
    #[serde(default)]
    pub audio_function_tools: bool,
    #[serde(default)]
    pub structured_output: bool,
    /// Covers temperature, top_p, seed, stop, penalties and max output tokens.
    #[serde(default)]
    pub sampling_controls: bool,
    #[serde(default)]
    pub reasoning_control: bool,
}

impl Capabilities {
    pub fn new(operations: impl IntoIterator<Item = Operation>) -> Self {
        Self {
            operations: operations.into_iter().collect(),
            ..Self::default()
        }
    }

    pub fn check_request(&self, request: &Request) -> Result<(), CapabilityError> {
        let operation = request.operation();
        if !self.operations.contains(&operation) {
            return Err(CapabilityError::UnsupportedOperation { operation });
        }
        if let Request::Chat(chat) = request {
            if !self.input_images
                && chat.messages.iter().any(|message| {
                    message
                        .content
                        .iter()
                        .any(|part| matches!(part, ChatContent::InputImage { .. }))
                })
            {
                return Err(CapabilityError::InputImagesUnavailable);
            }
            let has_audio = chat.messages.iter().any(|message| {
                message
                    .content
                    .iter()
                    .any(|content| matches!(content, ChatContent::InputAudio { .. }))
            });
            let has_audio_tools = !chat.tools.is_empty()
                || !matches!(chat.tool_choice, ToolChoice::None | ToolChoice::Auto)
                || chat.messages.iter().any(|message| {
                    message.content.iter().any(|content| {
                        matches!(
                            content,
                            ChatContent::ToolCall { .. } | ChatContent::ToolResult { .. }
                        )
                    })
                });
            if has_audio && !self.input_audio {
                return Err(CapabilityError::InputAudioUnavailable);
            }
            if has_audio && chat.stream && !self.audio_streaming_chat {
                return Err(CapabilityError::AudioStreamingUnavailable);
            }
            if has_audio && has_audio_tools && !self.audio_function_tools {
                return Err(CapabilityError::AudioFunctionToolsUnavailable);
            }
            if chat.stream && !self.streaming_chat {
                return Err(CapabilityError::StreamingUnavailable);
            }
            if !chat.tools.is_empty() && !self.function_tools {
                return Err(CapabilityError::FunctionToolsUnavailable);
            }
            if !matches!(chat.tool_choice, ToolChoice::None | ToolChoice::Auto)
                && !self.function_tools
            {
                return Err(CapabilityError::FunctionToolsUnavailable);
            }
            if chat.options.uses_structured_output() && !self.structured_output {
                return Err(CapabilityError::ChatOptionUnavailable {
                    param: "response_format",
                });
            }
            if let Some(param) = chat.options.sampling_param()
                && !self.sampling_controls
            {
                return Err(CapabilityError::ChatOptionUnavailable { param });
            }
            if chat.options.reasoning_effort.is_some() && !self.reasoning_control {
                return Err(CapabilityError::ChatOptionUnavailable {
                    param: "reasoning_effort",
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityError {
    UnsupportedOperation { operation: Operation },
    StreamingUnavailable,
    FunctionToolsUnavailable,
    InputAudioUnavailable,
    InputImagesUnavailable,
    AudioStreamingUnavailable,
    AudioFunctionToolsUnavailable,
    ChatOptionUnavailable { param: &'static str },
}

impl CapabilityError {
    /// Wire parameter responsible for the rejection, when one is identifiable.
    pub fn param(&self) -> Option<&'static str> {
        match self {
            Self::ChatOptionUnavailable { param } => Some(param),
            Self::InputImagesUnavailable => Some("messages"),
            _ => None,
        }
    }
}
