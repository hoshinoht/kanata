use std::{fmt, marker::PhantomData};

use serde::{
    Deserialize, Deserializer,
    de::{self, SeqAccess, Visitor},
};
use serde_json::Value;

use crate::core::{
    ChatContent, ChatMessage, ChatOptions, ChatRequest, ChatRole, Extensions, FunctionTool,
    ImageBudget, ImageValidationError, MAX_IMAGE_BASE64_BYTES, ModelAlias, ReasoningEffort,
    SamplingOptions, StopParam, Temperature, ToolCall, ToolChoice, TopP, ValidatedImage,
};

use super::super::wire::{ChatWireError, serde_error};

/// Bound on `context_management.edits`.
const MAX_CONTEXT_EDITS: usize = 8;
/// Bytes of `metadata.user_id`.
const MAX_USER_ID_BYTES: usize = 512;
/// Bytes of a replayed `tool_use.id` or `tool_result.tool_use_id`.
const MAX_TOOL_USE_ID_BYTES: usize = 256;

/// `/v1/messages` body. Unknown members fail with their JSON path; `cache_control`,
/// `metadata`, `thinking` (adaptive or disabled) and `context_management` thinking-clear
/// edits are accepted and never forwarded.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessagesWire {
    model: String,
    max_tokens: u64,
    messages: Vec<MessageWire>,
    #[serde(default)]
    system: Option<TextOrBlocks<TextBlock>>,
    /// Typed per entry, after hosted tool types are refused by `type`.
    #[serde(default)]
    tools: Vec<Value>,
    #[serde(default)]
    tool_choice: Option<ToolChoiceWire>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    metadata: Option<Metadata>,
    #[serde(default)]
    thinking: Option<Thinking>,
    #[serde(default)]
    output_config: Option<OutputConfig>,
    #[serde(default)]
    context_management: Option<ContextManagement>,
}

pub(in crate::api) fn parse(bytes: &[u8]) -> Result<ChatRequest, ChatWireError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let wire: MessagesWire =
        serde_path_to_error::deserialize(&mut deserializer).map_err(serde_error)?;
    deserializer.end().map_err(|_| ChatWireError::Malformed)?;
    wire.into_core()
}

impl MessagesWire {
    fn into_core(self) -> Result<ChatRequest, ChatWireError> {
        if self.model.is_empty() {
            return Err(ChatWireError::param("model"));
        }
        if self.messages.is_empty() {
            return Err(ChatWireError::param("messages"));
        }
        let max_output_tokens = ChatOptions::validate_max_output_tokens(self.max_tokens)
            .map_err(|_| ChatWireError::param("max_tokens"))?;
        let temperature = self
            .temperature
            .map(Temperature::new)
            .transpose()
            .map_err(|_| ChatWireError::param("temperature"))?;
        let top_p = self
            .top_p
            .map(TopP::new)
            .transpose()
            .map_err(|_| ChatWireError::param("top_p"))?;
        let stop = self
            .stop_sequences
            .map(SamplingOptions::validate_stop)
            .transpose()
            .map_err(|_| ChatWireError::param("stop_sequences"))?
            .unwrap_or_default();
        if self
            .metadata
            .and_then(|metadata| metadata.user_id)
            .is_some_and(|user_id| user_id.len() > MAX_USER_ID_BYTES)
        {
            return Err(ChatWireError::param("metadata.user_id"));
        }
        if matches!(self.thinking, Some(Thinking::Enabled { .. })) {
            return Err(ChatWireError::param("thinking"));
        }
        let reasoning_effort = match self.output_config {
            None => None,
            Some(config) => {
                if config.format.is_some() {
                    return Err(ChatWireError::param("output_config.format"));
                }
                config
                    .effort
                    .map(|effort| {
                        ReasoningEffort::parse(&effort)
                            .ok_or_else(|| ChatWireError::param("output_config.effort"))
                    })
                    .transpose()?
            }
        };
        if let Some(management) = self.context_management {
            if management.edits.len() > MAX_CONTEXT_EDITS {
                return Err(ChatWireError::param("context_management.edits"));
            }
            // Kanata emits no thinking blocks, so clearing them is a no-op.
            for (index, edit) in management.edits.iter().enumerate() {
                if edit
                    .kind
                    .strip_prefix("clear_thinking_")
                    .is_none_or(str::is_empty)
                {
                    return Err(ChatWireError::param(format!(
                        "context_management.edits[{index}]"
                    )));
                }
            }
        }
        let tools = into_tools(self.tools)?;
        let tool_choice = match self.tool_choice {
            None => ToolChoice::Auto,
            Some(choice) => choice.into_core(&tools)?,
        };

        let mut messages = Vec::with_capacity(self.messages.len() + 1);
        if let Some(system) = self.system {
            let content: Vec<_> = text_parts(system)
                .filter(|text| !text.is_empty())
                .map(|text| ChatContent::Text { text })
                .collect();
            if !content.is_empty() {
                messages.push(ChatMessage {
                    role: ChatRole::System,
                    content,
                });
            }
        }
        let mut images = ImageBudget::default();
        for (index, message) in self.messages.into_iter().enumerate() {
            message.push_core(index, &mut messages, &mut images)?;
        }
        if messages
            .iter()
            .all(|message| message.role == ChatRole::System)
        {
            return Err(ChatWireError::param("messages"));
        }

        Ok(ChatRequest {
            model: ModelAlias(self.model),
            messages,
            tools,
            tool_choice,
            stream: self.stream,
            options: ChatOptions {
                sampling: SamplingOptions {
                    temperature,
                    top_p,
                    stop,
                    ..SamplingOptions::default()
                },
                max_output_tokens: Some(max_output_tokens),
                stop_param: StopParam::StopSequences,
                reasoning_effort,
                ..ChatOptions::default()
            },
            prompt_cache_key: None,
            include_encrypted_reasoning: false,
            extensions: Extensions::default(),
        })
    }
}

