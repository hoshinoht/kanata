use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Chat,
    Transcription,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustZone {
    Local,
    PrivateNetwork,
    External,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ModelAlias(pub String);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RouteSelector {
    pub model_alias: ModelAlias,
    pub operation: Operation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RouteIdentity {
    pub route_id: String,
    pub upstream_id: String,
    #[serde(flatten)]
    pub selector: RouteSelector,
}

impl RouteIdentity {
    pub fn new(
        route_id: impl Into<String>,
        upstream_id: impl Into<String>,
        model_alias: ModelAlias,
        operation: Operation,
    ) -> Self {
        Self {
            route_id: route_id.into(),
            upstream_id: upstream_id.into(),
            selector: RouteSelector {
                model_alias,
                operation,
            },
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExtensionKey(String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtensionKeyError {
    Empty,
    TooLong,
    NotNamespaced,
    InvalidSegment,
}

impl ExtensionKey {
    pub fn parse(value: impl Into<String>) -> Result<Self, ExtensionKeyError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ExtensionKeyError::Empty);
        }
        if value.len() > 128 {
            return Err(ExtensionKeyError::TooLong);
        }
        let segments: Vec<_> = value.split('.').collect();
        if segments.len() < 2 {
            return Err(ExtensionKeyError::NotNamespaced);
        }
        if segments
            .iter()
            .any(|segment| !valid_extension_segment(segment))
        {
            return Err(ExtensionKeyError::InvalidSegment);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn valid_extension_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    matches!(chars.next(), Some(character) if character.is_ascii_lowercase())
        && chars.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

impl Serialize for ExtensionKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ExtensionKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

impl fmt::Display for ExtensionKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "extension key is empty",
            Self::TooLong => "extension key exceeds 128 bytes",
            Self::NotNamespaced => "extension key must contain a namespace",
            Self::InvalidSegment => "extension key contains an invalid namespace segment",
        })
    }
}

impl std::error::Error for ExtensionKeyError {}

pub const MAX_EXTENSION_ENTRIES: usize = 16;
pub const MAX_EXTENSION_BYTES: usize = 8 * 1024;
pub const MAX_EXTENSION_DEPTH: usize = 4;
pub const MAX_INPUT_AUDIO_BYTES: usize = 25 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtensionError {
    TooManyEntries,
    TooLarge,
    TooDeep,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Extensions(BTreeMap<ExtensionKey, Value>);

impl Extensions {
    pub fn try_from_map(entries: BTreeMap<ExtensionKey, Value>) -> Result<Self, ExtensionError> {
        validate_extensions(&entries)?;
        Ok(Self(entries))
    }

    pub fn insert(&mut self, key: ExtensionKey, value: Value) -> Result<(), ExtensionError> {
        let mut entries = self.0.clone();
        entries.insert(key, value);
        validate_extensions(&entries)?;
        self.0 = entries;
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ExtensionKey, &Value)> {
        self.0.iter()
    }
}

impl<'de> Deserialize<'de> for Extensions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let entries = BTreeMap::deserialize(deserializer)?;
        Self::try_from_map(entries).map_err(D::Error::custom)
    }
}

fn validate_extensions(entries: &BTreeMap<ExtensionKey, Value>) -> Result<(), ExtensionError> {
    if entries.len() > MAX_EXTENSION_ENTRIES {
        return Err(ExtensionError::TooManyEntries);
    }
    if entries
        .values()
        .any(|value| extension_depth(value) > MAX_EXTENSION_DEPTH)
    {
        return Err(ExtensionError::TooDeep);
    }
    if serde_json::to_vec(entries)
        .expect("JSON values serialize")
        .len()
        > MAX_EXTENSION_BYTES
    {
        return Err(ExtensionError::TooLarge);
    }
    Ok(())
}

fn extension_depth(value: &Value) -> usize {
    match value {
        Value::Array(values) => 1 + values.iter().map(extension_depth).max().unwrap_or(0),
        Value::Object(values) => 1 + values.values().map(extension_depth).max().unwrap_or(0),
        _ => 0,
    }
}

impl fmt::Display for ExtensionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooManyEntries => "extension count exceeds the contract limit",
            Self::TooLarge => "extensions exceed the contract byte limit",
            Self::TooDeep => "extension nesting exceeds the contract depth limit",
        })
    }
}

impl std::error::Error for ExtensionError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestContext {
    pub request_id: String,
    pub route: RouteIdentity,
    pub trust_zone: TrustZone,
    #[serde(default)]
    pub extensions: Extensions,
}

