use crate::adapter::{
    chat_completions::request::{
        ContentPart, MessageContent, SamplingPayload, StreamOptions, ToolCallPayload,
        ToolChoicePayload, ToolPayload, encode_tool_choice, encode_tools,
    },
    invalid_request, unsupported_operation,
};
use serde::Serialize;

use crate::core::{ChatContent, ChatMessage, ChatRequest, ChatRole, GatewayError, ResponseFormat};

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
    #[serde(flatten)]
    sampling: SamplingPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
}

/// Unlike the shared message, `content` is sent as `null` when absent.
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
    Ok(ChatPayload {
        model: upstream_model.to_owned(),
        messages,
        tools: encode_tools(&chat.tools),
        tool_choice: encode_tool_choice(&chat.tool_choice),
        stream,
        stream_options: StreamOptions::for_stream(stream),
        response_format: chat
            .options
            .response_format
            .clone()
            .filter(|format| !matches!(format, ResponseFormat::Text)),
        sampling: SamplingPayload::new(&chat.options.sampling),
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
                ChatContent::InputImage { image } => Ok(ContentPart::image(image.data_url())),
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
            ChatContent::ToolCall { call } => calls.push(ToolCallPayload::new(call)),
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