/// A string, or an array of blocks with exact JSON paths for failures inside it.
enum TextOrBlocks<T> {
    Text(String),
    Blocks(Vec<T>),
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for TextOrBlocks<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TextOrBlocksVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for TextOrBlocksVisitor<T> {
            type Value = TextOrBlocks<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a string or an array of content blocks")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(TextOrBlocks::Text(value.to_owned()))
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(TextOrBlocks::Text(value))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut blocks = Vec::new();
                while let Some(block) = seq.next_element()? {
                    blocks.push(block);
                }
                Ok(TextOrBlocks::Blocks(blocks))
            }
        }

        deserializer.deserialize_any(TextOrBlocksVisitor(PhantomData))
    }
}

/// Prompt-cache hint; accepted wherever the protocol allows it and never forwarded.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheControl {
    #[serde(rename = "type")]
    _kind: CacheKind,
    #[serde(default, rename = "ttl")]
    _ttl: Option<CacheTtl>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CacheKind {
    Ephemeral,
}

#[derive(Deserialize)]
enum CacheTtl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum TextKind {
    Text,
}

/// Text block of `system` and `tool_result.content`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextBlock {
    #[serde(rename = "type")]
    _kind: TextKind,
    text: String,
    #[serde(default, rename = "cache_control")]
    _cache_control: Option<CacheControl>,
}

