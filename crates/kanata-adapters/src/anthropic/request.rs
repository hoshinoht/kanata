use crate::{internal_error, invalid_request, unsupported_operation};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::core::{
    ChatContent, ChatMessage, ChatRequest, ChatRole, FunctionTool, GatewayError, ReasoningEffort,
    ResponseFormat, ToolChoice,
};

use super::response::{REDACTED_THINKING_PREFIX, THINKING_PREFIX};

#[derive(Serialize)]
pub(super) struct ChatPayload<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<Block<'a>>,
    messages: Vec<MessagePayload<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolPayload<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoicePayload<'a>>,
    #[serde(skip_serializing_if = "OutputConfig::is_empty")]
    output_config: OutputConfig,
    /// Automatic prompt caching of the longest reusable prefix.
    cache_control: CacheControl,
    stream: bool,
}

#[derive(Serialize)]
struct MessagePayload<'a> {
    role: &'static str,
    content: Vec<Block<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block<'a> {
    Text {
        text: &'a str,
    },
    Image {
        source: ImageSource<'a>,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: Map<String, Value>,
    },
    ToolResult {
        tool_use_id: &'a str,
        #[serde(skip_serializing_if = "is_empty")]
        content: &'a str,
    },
    /// Omitted-display thinking: the signature carries the reasoning.
    Thinking {
        thinking: &'static str,
        signature: &'a str,
    },
    RedactedThinking {
        data: &'a str,
    },
}

#[derive(Serialize)]
struct ImageSource<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    media_type: &'a str,
    data: &'a str,
}

#[derive(Serialize)]
struct ToolPayload<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    input_schema: &'a Value,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ToolChoicePayload<'a> {
    None,
    Auto,
    Any,
    Tool { name: &'a str },
}

#[derive(Default, Serialize)]
struct OutputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<OutputFormat>,
}

impl OutputConfig {
    fn is_empty(&self) -> bool {
        self.effort.is_none() && self.format.is_none()
    }
}

#[derive(Serialize)]
struct OutputFormat {
    #[serde(rename = "type")]
    kind: &'static str,
    schema: Value,
}

#[derive(Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

fn is_empty(value: &&str) -> bool {
    value.is_empty()
}

pub(super) fn encode<'a>(
    chat: &'a ChatRequest,
    upstream_model: &'a str,
    max_tokens: u32,
) -> Result<ChatPayload<'a>, GatewayError> {
    if upstream_model.trim().is_empty() {
        return Err(invalid_request());
    }
    let mut system = Vec::new();
    let mut messages: Vec<MessagePayload<'a>> = Vec::new();
    for message in chat.messages_without_reasoning() {
        match message.role {
            // The Messages API has one top-level system prompt, so instructions after the
            // conversation started cannot keep their position.
            ChatRole::System | ChatRole::Developer => {
                if !messages.is_empty() {
                    return Err(invalid_request());
                }
                for content in &message.content {
                    match content {
                        ChatContent::Text { text } if !text.is_empty() => {
                            system.push(Block::Text { text });
                        }
                        _ => return Err(invalid_request()),
                    }
                }
            }
            role => {
                let (role, content) = match role {
                    ChatRole::Assistant => ("assistant", encode_assistant(message)?),
                    ChatRole::Tool => ("user", encode_tool_result(message)?),
                    _ => ("user", encode_user(message)?),
                };
                // An assistant turn holding only empty text adds nothing.
                if content.is_empty() {
                    continue;
                }
                match messages.last_mut() {
                    Some(last) if last.role == role => last.content.extend(content),
                    _ => messages.push(MessagePayload { role, content }),
                }
            }
        }
    }
    if messages.is_empty() {
        return Err(invalid_request());
    }

    let tools: Vec<_> = chat
        .tools
        .iter()
        .map(encode_tool)
        .collect::<Result<_, _>>()?;
    let tool_choice = if tools.is_empty() {
        match &chat.tool_choice {
            ToolChoice::None | ToolChoice::Auto => None,
            ToolChoice::Required | ToolChoice::Function { .. } => return Err(invalid_request()),
        }
    } else {
        Some(match &chat.tool_choice {
            ToolChoice::None => ToolChoicePayload::None,
            ToolChoice::Auto => ToolChoicePayload::Auto,
            ToolChoice::Required => ToolChoicePayload::Any,
            ToolChoice::Function { name } => {
                if !tools.iter().any(|tool| tool.name == name.as_str()) {
                    return Err(invalid_request());
                }
                ToolChoicePayload::Tool {
                    name: name.as_str(),
                }
            }
        })
    };

    Ok(ChatPayload {
        model: upstream_model,
        max_tokens,
        system,
        messages,
        tools,
        tool_choice,
        output_config: OutputConfig {
            effort: chat
                .options
                .reasoning_effort
                .map(encode_effort)
                .transpose()?,
            format: encode_format(chat.options.response_format.as_ref())?,
        },
        cache_control: CacheControl { kind: "ephemeral" },
        stream: chat.stream,
    })
}

