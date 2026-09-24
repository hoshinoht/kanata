use serde_json::Value;

use crate::core::{
    ChatOptions, MaxTokensParam, ReasoningEffort, ResponseFormat, SamplingOptions, Temperature,
    TopP,
};

use super::messages::{ChatWireError, OptionalField};

pub(super) struct OptionsWire {
    pub(super) response_format: OptionalField<Value>,
    pub(super) temperature: OptionalField<Value>,
    pub(super) top_p: OptionalField<Value>,
    pub(super) seed: OptionalField<Value>,
    pub(super) max_tokens: OptionalField<Value>,
    pub(super) max_completion_tokens: OptionalField<Value>,
    pub(super) reasoning_effort: OptionalField<Value>,
}

impl OptionsWire {
    pub(super) fn into_core(self) -> Result<ChatOptions, ChatWireError> {
        let max_tokens = field(self.max_tokens, "max_tokens", max_output_tokens)?;
        let max_completion_tokens = field(
            self.max_completion_tokens,
            "max_completion_tokens",
            max_output_tokens,
        )?;
        if max_tokens.is_some() && max_completion_tokens.is_some() {
            return Err(ChatWireError::InvalidParam("max_completion_tokens"));
        }
        Ok(ChatOptions {
            response_format: field(self.response_format, "response_format", |value| {
                ResponseFormat::from_value(value).ok()
            })?,
            sampling: SamplingOptions {
                temperature: field(self.temperature, "temperature", |value| {
                    Temperature::new(value.as_f64()?).ok()
                })?,
                top_p: field(self.top_p, "top_p", |value| TopP::new(value.as_f64()?).ok())?,
                seed: field(self.seed, "seed", |value| value.as_i64())?,
            },
            max_output_tokens_param: if max_completion_tokens.is_some() {
                MaxTokensParam::MaxCompletionTokens
            } else {
                MaxTokensParam::MaxTokens
            },
            max_output_tokens: max_tokens.or(max_completion_tokens),
            reasoning_effort: field(self.reasoning_effort, "reasoning_effort", |value| {
                ReasoningEffort::parse(value.as_str()?)
            })?,
            enable_thinking: None,
        })
    }
}

fn field<T>(
    value: OptionalField<Value>,
    param: &'static str,
    parse: impl FnOnce(Value) -> Option<T>,
) -> Result<Option<T>, ChatWireError> {
    match value {
        OptionalField::Missing => Ok(None),
        OptionalField::Null => Err(ChatWireError::InvalidParam(param)),
        OptionalField::Value(value) => parse(value)
            .map(Some)
            .ok_or(ChatWireError::InvalidParam(param)),
    }
}

fn max_output_tokens(value: Value) -> Option<u32> {
    ChatOptions::validate_max_output_tokens(value.as_u64()?).ok()
}