impl RequestContext {
    pub fn check_request(&self, request: &Request) -> Result<(), RouteError> {
        let request_operation = request.operation();
        if self.route.selector.operation != request_operation {
            return Err(RouteError::OperationMismatch {
                route_operation: self.route.selector.operation,
                request_operation,
            });
        }
        if self.route.selector.model_alias != *request.model_alias() {
            return Err(RouteError::ModelAliasMismatch {
                route_model_alias: self.route.selector.model_alias.clone(),
                request_model_alias: request.model_alias().clone(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteError {
    OperationMismatch {
        route_operation: Operation,
        request_operation: Operation,
    },
    ModelAliasMismatch {
        route_model_alias: ModelAlias,
        request_model_alias: ModelAlias,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestValidationError {
    RequiredToolChoiceWithoutTools,
    UndeclaredToolChoice { name: String },
    InputAudioRoleMismatch,
    InputAudioTooLarge,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RoutedRequestError {
    Route(RouteError),
    Request(RequestValidationError),
}

pub struct RoutedRequest {
    context: RequestContext,
    request: Request,
}

impl RoutedRequest {
    pub fn new(context: RequestContext, request: Request) -> Result<Self, RoutedRequestError> {
        context
            .check_request(&request)
            .map_err(RoutedRequestError::Route)?;
        request.validate().map_err(RoutedRequestError::Request)?;
        Ok(Self { context, request })
    }

    pub fn context(&self) -> &RequestContext {
        &self.context
    }

    pub fn request(&self) -> &Request {
        &self.request
    }

    pub fn into_parts(self) -> (RequestContext, Request) {
        (self.context, self.request)
    }
}

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
            extensions: raw.extensions,
        };
        chat.validate_audio()
            .map_err(|_| D::Error::custom("invalid chat audio content"))?;
        Ok(chat)
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Wire field name used for the output token limit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MaxTokensParam {
    #[default]
    MaxTokens,
    MaxCompletionTokens,
}

impl MaxTokensParam {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }
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
            reasoning_effort: raw.reasoning_effort,
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
}

impl SamplingOptions {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "none" => Self::None,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::Xhigh,
            "max" => Self::Max,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
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
    Text { text: String },
    ToolCall { call: ToolCall },
    ToolResult { call_id: String, content: String },
    InputAudio { audio: ValidatedAudio },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputAudioFormat {
    Wav,
    Mp3,
}

#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatedAudio {
    format: InputAudioFormat,
    bytes: Vec<u8>,
}

impl fmt::Debug for ValidatedAudio {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedAudio")
            .field("format", &self.format)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioValidationError {
    EmptyBytes,
    TooLarge,
}

impl fmt::Display for AudioValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyBytes => "audio bytes are empty",
            Self::TooLarge => "audio bytes exceed the contract limit",
        })
    }
}

impl std::error::Error for AudioValidationError {}

impl ValidatedAudio {
    pub fn new(format: InputAudioFormat, bytes: Vec<u8>) -> Result<Self, AudioValidationError> {
        Self::with_max_bytes(format, bytes, MAX_INPUT_AUDIO_BYTES)
    }

    pub fn with_max_bytes(
        format: InputAudioFormat,
        bytes: Vec<u8>,
        max_bytes: usize,
    ) -> Result<Self, AudioValidationError> {
        if bytes.is_empty() {
            return Err(AudioValidationError::EmptyBytes);
        }
        if bytes.len() > max_bytes.min(MAX_INPUT_AUDIO_BYTES) {
            return Err(AudioValidationError::TooLarge);
        }
        Ok(Self { format, bytes })
    }

    pub fn format(&self) -> InputAudioFormat {
        self.format
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl<'de> Deserialize<'de> for ValidatedAudio {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawAudio {
            format: InputAudioFormat,
            bytes: Vec<u8>,
        }

        let raw = RawAudio::deserialize(deserializer)?;
        Self::new(raw.format, raw.bytes).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatedFile {
    file_name: String,
    media_type: String,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileValidationError {
    EmptyName,
    InvalidMediaType,
    EmptyBytes,
}

impl fmt::Display for FileValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyName => "file name is empty",
            Self::InvalidMediaType => "file media type is invalid",
            Self::EmptyBytes => "file bytes are empty",
        })
    }
}

impl std::error::Error for FileValidationError {}

impl ValidatedFile {
    pub fn new(
        file_name: impl Into<String>,
        media_type: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Result<Self, FileValidationError> {
        let file_name = file_name.into();
        let media_type = media_type.into();
        if file_name.trim().is_empty() {
            return Err(FileValidationError::EmptyName);
        }
        if !valid_media_type(&media_type) {
            return Err(FileValidationError::InvalidMediaType);
        }
        if bytes.is_empty() {
            return Err(FileValidationError::EmptyBytes);
        }
        Ok(Self {
            file_name,
            media_type,
            bytes,
        })
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl<'de> Deserialize<'de> for ValidatedFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawFile {
            file_name: String,
            media_type: String,
            bytes: Vec<u8>,
        }

        let raw = RawFile::deserialize(deserializer)?;
        Self::new(raw.file_name, raw.media_type, raw.bytes).map_err(D::Error::custom)
    }
}

fn valid_media_type(value: &str) -> bool {
    let mut parts = value.split('/');
    let Some(type_name) = parts.next() else {
        return false;
    };
    let Some(subtype) = parts.next() else {
        return false;
    };
    parts.next().is_none() && valid_media_token(type_name) && valid_media_token(subtype)
}

fn valid_media_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-'))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptionRequest {
    pub model: ModelAlias,
    pub file: ValidatedFile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default)]
    pub extensions: Extensions,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Chat(ChatRequest),
    Transcription(TranscriptionRequest),
}

impl Request {
    pub fn operation(&self) -> Operation {
        match self {
            Self::Chat(_) => Operation::Chat,
            Self::Transcription(_) => Operation::Transcription,
        }
    }

    pub fn model_alias(&self) -> &ModelAlias {
        match self {
            Self::Chat(request) => &request.model,
            Self::Transcription(request) => &request.model,
        }
    }

    fn validate(&self) -> Result<(), RequestValidationError> {
        let Self::Chat(chat) = self else {
            return Ok(());
        };
        chat.validate_audio()?;
        match &chat.tool_choice {
            ToolChoice::Required if chat.tools.is_empty() => {
                Err(RequestValidationError::RequiredToolChoiceWithoutTools)
            }
            ToolChoice::Function { name } if !chat.tools.iter().any(|tool| tool.name == *name) => {
                Err(RequestValidationError::UndeclaredToolChoice { name: name.clone() })
            }
            _ => Ok(()),
        }
    }
}

impl ChatRequest {
    fn validate_audio(&self) -> Result<(), RequestValidationError> {
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
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelDescriptor {
    pub alias: ModelAlias,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelsResponse {
    pub models: Vec<ModelDescriptor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChatResponse {
    pub model: ModelAlias,
    pub message: ChatMessage,
    pub finish_reason: FinishReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TranscriptionResponse {
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Chat(ChatResponse),
    Transcription(TranscriptionResponse),
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

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Capabilities {
    pub operations: BTreeSet<Operation>,
    pub streaming_chat: bool,
    pub function_tools: bool,
    #[serde(default)]
    pub input_audio: bool,
    #[serde(default)]
    pub audio_streaming_chat: bool,
    #[serde(default)]
    pub audio_function_tools: bool,
    #[serde(default)]
    pub structured_output: bool,
    /// Covers temperature, top_p, seed and max output tokens.
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
    AudioStreamingUnavailable,
    AudioFunctionToolsUnavailable,
    ChatOptionUnavailable { param: &'static str },
}

impl CapabilityError {
    /// Wire parameter responsible for the rejection, when one is identifiable.
    pub fn param(&self) -> Option<&'static str> {
        match self {
            Self::ChatOptionUnavailable { param } => Some(param),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutPhase {
    Queue,
    Connect,
    Headers,
    FirstByte,
    Idle,
    Overall,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict,
    RateLimited,
    Timeout { phase: TimeoutPhase },
    Cancelled,
    UpstreamUnavailable,
    UpstreamFailure,
    UnsupportedOperation,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ErrorMapping {
    pub status: u16,
    pub code: &'static str,
    pub error_type: &'static str,
}

impl ErrorKind {
    pub const fn mapping(self) -> ErrorMapping {
        match self {
            Self::InvalidRequest => ErrorMapping {
                status: 400,
                code: "invalid_request",
                error_type: "invalid_request_error",
            },
            Self::Unauthorized => ErrorMapping {
                status: 401,
                code: "invalid_api_key",
                error_type: "authentication_error",
            },
            Self::Forbidden => ErrorMapping {
                status: 403,
                code: "permission_denied",
                error_type: "permission_error",
            },
            Self::NotFound => ErrorMapping {
                status: 404,
                code: "not_found",
                error_type: "invalid_request_error",
            },
            Self::Conflict => ErrorMapping {
                status: 409,
                code: "conflict",
                error_type: "invalid_request_error",
            },
            Self::RateLimited => ErrorMapping {
                status: 429,
                code: "rate_limit_exceeded",
                error_type: "rate_limit_error",
            },
            Self::Timeout { .. } => ErrorMapping {
                status: 504,
                code: "upstream_timeout",
                error_type: "api_error",
            },
            Self::Cancelled => ErrorMapping {
                status: 408,
                code: "request_cancelled",
                error_type: "api_error",
            },
            Self::UpstreamUnavailable => ErrorMapping {
                status: 503,
                code: "upstream_unavailable",
                error_type: "api_error",
            },
            Self::UpstreamFailure => ErrorMapping {
                status: 502,
                code: "upstream_failure",
                error_type: "api_error",
            },
            Self::UnsupportedOperation => ErrorMapping {
                status: 400,
                code: "unsupported_operation",
                error_type: "invalid_request_error",
            },
            Self::Internal => ErrorMapping {
                status: 500,
                code: "internal_error",
                error_type: "api_error",
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GatewayError {
    pub kind: ErrorKind,
}