fn encode_user(message: &ChatMessage) -> Result<Vec<Block<'_>>, GatewayError> {
    message
        .content
        .iter()
        .map(|content| match content {
            ChatContent::Text { text } if !text.is_empty() => Ok(Block::Text { text }),
            ChatContent::InputImage { image } => {
                let (media_type, data) = image
                    .data_url()
                    .strip_prefix("data:")
                    .and_then(|rest| rest.split_once(";base64,"))
                    .ok_or_else(invalid_request)?;
                Ok(Block::Image {
                    source: ImageSource {
                        kind: "base64",
                        media_type,
                        data,
                    },
                })
            }
            ChatContent::InputAudio { .. } => Err(unsupported_operation()),
            _ => Err(invalid_request()),
        })
        .collect()
}

/// Replayed thinking first, then text and tool calls. Reasoning items minted by other
/// providers (for example `rs_…`) and plaintext reasoning are skipped.
fn encode_assistant(message: &ChatMessage) -> Result<Vec<Block<'_>>, GatewayError> {
    let mut thinking = Vec::new();
    let mut output = Vec::new();
    for content in &message.content {
        match content {
            ChatContent::Text { text } if text.is_empty() => {}
            ChatContent::Text { text } => output.push(Block::Text { text }),
            ChatContent::ToolCall { call } => {
                let Ok(Value::Object(input)) = serde_json::from_str(&call.arguments) else {
                    return Err(invalid_request());
                };
                output.push(Block::ToolUse {
                    id: &call.id,
                    name: &call.name,
                    input,
                });
            }
            ChatContent::Reasoning { item } => {
                let id = item.id().unwrap_or_default();
                if id.starts_with(THINKING_PREFIX) {
                    thinking.push(Block::Thinking {
                        thinking: "",
                        signature: item.encrypted_content(),
                    });
                } else if id.starts_with(REDACTED_THINKING_PREFIX) {
                    thinking.push(Block::RedactedThinking {
                        data: item.encrypted_content(),
                    });
                }
            }
            ChatContent::ReasoningText { .. } => {}
            ChatContent::ToolResult { .. }
            | ChatContent::InputAudio { .. }
            | ChatContent::InputImage { .. } => return Err(invalid_request()),
        }
    }
    if output.is_empty() {
        return Ok(output);
    }
    thinking.extend(output);
    Ok(thinking)
}

fn encode_tool_result(message: &ChatMessage) -> Result<Vec<Block<'_>>, GatewayError> {
    let [ChatContent::ToolResult { call_id, content }] = message.content.as_slice() else {
        return Err(invalid_request());
    };
    Ok(vec![Block::ToolResult {
        tool_use_id: call_id,
        content,
    }])
}

fn encode_tool(tool: &FunctionTool) -> Result<ToolPayload<'_>, GatewayError> {
    if !tool.parameters.is_object() {
        return Err(invalid_request());
    }
    Ok(ToolPayload {
        name: &tool.name,
        description: tool.description.as_deref(),
        input_schema: &tool.parameters,
    })
}

/// `output_config.effort` levels; the API has no off or minimal level.
fn encode_effort(effort: ReasoningEffort) -> Result<&'static str, GatewayError> {
    match effort {
        ReasoningEffort::None | ReasoningEffort::Minimal => Err(invalid_request()),
        ReasoningEffort::Low
        | ReasoningEffort::Medium
        | ReasoningEffort::High
        | ReasoningEffort::Xhigh
        | ReasoningEffort::Max => Ok(effort.as_str()),
    }
}

/// Only schema-constrained JSON exists upstream; free-form `json_object` is rejected, as on
/// Apple Foundation Models.
fn encode_format(format: Option<&ResponseFormat>) -> Result<Option<OutputFormat>, GatewayError> {
    match format {
        None | Some(ResponseFormat::Text) => Ok(None),
        Some(ResponseFormat::JsonObject) => Err(invalid_request()),
        Some(ResponseFormat::JsonSchema { json_schema }) => {
            let schema = serde_json::to_value(json_schema)
                .ok()
                .and_then(|mut value| value.get_mut("schema").map(Value::take))
                .ok_or_else(internal_error)?;
            Ok(Some(OutputFormat {
                kind: "json_schema",
                schema,
            }))
        }
    }
}
