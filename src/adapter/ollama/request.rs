use serde::Serialize;
use serde_json::Value;

use crate::core::{
    ChatContent, ChatMessage, ChatRequest, ChatRole, FunctionTool, ResponseFormat, ToolChoice,
};

use super::super::super::core::{ErrorKind, GatewayError};

#[derive(Serialize)]
pub(super) struct ChatPayload {
    model: String,
    messages: Vec<MessagePayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolPayload>>,
    tool_choice: ToolChoicePayload,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct MessagePayload {
    role: &'static str,
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallPayload>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct ToolPayload {
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionPayload,
}

#[derive(Serialize)]
struct FunctionPayload {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: Value,
}

#[derive(Serialize)]
struct ToolCallPayload {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: CallFunctionPayload,
}

#[derive(Serialize)]
struct CallFunctionPayload {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ToolChoicePayload {
    Simple(&'static str),
    Named {
        #[serde(rename = "type")]
        kind: &'static str,
        function: NamedFunctionPayload,
    },
}

#[derive(Serialize)]
struct NamedFunctionPayload {
    name: String,
}

pub(super) fn encode(
    chat: &ChatRequest,
    upstream_model: &str,
    stream: bool,
) -> Result<ChatPayload, GatewayError> {
    if upstream_model.is_empty() {
        return Err(invalid_request());
    }
    let messages = chat
        .messages
        .iter()
        .map(encode_message)
        .collect::<Result<Vec<_>, _>>()?;
    let tools = (!chat.tools.is_empty()).then(|| {
        chat.tools
            .iter()
            .map(encode_tool)
            .collect::<Result<Vec<_>, _>>()
    });
    let tools = match tools {
        Some(tools) => Some(tools?),
        None => None,
    };
    Ok(ChatPayload {
        model: upstream_model.to_owned(),
        messages,
        tools,
        tool_choice: encode_tool_choice(&chat.tool_choice),
        stream,
        stream_options: stream.then_some(StreamOptions {
            include_usage: true,
        }),
        response_format: chat
            .options
            .response_format
            .clone()
            .filter(|format| !matches!(format, ResponseFormat::Text)),
        temperature: chat.options.sampling.temperature.map(|value| value.get()),
        top_p: chat.options.sampling.top_p.map(|value| value.get()),
        seed: chat.options.sampling.seed,
        max_tokens: chat.options.max_output_tokens,
        reasoning_effort: chat.options.reasoning_effort.map(|effort| effort.as_str()),
    })
}

fn encode_message(message: &ChatMessage) -> Result<MessagePayload, GatewayError> {
    let role = match message.role {
        ChatRole::System => "system",
        ChatRole::Developer => "developer",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
    };
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut result = None;
    for content in &message.content {
        match content {
            ChatContent::Text { text: value } => text.push_str(value),
            ChatContent::ToolCall { call } => calls.push(ToolCallPayload {
                id: call.id.clone(),
                kind: "function",
                function: CallFunctionPayload {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                },
            }),
            ChatContent::ToolResult { call_id, content } => {
                if result.replace((call_id.clone(), content.clone())).is_some() {
                    return Err(invalid_request());
                }
            }
            ChatContent::InputAudio { .. } => return Err(unsupported_operation()),
        }
    }
    let (content, tool_calls, tool_call_id) = match message.role {
        ChatRole::Assistant => (
            (!text.is_empty()).then_some(text),
            (!calls.is_empty()).then_some(calls),
            None,
        ),
        ChatRole::Tool => {
            if !calls.is_empty() {
                return Err(invalid_request());
            }
            let Some((call_id, content)) = result else {
                return Err(invalid_request());
            };
            (Some(content), None, Some(call_id))
        }
        ChatRole::System | ChatRole::Developer | ChatRole::User => {
            if !calls.is_empty() || result.is_some() {
                return Err(invalid_request());
            }
            (Some(text), None, None)
        }
    };
    Ok(MessagePayload {
        role,
        content,
        tool_calls,
        tool_call_id,
    })
}

fn encode_tool(tool: &FunctionTool) -> Result<ToolPayload, GatewayError> {
    Ok(ToolPayload {
        kind: "function",
        function: FunctionPayload {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
        },
    })
}

fn encode_tool_choice(choice: &ToolChoice) -> ToolChoicePayload {
    match choice {
        ToolChoice::None => ToolChoicePayload::Simple("none"),
        ToolChoice::Auto => ToolChoicePayload::Simple("auto"),
        ToolChoice::Required => ToolChoicePayload::Simple("required"),
        ToolChoice::Function { name } => ToolChoicePayload::Named {
            kind: "function",
            function: NamedFunctionPayload { name: name.clone() },
        },
    }
}

fn invalid_request() -> GatewayError {
    GatewayError {
        kind: ErrorKind::InvalidRequest,
    }
}

fn unsupported_operation() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UnsupportedOperation,
    }
}
