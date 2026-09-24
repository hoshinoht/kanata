use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::core::{ChatRequest, Extensions, FunctionTool};

use super::{
    messages::{ChatWireError, MessageWire, OptionalField},
    options::OptionsWire,
    tools::ToolChoiceWire,
    tools::ToolWire,
};

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
    max_tokens: OptionalField<Value>,
    #[serde(default)]
    max_completion_tokens: OptionalField<Value>,
    #[serde(default)]
    reasoning_effort: OptionalField<Value>,
    #[serde(default)]
    extensions: Extensions,
}

impl ChatWire {
    pub(in crate::api) fn into_core(
        self,
        max_audio_bytes: usize,
    ) -> Result<(ChatRequest, bool), ChatWireError> {
        if self.model.is_empty() || self.messages.is_empty() {
            return Err(ChatWireError::Invalid);
        }
        if !self.stream && self.stream_options.is_some() {
            return Err(ChatWireError::Invalid);
        }

        let options = OptionsWire {
            response_format: self.response_format,
            temperature: self.temperature,
            top_p: self.top_p,
            seed: self.seed,
            max_tokens: self.max_tokens,
            max_completion_tokens: self.max_completion_tokens,
            reasoning_effort: self.reasoning_effort,
        }
        .into_core()?;

        let tools = into_tools(self.tools).map_err(|_| ChatWireError::Invalid)?;
        let tool_choice = self
            .tool_choice
            .into_core(&tools)
            .map_err(|_| ChatWireError::Invalid)?;
        let mut total_audio_bytes = 0;
        let messages = self
            .messages
            .into_iter()
            .map(|message| message.into_core(max_audio_bytes, &mut total_audio_bytes))
            .collect::<Result<Vec<_>, _>>()?;
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
                extensions: self.extensions,
            },
            include_usage,
        ))
    }
}

fn into_tools(wire_tools: Vec<ToolWire>) -> Result<Vec<FunctionTool>, ()> {
    let mut names = std::collections::BTreeSet::new();
    wire_tools
        .into_iter()
        .map(|tool| {
            let tool = tool.into_core()?;
            names.insert(tool.name.clone()).then_some(tool).ok_or(())
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamOptions {
    #[serde(default)]
    include_usage: bool,
}

fn reject_null<'de, D>(deserializer: D) -> Result<Option<StreamOptions>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<StreamOptions>::deserialize(deserializer)?
        .map(Some)
        .ok_or_else(|| D::Error::custom("stream_options cannot be null"))
}
