use crate::adapter::{
    chat_completions::{
        embeddings,
        response::{
            self as wire, CompletionTokensDetails, FunctionCallPayload, PromptTokensDetails,
        },
    },
    upstream_failure,
};

use serde::{Deserialize, de::IgnoredAny};

use crate::core::{ChatResponse, GatewayError, ModelAlias, TranscriptionResponse, Usage};

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
    #[serde(default, rename = "openrouter_metadata")]
    _openrouter_metadata: Option<IgnoredAny>,
    #[serde(default, rename = "service_tier")]
    _service_tier: Option<String>,
    #[serde(default, rename = "provider")]
    _provider: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChoicePayload {
    index: u64,
    message: MessagePayload,
    finish_reason: String,
    #[serde(default, rename = "native_finish_reason")]
    _native_finish_reason: Option<String>,
    #[serde(default, rename = "logprobs")]
    _logprobs: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessagePayload {
    role: String,
    content: Option<String>,
    // A refusal without content is not an empty answer; it still fails.
    #[serde(default)]
    refusal: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    // Structured reasoning is accepted and not forwarded.
    #[serde(default, rename = "reasoning_details")]
    _reasoning_details: Option<serde_json::Value>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallPayload>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallPayload {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "index")]
    _index: Option<u64>,
    function: FunctionCallPayload,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UsagePayload {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    #[serde(default, rename = "cost")]
    _cost: Option<IgnoredAny>,
    #[serde(default, rename = "cost_details")]
    _cost_details: Option<IgnoredAny>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
    #[serde(default, rename = "is_byok")]
    _is_byok: Option<IgnoredAny>,
    #[serde(default, rename = "server_tool_use_details")]
    _server_tool_use_details: Option<IgnoredAny>,
    #[serde(default, rename = "server_tool_use")]
    _server_tool_use: Option<IgnoredAny>,
}

impl UsagePayload {
    pub(super) fn normalize(self) -> Result<Usage, GatewayError> {
        wire::checked_usage(
            self.prompt_tokens,
            self.completion_tokens,
            self.total_tokens,
            self.prompt_tokens_details,
            self.completion_tokens_details,
        )
    }
}

pub(super) fn decode(bytes: &[u8], public_model: ModelAlias) -> Result<ChatResponse, GatewayError> {
    let payload: CompletionPayload =
        serde_json::from_slice(bytes).map_err(|_| upstream_failure())?;
    let choice = wire::only_choice(
        &payload.id,
        &payload.object,
        &payload.model,
        payload.system_fingerprint.as_deref(),
        payload.choices,
    )?;
    let message = choice.message;
    if choice.index != 0 || message.role != "assistant" {
        return Err(upstream_failure());
    }
    let finish_reason = wire::parse_finish_reason(&choice.finish_reason)?;
    let reasoning = message.reasoning.filter(|text| !text.is_empty());
    let calls = message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|call| call.function.into_call(call.kind, call.id));
    let (content, has_tool_calls) = wire::assistant_content(message.content, calls)?;
    let refused = message.refusal.is_some_and(|text| !text.is_empty());
    wire::check_finish(finish_reason, &content, has_tool_calls, refused)?;
    let usage = payload.usage.map(UsagePayload::normalize).transpose()?;
    Ok(wire::chat_response(
        public_model,
        content,
        finish_reason,
        usage,
        reasoning,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TranscriptionPayload {
    text: String,
    #[serde(default, rename = "usage")]
    _usage: Option<IgnoredAny>,
}

pub(super) fn decode_transcription(bytes: &[u8]) -> Result<TranscriptionResponse, GatewayError> {
    let payload: TranscriptionPayload =
        serde_json::from_slice(bytes).map_err(|_| upstream_failure())?;
    let text = payload.text.trim();
    if text.is_empty() {
        return Err(upstream_failure());
    }
    Ok(TranscriptionResponse {
        text: text.to_owned(),
    })
}

/// The documented `embeddings` response; `id` and `model` are accepted and not forwarded.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EmbeddingListPayload {
    #[serde(default, rename = "id")]
    _id: Option<String>,
    object: String,
    #[serde(rename = "model")]
    _model: String,
    data: Vec<EmbeddingPayload>,
    #[serde(default)]
    usage: Option<EmbeddingUsagePayload>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbeddingPayload {
    object: String,
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbeddingUsagePayload {
    prompt_tokens: u64,
    total_tokens: u64,
    #[serde(default, rename = "cost")]
    _cost: Option<IgnoredAny>,
    #[serde(default, rename = "cost_details")]
    _cost_details: Option<IgnoredAny>,
    #[serde(default, rename = "is_byok")]
    _is_byok: Option<IgnoredAny>,
    #[serde(default, rename = "prompt_tokens_details")]
    _prompt_tokens_details: Option<IgnoredAny>,
}

impl From<EmbeddingListPayload> for embeddings::List {
    fn from(payload: EmbeddingListPayload) -> Self {
        Self {
            object: payload.object,
            data: payload
                .data
                .into_iter()
                .map(|item| embeddings::Item {
                    object: item.object,
                    index: item.index,
                    embedding: item.embedding,
                })
                .collect(),
            usage: payload.usage.map(|usage| embeddings::TokenUsage {
                prompt_tokens: usage.prompt_tokens,
                total_tokens: usage.total_tokens,
            }),
        }
    }
}
