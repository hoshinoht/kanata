use super::*;

/// Upper bound for `prompt_cache_key`; harnesses send 36-byte UUIDs.
pub const MAX_PROMPT_CACHE_KEY_BYTES: usize = 64;

/// Upstream prefix-cache routing key: 1–64 bytes of printable ASCII (`0x20..=0x7e`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PromptCacheKey(String);

impl PromptCacheKey {
    pub fn new(value: String) -> Result<Self, InvalidChatOption> {
        ((1..=MAX_PROMPT_CACHE_KEY_BYTES).contains(&value.len())
            && value.bytes().all(|byte| matches!(byte, b' '..=b'~')))
        .then_some(Self(value))
        .ok_or(InvalidChatOption)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for PromptCacheKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

pub const MAX_RESPONSE_SCHEMA_BYTES: usize = 64 * 1024;
pub const MAX_RESPONSE_SCHEMA_DEPTH: usize = 32;
pub const MAX_RESPONSE_SCHEMA_NAME_BYTES: usize = 64;
pub const MAX_RESPONSE_SCHEMA_DESCRIPTION_BYTES: usize = 1024;
pub const MAX_OUTPUT_TOKENS: u32 = 1_048_576;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidChatOption;

impl fmt::Display for InvalidChatOption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("chat option is outside the contract bounds")
    }
}

impl std::error::Error for InvalidChatOption {}

/// Provider-neutral generation options; adapters encode only what their capabilities declare.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ChatOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(default, skip_serializing_if = "SamplingOptions::is_empty")]
    pub sampling: SamplingOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Wire field that carried `max_output_tokens`, for error attribution only.
    #[serde(skip)]
    pub max_output_tokens_param: MaxTokensParam,
    /// Wire field that carried `sampling.stop`, for error attribution only.
    #[serde(skip)]
    pub stop_param: StopParam,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Chat-template thinking switch (`chat_template_kwargs.enable_thinking`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_thinking: Option<bool>,
}

str_enum! {
    /// Wire field name used for the output token limit.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub enum MaxTokensParam {
        #[default]
        MaxTokens => "max_tokens",
        MaxCompletionTokens => "max_completion_tokens",
        MaxOutputTokens => "max_output_tokens",
    }
    pub const fn as_str;
}

str_enum! {
    /// Wire field name used for stop sequences.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub enum StopParam {
        #[default]
        Stop => "stop",
        StopSequences => "stop_sequences",
    }
    pub const fn as_str;
}

impl ChatOptions {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn validate_max_output_tokens(value: u64) -> Result<u32, InvalidChatOption> {
        u32::try_from(value)
            .ok()
            .filter(|value| (1..=MAX_OUTPUT_TOKENS).contains(value))
            .ok_or(InvalidChatOption)
    }

    /// Structured output only when the format constrains the reply; `text` is the default.
    pub fn uses_structured_output(&self) -> bool {
        self.response_format
            .as_ref()
            .is_some_and(|format| !matches!(format, ResponseFormat::Text))
    }

    /// First sampling-class parameter present, named as on the wire.
    pub fn sampling_param(&self) -> Option<&'static str> {
        if self.sampling.temperature.is_some() {
            Some("temperature")
        } else if self.sampling.top_p.is_some() {
            Some("top_p")
        } else if self.sampling.seed.is_some() {
            Some("seed")
        } else if !self.sampling.stop.is_empty() {
            Some(self.stop_param.as_str())
        } else if self.sampling.presence_penalty.is_some() {
            Some("presence_penalty")
        } else if self.sampling.frequency_penalty.is_some() {
            Some("frequency_penalty")
        } else if self.max_output_tokens.is_some() {
            Some(self.max_output_tokens_param.as_str())
        } else {
            None
        }
    }
}

impl<'de> Deserialize<'de> for ChatOptions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawChatOptions {
            #[serde(default)]
            response_format: Option<ResponseFormat>,
            #[serde(default)]
            sampling: SamplingOptions,
            #[serde(default)]
            max_output_tokens: Option<u64>,
            #[serde(default)]
            reasoning_effort: Option<ReasoningEffort>,
            #[serde(default)]
            enable_thinking: Option<bool>,
        }

        let raw = RawChatOptions::deserialize(deserializer)?;
        let max_output_tokens = raw
            .max_output_tokens
            .map(Self::validate_max_output_tokens)
            .transpose()
            .map_err(D::Error::custom)?;
        Ok(Self {
            response_format: raw.response_format,
            sampling: raw.sampling,
            max_output_tokens,
            max_output_tokens_param: MaxTokensParam::default(),
            stop_param: StopParam::default(),
            reasoning_effort: raw.reasoning_effort,
            enable_thinking: raw.enable_thinking,
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SamplingOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<Temperature>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<TopP>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_stop"
    )]
    pub stop: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<Penalty>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<Penalty>,
}

/// Stop sequences per request, the common chat API limit.
pub const MAX_STOP_SEQUENCES: usize = 4;
/// Bytes per stop sequence.
pub const MAX_STOP_SEQUENCE_BYTES: usize = 256;