fn text_parts(value: TextOrBlocks<TextBlock>) -> impl Iterator<Item = String> {
    let (text, blocks) = match value {
        TextOrBlocks::Text(text) => (Some(text), Vec::new()),
        TextOrBlocks::Blocks(blocks) => (None, blocks),
    };
    text.into_iter()
        .chain(blocks.into_iter().map(|block| block.text))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageWire {
    role: RoleWire,
    content: TextOrBlocks<Block>,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum RoleWire {
    User,
    Assistant,
    System,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Block {
    Text {
        text: String,
        #[serde(default, rename = "cache_control")]
        _cache_control: Option<CacheControl>,
    },
    Image {
        source: ImageSource,
        #[serde(default, rename = "cache_control")]
        _cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(default, rename = "cache_control")]
        _cache_control: Option<CacheControl>,
    },
    /// `is_error` is carried by the client in the result text; the flag is dropped.
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<TextOrBlocks<TextBlock>>,
        #[serde(default, rename = "is_error")]
        _is_error: Option<bool>,
        #[serde(default, rename = "cache_control")]
        _cache_control: Option<CacheControl>,
    },
    /// Earlier-turn thinking is dropped: Kanata routes neither emit nor replay it.
    Thinking {
        #[serde(rename = "thinking")]
        _thinking: String,
        #[serde(default, rename = "signature")]
        _signature: Option<String>,
    },
    RedactedThinking {
        #[serde(rename = "data")]
        _data: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ImageSource {
    Base64 {
        media_type: String,
        data: String,
    },
    Url {
        #[serde(rename = "url")]
        _url: String,
    },
}

impl MessageWire {
    /// Appends the IR messages for `messages[index]`; a user turn's tool results become
    /// tool messages in block order.
    fn push_core(
        self,
        index: usize,
        messages: &mut Vec<ChatMessage>,
        images: &mut ImageBudget,
    ) -> Result<(), ChatWireError> {
        let path = |member: &str| ChatWireError::param(format!("messages[{index}].{member}"));
        let block_path = |block: usize| path(&format!("content[{block}]"));
        let role = self.role;
        let blocks = match self.content {
            TextOrBlocks::Text(text) if text.is_empty() && role != RoleWire::Assistant => {
                return Err(path("content"));
            }
            TextOrBlocks::Text(text) => vec![Block::Text {
                text,
                _cache_control: None,
            }],
            TextOrBlocks::Blocks(blocks) => blocks,
        };
        // A replayed empty answer (`""`, `[]` or only empty text blocks) maps to no turn.
        if role == RoleWire::Assistant
            && blocks
                .iter()
                .all(|block| matches!(block, Block::Text { text, .. } if text.is_empty()))
        {
            return Ok(());
        }
        if blocks.is_empty() {
            return Err(path("content"));
        }
        let chat_role = match role {
            RoleWire::User => ChatRole::User,
            RoleWire::Assistant => ChatRole::Assistant,
            RoleWire::System => ChatRole::System,
        };
        let mut content = Vec::new();
        for (block_index, block) in blocks.into_iter().enumerate() {
            match (role, block) {
                (_, Block::Text { text, .. }) if text.is_empty() => {
                    return Err(block_path(block_index));
                }
                (_, Block::Text { text, .. }) => content.push(ChatContent::Text { text }),
                (RoleWire::User, Block::Image { source, .. }) => {
                    let image = image(source).map_err(|error| match error {
                        ImageValidationError::TooLarge => ChatWireError::TooLarge,
                        ImageValidationError::Invalid => block_path(block_index),
                    })?;
                    images.add(&image).map_err(|_| ChatWireError::TooLarge)?;
                    content.push(ChatContent::InputImage { image });
                }
                (
                    RoleWire::User,
                    Block::ToolResult {
                        tool_use_id,
                        content: result,
                        ..
                    },
                ) => {
                    if !valid_tool_use_id(&tool_use_id) {
                        return Err(path(&format!("content[{block_index}].tool_use_id")));
                    }
                    if !content.is_empty() {
                        messages.push(ChatMessage {
                            role: ChatRole::User,
                            content: std::mem::take(&mut content),
                        });
                    }
                    messages.push(ChatMessage {
                        role: ChatRole::Tool,
                        content: vec![ChatContent::ToolResult {
                            call_id: tool_use_id,
                            content: result.map(text_parts).into_iter().flatten().collect(),
                        }],
                    });
                }
                (
                    RoleWire::Assistant,
                    Block::ToolUse {
                        id, name, input, ..
                    },
                ) => {
                    if !valid_tool_use_id(&id) || !valid_name(&name) || !input.is_object() {
                        return Err(block_path(block_index));
                    }
                    content.push(ChatContent::ToolCall {
                        call: ToolCall {
                            id,
                            name,
                            arguments: input.to_string(),
                        },
                    });
                }
                (RoleWire::Assistant, Block::Thinking { .. } | Block::RedactedThinking { .. }) => {}
                _ => return Err(block_path(block_index)),
            }
        }
        if !content.is_empty() {
            messages.push(ChatMessage {
                role: chat_role,
                content,
            });
        } else if role != RoleWire::User {
            // Only a user turn made of tool results maps to no message of its own role.
            return Err(path("content"));
        }
        Ok(())
    }
}

fn image(source: ImageSource) -> Result<ValidatedImage, ImageValidationError> {
    let ImageSource::Base64 { media_type, data } = source else {
        return Err(ImageValidationError::Invalid);
    };
    if !matches!(media_type.as_str(), "image/png" | "image/jpeg") {
        return Err(ImageValidationError::Invalid);
    }
    // Bounded before the data URL copies it.
    if data.len() > MAX_IMAGE_BASE64_BYTES {
        return Err(ImageValidationError::TooLarge);
    }
    ValidatedImage::from_data_url(format!("data:{media_type};base64,{data}"))
}

fn valid_tool_use_id(id: &str) -> bool {
    (1..=MAX_TOOL_USE_ID_BYTES).contains(&id.len())
}

/// Function names as chat accepts them.
fn valid_name(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Client-defined tool. `strict` is accepted and dropped like chat's.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolWire {
    name: String,
    #[serde(default)]
    description: Option<String>,
    input_schema: Value,
    #[serde(default, rename = "strict")]
    _strict: Option<bool>,
    #[serde(default, rename = "cache_control")]
    _cache_control: Option<CacheControl>,
}

fn into_tools(tools: Vec<Value>) -> Result<Vec<FunctionTool>, ChatWireError> {
    let mut names = std::collections::BTreeSet::new();
    tools
        .into_iter()
        .enumerate()
        .map(|(index, tool)| {
            let path = format!("tools[{index}]");
            let Value::Object(mut tool) = tool else {
                return Err(ChatWireError::param(path));
            };
            // Hosted tools (any other `type`) have no chat equivalent.
            if tool
                .remove("type")
                .is_some_and(|kind| kind.as_str() != Some("custom"))
            {
                return Err(ChatWireError::param(path));
            }
            let tool: ToolWire = serde_path_to_error::deserialize(Value::Object(tool))
                .map_err(|error| nested(&path, serde_error(error)))?;
            if !valid_name(&tool.name)
                || !tool.input_schema.is_object()
                || !names.insert(tool.name.clone())
            {
                return Err(ChatWireError::param(path));
            }
            Ok(FunctionTool {
                name: tool.name,
                description: tool.description,
                parameters: tool.input_schema,
            })
        })
        .collect()
}

/// Prefixes a failure found inside the member at `parent`.
fn nested(parent: &str, error: ChatWireError) -> ChatWireError {
    match error {
        ChatWireError::InvalidParam(param) => ChatWireError::param(format!("{parent}.{param}")),
        ChatWireError::Malformed => ChatWireError::param(parent.to_owned()),
        ChatWireError::TooLarge => ChatWireError::TooLarge,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolChoiceWire {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    disable_parallel_tool_use: Option<bool>,
}

impl ToolChoiceWire {
    fn into_core(self, tools: &[FunctionTool]) -> Result<ToolChoice, ChatWireError> {
        // Upstreams decide parallel calls themselves; only the default can be honoured.
        if self.disable_parallel_tool_use == Some(true) {
            return Err(ChatWireError::param(
                "tool_choice.disable_parallel_tool_use",
            ));
        }
        match (self.kind.as_str(), self.name) {
            ("auto", None) => Ok(ToolChoice::Auto),
            ("none", None) => Ok(ToolChoice::None),
            ("any", None) if !tools.is_empty() => Ok(ToolChoice::Required),
            ("tool", Some(name)) if tools.iter().any(|tool| tool.name == name) => {
                Ok(ToolChoice::Function { name })
            }
            _ => Err(ChatWireError::param("tool_choice")),
        }
    }
}

/// Never forwarded upstream.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    #[serde(default)]
    user_id: Option<String>,
}

/// Adaptive and disabled thinking are accepted; no thinking blocks are emitted.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Thinking {
    Adaptive {
        #[serde(default, rename = "display")]
        _display: Option<ThinkingDisplay>,
    },
    Disabled {},
    /// A thinking budget cannot be honoured; refused with `param=thinking`.
    Enabled {
        #[serde(default, rename = "budget_tokens")]
        _budget_tokens: Option<Value>,
        #[serde(default, rename = "display")]
        _display: Option<Value>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ThinkingDisplay {
    Summarized,
    Omitted,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputConfig {
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    format: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextManagement {
    #[serde(default)]
    edits: Vec<ContextEdit>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextEdit {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "keep")]
    _keep: Option<Keep>,
}

/// `"all"` or `{"type":"thinking_turns","value":N}`.
#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum Keep {
    All(KeepAll),
    Turns {
        #[serde(rename = "type")]
        _kind: KeepTurnsKind,
        #[serde(rename = "value")]
        _value: u32,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum KeepAll {
    All,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum KeepTurnsKind {
    ThinkingTurns,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(body: Value) -> String {
        match parse(body.to_string().as_bytes()) {
            Err(ChatWireError::InvalidParam(param)) => param.into_owned(),
            Err(other) => panic!("{body}: {other:?}"),
            Ok(_) => panic!("{body}: accepted"),
        }
    }

    fn base() -> Value {
        serde_json::json!({"model":"m","max_tokens":16,"messages":[{"role":"user","content":"hi"}]})
    }

    fn with(member: &str, value: Value) -> Value {
        let mut body = base();
        body[member] = value;
        body
    }

    #[test]
    fn failures_name_the_offending_member() {
        let cases = [
            (with("top_k", serde_json::json!(5)), "top_k"),
            (
                with("service_tier", serde_json::json!("auto")),
                "service_tier",
            ),
            (with("max_tokens", serde_json::json!(0)), "max_tokens"),
            (
                with(
                    "system",
                    serde_json::json!([{"type":"text","text":"s","extra":1}]),
                ),
                "system[0].extra",
            ),
            (
                with(
                    "system",
                    serde_json::json!([{"type":"text","text":"s","cache_control":{"type":"ephemeral","ttl":"2h"}}]),
                ),
                "system[0].cache_control.ttl",
            ),
            (
                with(
                    "thinking",
                    serde_json::json!({"type":"enabled","budget_tokens":1024}),
                ),
                "thinking",
            ),
            (
                with("output_config", serde_json::json!({"effort":"extreme"})),
                "output_config.effort",
            ),
            (
                with(
                    "output_config",
                    serde_json::json!({"format":{"type":"json_schema"}}),
                ),
                "output_config.format",
            ),
            (
                with(
                    "context_management",
                    serde_json::json!({"edits":[{"type":"clear_tool_uses_20250919"}]}),
                ),
                "context_management.edits[0]",
            ),
            (
                with(
                    "context_management",
                    serde_json::json!({"edits":vec![serde_json::json!({"type":"clear_thinking_x"}); 9]}),
                ),
                "context_management.edits",
            ),
            (
                with(
                    "tools",
                    serde_json::json!([{"type":"web_search_20250305","name":"web_search"}]),
                ),
                "tools[0]",
            ),
            (
                with(
                    "tools",
                    serde_json::json!([{"name":"t","input_schema":{},"defer_loading":true}]),
                ),
                "tools[0].defer_loading",
            ),
            (
                with(
                    "tool_choice",
                    serde_json::json!({"type":"auto","disable_parallel_tool_use":true}),
                ),
                "tool_choice.disable_parallel_tool_use",
            ),
            (
                with("metadata", serde_json::json!({"user_id":"x".repeat(513)})),
                "metadata.user_id",
            ),
            (
                with(
                    "messages",
                    serde_json::json!([{"role":"user","content":[{"type":"document"}]}]),
                ),
                "messages[0].content[0].type",
            ),
            (
                with(
                    "messages",
                    serde_json::json!([{"role":"user","content":[{"type":"text","text":"a","citations":[]}]}]),
                ),
                "messages[0].content[0].citations",
            ),
            (
                with(
                    "messages",
                    serde_json::json!([{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"f","input":[]}]}]),
                ),
                "messages[0].content[0]",
            ),
            (
                with(
                    "messages",
                    serde_json::json!([{"role":"user","content":[{"type":"image","source":{"type":"url","url":"https://example.invalid/a.png"}}]}]),
                ),
                "messages[0].content[0]",
            ),
            (
                with(
                    "messages",
                    serde_json::json!([{"role":"tool","content":"x"}]),
                ),
                "messages[0].role",
            ),
        ];
        for (body, expected) in cases {
            assert_eq!(param(body.clone()), expected, "{body}");
        }
    }

    #[test]
    fn tool_results_split_user_turns_in_block_order() {
        let body = with(
            "messages",
            serde_json::json!([
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"t","signature":"s"},
                    {"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"a"}}]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"out"}],"is_error":false},
                    {"type":"text","text":"next"}]},
                {"role":"system","content":"reminder"}
            ]),
        );
        let chat = parse(body.to_string().as_bytes()).expect("parses");
        let roles: Vec<_> = chat.messages.iter().map(|message| message.role).collect();
        assert_eq!(
            roles,
            [
                ChatRole::Assistant,
                ChatRole::Tool,
                ChatRole::User,
                ChatRole::System
            ]
        );
        assert_eq!(
            chat.messages[0].content,
            [ChatContent::ToolCall {
                call: ToolCall {
                    id: "toolu_1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"a"}"#.into(),
                },
            }]
        );
        assert_eq!(
            chat.messages[1].content,
            [ChatContent::ToolResult {
                call_id: "toolu_1".into(),
                content: "out".into(),
            }]
        );
        assert_eq!(chat.options.stop_param, StopParam::StopSequences);
    }
}
