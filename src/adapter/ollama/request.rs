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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
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
    content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallPayload>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    // Replayed assistant reasoning; Ollama's OpenAI layer maps it to the message's thinking.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Serialize)]
struct ImageUrl {
    url: String,
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

/// `apple_fm` selects the `fm serve` dialect: it ignores `max_tokens`, rejects `json_object`,
/// and is not known to honour stop sequences or penalties. `replay_reasoning` forwards
/// assistant reasoning text; otherwise it is dropped.
pub(super) fn encode(
    chat: &ChatRequest,
    upstream_model: &str,
    stream: bool,
    apple_fm: bool,
    replay_reasoning: bool,
) -> Result<ChatPayload, GatewayError> {
    if upstream_model.is_empty() {
        return Err(invalid_request());
    }
    if apple_fm
        && (matches!(
            chat.options.response_format,
            Some(ResponseFormat::JsonObject)
        ) || !chat.options.sampling.stop.is_empty()
            || chat.options.sampling.presence_penalty.is_some()
            || chat.options.sampling.frequency_penalty.is_some())
    {
        return Err(invalid_request());
    }
    let messages = chat
        .messages_without_reasoning()
        .map(|message| encode_message(message, replay_reasoning))
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
        stop: chat.options.sampling.stop.clone(),
        presence_penalty: chat
            .options
            .sampling
            .presence_penalty
            .map(|value| value.get()),
        frequency_penalty: chat
            .options
            .sampling
            .frequency_penalty
            .map(|value| value.get()),
        max_tokens: chat.options.max_output_tokens.filter(|_| !apple_fm),
        max_completion_tokens: chat.options.max_output_tokens.filter(|_| apple_fm),
        reasoning_effort: chat.options.reasoning_effort.map(|effort| effort.as_str()),
    })
}

fn encode_message(
    message: &ChatMessage,
    replay_reasoning: bool,
) -> Result<MessagePayload, GatewayError> {
    let role = match message.role {
        ChatRole::System => "system",
        ChatRole::Developer => "developer",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
    };
    if message
        .content
        .iter()
        .any(|part| matches!(part, ChatContent::InputImage { .. }))
    {
        if message.role != ChatRole::User {
            return Err(unsupported_operation());
        }
        let parts = message
            .content
            .iter()
            .map(|part| match part {
                ChatContent::Text { text } => Ok(ContentPart::Text { text: text.clone() }),
                ChatContent::InputImage { image } => Ok(ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: image.data_url().to_owned(),
                    },
                }),
                _ => Err(unsupported_operation()),
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(MessagePayload {
            role,
            content: Some(MessageContent::Parts(parts)),
            tool_calls: None,
            tool_call_id: None,
            reasoning: None,
        });
    }
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut result = None;
    let mut reasoning = String::new();
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
            ChatContent::InputAudio { .. } | ChatContent::InputImage { .. } => {
                return Err(unsupported_operation());
            }
            ChatContent::ReasoningText { text: value }
                if replay_reasoning && message.role == ChatRole::Assistant =>
            {
                reasoning.push_str(value);
            }
            // Encrypted reasoning never replays here; reasoning text only where accepted.
            ChatContent::Reasoning { .. } | ChatContent::ReasoningText { .. } => {}
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
        content: content.map(MessageContent::Text),
        tool_calls,
        tool_call_id,
        reasoning: (!reasoning.is_empty()).then_some(reasoning),
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

#[cfg(test)]
mod tests {
    use crate::core::{
        ChatContent, ChatMessage, ChatRequest, ChatRole, ErrorKind, ModelAlias, Penalty, ToolChoice,
    };

    #[test]
    fn stop_and_penalties_reach_ollama_but_not_apple_fm() {
        let mut chat = ChatRequest {
            model: ModelAlias("local-chat".into()),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: vec![ChatContent::Text { text: "hi".into() }],
            }],
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            stream: false,
            options: Default::default(),
            prompt_cache_key: None,
            include_encrypted_reasoning: false,
            extensions: Default::default(),
        };
        chat.options.sampling.stop = vec!["END".into()];
        chat.options.sampling.presence_penalty = Some(Penalty::new(0.5).expect("penalty"));
        chat.options.sampling.frequency_penalty = Some(Penalty::new(1.0).expect("penalty"));
        let payload =
            serde_json::to_value(super::encode(&chat, "llama3", false, false, true).unwrap())
                .expect("serializes");
        assert_eq!(payload["stop"], serde_json::json!(["END"]));
        assert_eq!(payload["presence_penalty"], 0.5);
        assert_eq!(payload["frequency_penalty"], 1.0);
        assert_eq!(
            super::encode(&chat, "llama3", false, true, false)
                .err()
                .map(|error| error.kind),
            Some(ErrorKind::InvalidRequest)
        );
    }

    #[test]
    fn assistant_reasoning_text_is_replayed_only_when_enabled() {
        let chat = ChatRequest {
            model: ModelAlias("local-chat".into()),
            messages: vec![
                ChatMessage {
                    role: ChatRole::User,
                    content: vec![ChatContent::Text { text: "hi".into() }],
                },
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: vec![
                        ChatContent::ReasoningText {
                            text: "greet back".into(),
                        },
                        ChatContent::Text {
                            text: "hello".into(),
                        },
                    ],
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: vec![ChatContent::Text {
                        text: "again".into(),
                    }],
                },
            ],
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            stream: false,
            options: Default::default(),
            prompt_cache_key: None,
            include_encrypted_reasoning: false,
            extensions: Default::default(),
        };
        let encode = |replay| {
            serde_json::to_value(super::encode(&chat, "gpt-oss:20b", false, false, replay).unwrap())
                .expect("serializes")
        };
        assert_eq!(
            encode(true)["messages"][1],
            serde_json::json!({"role": "assistant", "content": "hello", "reasoning": "greet back"})
        );
        assert_eq!(
            encode(false)["messages"][1],
            serde_json::json!({"role": "assistant", "content": "hello"})
        );
    }
}
