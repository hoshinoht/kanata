use super::*;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChatRequest {
    pub model: ModelAlias,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub tools: Vec<FunctionTool>,
    #[serde(default)]
    pub tool_choice: ToolChoice,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "ChatOptions::is_empty")]
    pub options: ChatOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<PromptCacheKey>,
    /// Ask a replaying upstream to return `ReasoningItem`s for the next turn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_encrypted_reasoning: bool,
    #[serde(default)]
    pub extensions: Extensions,
}

impl<'de> Deserialize<'de> for ChatRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawChatRequest {
            model: ModelAlias,
            messages: Vec<ChatMessage>,
            #[serde(default)]
            tools: Vec<FunctionTool>,
            #[serde(default)]
            tool_choice: ToolChoice,
            #[serde(default)]
            stream: bool,
            #[serde(default)]
            options: ChatOptions,
            #[serde(default)]
            prompt_cache_key: Option<PromptCacheKey>,
            #[serde(default)]
            include_encrypted_reasoning: bool,
            #[serde(default)]
            extensions: Extensions,
        }

        let raw = RawChatRequest::deserialize(deserializer)?;
        let chat = Self {
            model: raw.model,
            messages: raw.messages,
            tools: raw.tools,
            tool_choice: raw.tool_choice,
            stream: raw.stream,
            options: raw.options,
            prompt_cache_key: raw.prompt_cache_key,
            include_encrypted_reasoning: raw.include_encrypted_reasoning,
            extensions: raw.extensions,
        };
        chat.validate_audio()
            .map_err(|_| D::Error::custom("invalid chat audio content"))?;
        chat.validate_images()
            .map_err(|_| D::Error::custom("invalid chat image content"))?;
        chat.validate_reasoning()
            .map_err(|_| D::Error::custom("reasoning content outside an assistant message"))?;
        Ok(chat)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChatMessage {
    pub role: ChatRole,
    #[serde(default)]
    pub content: Vec<ChatContent>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolChoice {
    None,
    #[default]
    Auto,
    Required,
    Function {
        name: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChatContent {
    Text {
        text: String,
    },
    ToolCall {
        call: ToolCall,
    },
    ToolResult {
        call_id: String,
        content: String,
    },
    InputAudio {
        audio: ValidatedAudio,
    },
    InputImage {
        image: crate::core::ValidatedImage,
    },
    /// Opaque upstream reasoning replayed in an assistant turn; adapters that cannot
    /// replay it skip it.
    Reasoning {
        item: ReasoningItem,
    },
    /// Plaintext reasoning a client replays on an assistant turn (chat `reasoning_content`);
    /// adapters whose upstream accepts it forward it, others skip it. Never encrypted.
    ReasoningText {
        text: String,
    },
}

impl ChatContent {
    /// Whether this part is replayed reasoning, encrypted or plaintext.
    pub fn is_reasoning(&self) -> bool {
        matches!(self, Self::Reasoning { .. } | Self::ReasoningText { .. })
    }
}

impl ChatRequest {
    /// History for an upstream adapter: assistant turns holding nothing but reasoning
    /// (encrypted or plaintext) are dropped, so the remaining turns never go empty.
    /// Callers still skip or forward reasoning parts inside the turns that remain.
    pub fn messages_without_reasoning(&self) -> impl Iterator<Item = &ChatMessage> {
        self.messages.iter().filter(|message| {
            message.content.is_empty() || !message.content.iter().all(ChatContent::is_reasoning)
        })
    }

    pub(super) fn validate_images(&self) -> Result<(), RequestValidationError> {
        let mut budget = crate::core::ImageBudget::default();
        for message in &self.messages {
            for content in &message.content {
                if let ChatContent::InputImage { image } = content {
                    if message.role != ChatRole::User {
                        return Err(RequestValidationError::InputImageRoleMismatch);
                    }
                    budget
                        .add(image)
                        .map_err(|_| RequestValidationError::InputImagesTooLarge)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_audio(&self) -> Result<(), RequestValidationError> {
        let mut total_bytes = 0_usize;
        for message in &self.messages {
            for content in &message.content {
                let ChatContent::InputAudio { audio } = content else {
                    continue;
                };
                if message.role != ChatRole::User {
                    return Err(RequestValidationError::InputAudioRoleMismatch);
                }
                total_bytes = total_bytes
                    .checked_add(audio.bytes().len())
                    .ok_or(RequestValidationError::InputAudioTooLarge)?;
                if total_bytes > MAX_INPUT_AUDIO_BYTES {
                    return Err(RequestValidationError::InputAudioTooLarge);
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_reasoning(&self) -> Result<(), RequestValidationError> {
        let misplaced = self.messages.iter().any(|message| {
            message.role != ChatRole::Assistant
                && message.content.iter().any(ChatContent::is_reasoning)
        });
        if misplaced {
            Err(RequestValidationError::ReasoningRoleMismatch)
        } else {
            Ok(())
        }
    }
}
