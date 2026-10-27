use serde_json::Value;

use crate::core::{
    ChatOptions, MaxTokensParam, Penalty, ReasoningEffort, ResponseFormat, SamplingOptions,
    Temperature, TopP,
};

use super::{ChatWireError, messages::OptionalField};

pub(super) struct OptionsWire {
    pub(super) response_format: OptionalField<Value>,
    pub(super) temperature: OptionalField<Value>,
    pub(super) top_p: OptionalField<Value>,
    pub(super) seed: OptionalField<Value>,
    pub(super) stop: OptionalField<Value>,
    pub(super) presence_penalty: OptionalField<Value>,
    pub(super) frequency_penalty: OptionalField<Value>,
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
            return Err(ChatWireError::param("max_completion_tokens"));
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
                stop: field(self.stop, "stop", stop)?.unwrap_or_default(),
                presence_penalty: field(self.presence_penalty, "presence_penalty", penalty)?,
                frequency_penalty: field(self.frequency_penalty, "frequency_penalty", penalty)?,
            },
            stop_param: Default::default(),
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
        OptionalField::Null => Err(ChatWireError::param(param)),
        OptionalField::Value(value) => parse(value)
            .map(Some)
            .ok_or_else(|| ChatWireError::param(param)),
    }
}

fn max_output_tokens(value: Value) -> Option<u32> {
    ChatOptions::validate_max_output_tokens(value.as_u64()?).ok()
}

/// A single string or an array of strings.
fn stop(value: Value) -> Option<Vec<String>> {
    let stop = match value {
        Value::String(sequence) => vec![sequence],
        Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                Value::String(sequence) => Some(sequence),
                _ => None,
            })
            .collect::<Option<_>>()?,
        _ => return None,
    };
    SamplingOptions::validate_stop(stop).ok()
}

fn penalty(value: Value) -> Option<Penalty> {
    Penalty::new(value.as_f64()?).ok()
}
