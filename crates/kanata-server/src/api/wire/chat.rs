use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::core::{ChatRequest, Extensions, FunctionTool, PromptCacheKey};

use super::{
    ChatWireError,
    messages::{MessageWire, OptionalField},
    options::OptionsWire,
    serde_error,
    tools::ToolChoiceWire,
    tools::ToolWire,
};

/// `/v1/chat/completions` body. Unknown members fail with their JSON path. `store`,
/// `parallel_tool_calls` and `prompt_cache_key` are client hints, accepted only with
/// values Kanata can honour.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct ChatWire {
    model: String,
    messages: Vec<MessageWire>,
    #[serde(default)]
    tools: Vec<ToolWire>,
    #[serde(default)]
    tool_choice: ToolChoiceWire,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    #[serde(deserialize_with = "reject_null")]
    stream_options: Option<StreamOptions>,
    #[serde(default)]
    response_format: OptionalField<Value>,
    #[serde(default)]
    temperature: OptionalField<Value>,
    #[serde(default)]
    top_p: OptionalField<Value>,
    #[serde(default)]
    seed: OptionalField<Value>,
    #[serde(default)]
    stop: OptionalField<Value>,
    #[serde(default)]
    presence_penalty: OptionalField<Value>,
    #[serde(default)]
    frequency_penalty: OptionalField<Value>,
    #[serde(default)]
    max_tokens: OptionalField<Value>,
    #[serde(default)]
    max_completion_tokens: OptionalField<Value>,
    #[serde(default)]
    reasoning_effort: OptionalField<Value>,
    #[serde(default)]
    #[serde(deserialize_with = "reject_null_kwargs")]
    chat_template_kwargs: Option<ChatTemplateKwargs>,
    /// Only `false` (no stored completion) can be honoured.
    #[serde(default)]
    store: Option<bool>,
    /// Only `true`, the backend default, can be honoured.
    #[serde(default)]
    parallel_tool_calls: Option<bool>,
    #[serde(default)]
    prompt_cache_key: Option<String>,
    #[serde(default)]
    extensions: Extensions,
}

pub(in crate::api) fn parse(
    bytes: &[u8],
    max_audio_bytes: usize,
) -> Result<(ChatRequest, bool), ChatWireError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let wire: ChatWire =
        serde_path_to_error::deserialize(&mut deserializer).map_err(serde_error)?;
    deserializer.end().map_err(|_| ChatWireError::Malformed)?;
    wire.into_core(max_audio_bytes)
}

impl ChatWire {
    pub(in crate::api) fn into_core(
        self,
        max_audio_bytes: usize,
    ) -> Result<(ChatRequest, bool), ChatWireError> {
        if self.model.is_empty() {
            return Err(ChatWireError::param("model"));
        }
        if self.messages.is_empty() {
            return Err(ChatWireError::param("messages"));
        }
        if !self.stream && self.stream_options.is_some() {
            return Err(ChatWireError::param("stream_options"));
        }
        if self.store == Some(true) {
            return Err(ChatWireError::param("store"));
        }
        if self.parallel_tool_calls == Some(false) {
            return Err(ChatWireError::param("parallel_tool_calls"));
        }
        let prompt_cache_key = self
            .prompt_cache_key
            .map(PromptCacheKey::new)
            .transpose()
            .map_err(|_| ChatWireError::param("prompt_cache_key"))?;

        let mut options = OptionsWire {
            response_format: self.response_format,
            temperature: self.temperature,
            top_p: self.top_p,
            seed: self.seed,
            stop: self.stop,
            presence_penalty: self.presence_penalty,
            frequency_penalty: self.frequency_penalty,
            max_tokens: self.max_tokens,
            max_completion_tokens: self.max_completion_tokens,
            reasoning_effort: self.reasoning_effort,
        }
        .into_core()?;
        options.enable_thinking = self
            .chat_template_kwargs
            .and_then(|kwargs| kwargs.enable_thinking);

        let tools = into_tools(self.tools)?;
        let tool_choice = self
            .tool_choice
            .into_core(&tools)
            .map_err(|_| ChatWireError::param("tool_choice"))?;
        let mut total_audio_bytes = 0;
        let mut image_budget = crate::core::ImageBudget::default();
        let messages = self
            .messages
            .into_iter()
            .enumerate()
            .filter_map(|(index, message)| {
                message
                    .into_core(
                        index,
                        max_audio_bytes,
                        &mut total_audio_bytes,
                        &mut image_budget,
                    )
                    .transpose()
            })
            .collect::<Result<Vec<_>, _>>()?;
        if messages.is_empty() {
            return Err(ChatWireError::param("messages"));
        }
        let include_usage = self
            .stream_options
            .is_some_and(|options| options.include_usage);

        Ok((
            ChatRequest {
                model: crate::core::ModelAlias(self.model),
                messages,
                tools,
                tool_choice,
                stream: self.stream,
                options,
                prompt_cache_key,
                include_encrypted_reasoning: false,
                extensions: self.extensions,
            },
            include_usage,
        ))
    }
}

fn into_tools(wire_tools: Vec<ToolWire>) -> Result<Vec<FunctionTool>, ChatWireError> {
    let mut names = std::collections::BTreeSet::new();
    wire_tools
        .into_iter()
        .enumerate()
        .map(|(index, tool)| {
            tool.into_core()
                .ok()
                .filter(|tool| names.insert(tool.name.clone()))
                .ok_or_else(|| ChatWireError::param(format!("tools[{index}]")))
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamOptions {
    #[serde(default)]
    include_usage: bool,
}

/// Only the thinking switch; other template kwargs are rejected.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatTemplateKwargs {
    #[serde(default)]
    enable_thinking: Option<bool>,
}

fn reject_null_kwargs<'de, D>(deserializer: D) -> Result<Option<ChatTemplateKwargs>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<ChatTemplateKwargs>::deserialize(deserializer)?
        .map(Some)
        .ok_or_else(|| D::Error::custom("chat_template_kwargs cannot be null"))
}

fn reject_null<'de, D>(deserializer: D) -> Result<Option<StreamOptions>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<StreamOptions>::deserialize(deserializer)?
        .map(Some)
        .ok_or_else(|| D::Error::custom("stream_options cannot be null"))
}