impl SamplingOptions {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// At most four sequences of 1–256 bytes each.
    pub fn validate_stop(stop: Vec<String>) -> Result<Vec<String>, InvalidChatOption> {
        (stop.len() <= MAX_STOP_SEQUENCES
            && stop
                .iter()
                .all(|sequence| (1..=MAX_STOP_SEQUENCE_BYTES).contains(&sequence.len())))
        .then_some(stop)
        .ok_or(InvalidChatOption)
    }
}

fn deserialize_stop<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    SamplingOptions::validate_stop(Vec::deserialize(deserializer)?).map_err(D::Error::custom)
}

/// Finite temperature in `[0, 2]`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Temperature(f64);

// Finite by construction, so reflexive equality holds.
impl Eq for Temperature {}

impl Temperature {
    pub fn new(value: f64) -> Result<Self, InvalidChatOption> {
        (value.is_finite() && (0.0..=2.0).contains(&value))
            .then_some(Self(value))
            .ok_or(InvalidChatOption)
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for Temperature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(f64::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Finite nucleus-sampling mass in `(0, 1]`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TopP(f64);

// Finite by construction, so reflexive equality holds.
impl Eq for TopP {}

impl TopP {
    pub fn new(value: f64) -> Result<Self, InvalidChatOption> {
        (value.is_finite() && value > 0.0 && value <= 1.0)
            .then_some(Self(value))
            .ok_or(InvalidChatOption)
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for TopP {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(f64::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Finite presence or frequency penalty in `[-2, 2]`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Penalty(f64);

// Finite by construction, so reflexive equality holds.
impl Eq for Penalty {}

impl Penalty {
    pub fn new(value: f64) -> Result<Self, InvalidChatOption> {
        (value.is_finite() && (-2.0..=2.0).contains(&value))
            .then_some(Self(value))
            .ok_or(InvalidChatOption)
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for Penalty {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(f64::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

str_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
    #[serde(rename_all = "lowercase")]
    pub enum ReasoningEffort {
        None => "none",
        Minimal => "minimal",
        Low => "low",
        Medium => "medium",
        High => "high",
        Xhigh => "xhigh",
        Max => "max",
    }
    pub const fn as_str;
    pub fn parse;
}

/// Serializes in the public wire `response_format` shape.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    JsonObject,
    JsonSchema { json_schema: JsonSchemaFormat },
}

impl ResponseFormat {
    /// Strict parse: unknown keys and nulls are rejected at every level.
    pub fn from_value(value: Value) -> Result<Self, InvalidChatOption> {
        let Value::Object(mut object) = value else {
            return Err(InvalidChatOption);
        };
        let kind = object.remove("type").ok_or(InvalidChatOption)?;
        let format = match kind.as_str() {
            Some("text") => Self::Text,
            Some("json_object") => Self::JsonObject,
            Some("json_schema") => {
                let Some(Value::Object(schema)) = object.remove("json_schema") else {
                    return Err(InvalidChatOption);
                };
                Self::JsonSchema {
                    json_schema: JsonSchemaFormat::from_object(schema)?,
                }
            }
            _ => return Err(InvalidChatOption),
        };
        object.is_empty().then_some(format).ok_or(InvalidChatOption)
    }
}

impl<'de> Deserialize<'de> for ResponseFormat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_value(Value::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct JsonSchemaFormat {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
}

impl JsonSchemaFormat {
    pub fn new(
        name: String,
        description: Option<String>,
        schema: Value,
        strict: Option<bool>,
    ) -> Result<Self, InvalidChatOption> {
        let valid_name = !name.is_empty()
            && name.len() <= MAX_RESPONSE_SCHEMA_NAME_BYTES
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
        let valid_description = description
            .as_ref()
            .is_none_or(|value| value.len() <= MAX_RESPONSE_SCHEMA_DESCRIPTION_BYTES);
        if !valid_name
            || !valid_description
            || !schema.is_object()
            || extension_depth(&schema) > MAX_RESPONSE_SCHEMA_DEPTH
            || serde_json::to_vec(&schema)
                .expect("JSON values serialize")
                .len()
                > MAX_RESPONSE_SCHEMA_BYTES
        {
            return Err(InvalidChatOption);
        }
        Ok(Self {
            name,
            description,
            schema,
            strict,
        })
    }

    fn from_object(mut object: serde_json::Map<String, Value>) -> Result<Self, InvalidChatOption> {
        let Some(Value::String(name)) = object.remove("name") else {
            return Err(InvalidChatOption);
        };
        let schema = object.remove("schema").ok_or(InvalidChatOption)?;
        let strict = match object.remove("strict") {
            None => None,
            Some(Value::Bool(strict)) => Some(strict),
            Some(_) => return Err(InvalidChatOption),
        };
        let description = match object.remove("description") {
            None => None,
            Some(Value::String(description)) => Some(description),
            Some(_) => return Err(InvalidChatOption),
        };
        if !object.is_empty() {
            return Err(InvalidChatOption);
        }
        Self::new(name, description, schema, strict)
    }
}
