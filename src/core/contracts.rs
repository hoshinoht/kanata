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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
    #[serde(default)]
    pub extensions: Extensions,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Capabilities {
    pub operations: BTreeSet<Operation>,
    pub streaming_chat: bool,
    pub function_tools: bool,
}

impl Capabilities {
    pub fn new(operations: impl IntoIterator<Item = Operation>) -> Self {
        Self {
            operations: operations.into_iter().collect(),
            streaming_chat: false,
            function_tools: false,
        }
    }

    pub fn check_request(&self, request: &Request) -> Result<(), CapabilityError> {
        let operation = request.operation();
        if !self.operations.contains(&operation) {
            return Err(CapabilityError::UnsupportedOperation { operation });
        }
        if let Request::Chat(chat) = request {
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
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityError {
    UnsupportedOperation { operation: Operation },
    StreamingUnavailable,
    FunctionToolsUnavailable,
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
