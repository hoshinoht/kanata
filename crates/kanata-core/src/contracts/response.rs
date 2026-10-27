use super::*;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChatResponse {
    pub model: ModelAlias,
    pub message: ChatMessage,
    pub finish_reason: FinishReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Upstream reasoning text or summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Encrypted reasoning to replay on the next turn, when the client asked for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_items: Vec<ReasoningItem>,
}

pub use kanata_contract::limits::MAX_REASONING_BYTES;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TranscriptionResponse {
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Chat(ChatResponse),
    Transcription(TranscriptionResponse),
    Embeddings(EmbeddingResponse),
    Speech(crate::SpeechResponse),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// Part of `output_tokens`, when the upstream reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Part of `input_tokens` served from the upstream prefix cache; set it through
    /// `with_cached_input_tokens`, which drops counts above `input_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
}

impl Usage {
    /// Records the cached prefix count; a count above `input_tokens` is contradictory
    /// upstream data and is dropped rather than failing the completion.
    pub fn with_cached_input_tokens(mut self, cached: u64) -> Self {
        self.cached_input_tokens = (cached <= self.input_tokens).then_some(cached);
        self
    }

    /// Drops the cached count where it must not be disclosed (public listener).
    pub fn without_cached_input_tokens(mut self) -> Self {
        self.cached_input_tokens = None;
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NormalizedEvent {
    ChatStarted {
        model: ModelAlias,
    },
    ChatTextDelta {
        text: String,
    },
    ChatReasoningDelta {
        text: String,
    },
    ChatReasoningItem {
        item: ReasoningItem,
    },
    ChatToolCallDelta {
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        arguments_delta: String,
    },
    ChatCompleted {
        finish_reason: FinishReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
}
