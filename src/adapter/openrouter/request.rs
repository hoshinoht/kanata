use serde::Serialize;

use crate::core::{
    ChatContent, ChatMessage, ChatRequest, ChatRole, ErrorKind, GatewayError, ResponseFormat,
};

#[derive(Serialize)]
pub(super) struct ChatPayload {
    model: String,
    messages: Vec<MessagePayload>,
    stream: bool,
    provider: ProviderOptions,
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
    reasoning: Option<ReasoningOptions>,
}

#[derive(Serialize)]
struct ReasoningOptions {
    effort: &'static str,
}

#[derive(Serialize)]
struct MessagePayload {
    role: &'static str,
    content: String,
}

#[derive(Serialize)]
struct ProviderOptions {
    allow_fallbacks: bool,
}

pub(super) fn encode(
    chat: &ChatRequest,
    upstream_model: &str,
    stream: bool,
) -> Result<ChatPayload, GatewayError> {
    if upstream_model.trim().is_empty() {
        return Err(invalid_request());
    }
    let messages = chat
        .messages
        .iter()
        .map(encode_message)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ChatPayload {
        model: upstream_model.to_owned(),
        messages,
        stream,
        provider: ProviderOptions {
            allow_fallbacks: false,
        },
        response_format: chat
            .options
            .response_format
            .clone()
            .filter(|format| !matches!(format, ResponseFormat::Text)),
        temperature: chat.options.sampling.temperature.map(|value| value.get()),
        top_p: chat.options.sampling.top_p.map(|value| value.get()),
        seed: chat.options.sampling.seed,
        max_tokens: chat.options.max_output_tokens,
        reasoning: chat
            .options
            .reasoning_effort
            .map(|effort| ReasoningOptions {
                effort: effort.as_str(),
            }),
    })
}

fn encode_message(message: &ChatMessage) -> Result<MessagePayload, GatewayError> {
    let role = match message.role {
        ChatRole::System => "system",
        ChatRole::Developer => "developer",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => return Err(unsupported_operation()),
    };

    let mut content = String::new();
    for segment in &message.content {
        let ChatContent::Text { text } = segment else {
            return Err(unsupported_operation());
        };
        if text.is_empty() {
            return Err(invalid_request());
        }
        content.push_str(text);
    }
    if content.is_empty() {
        return Err(invalid_request());
    }

    Ok(MessagePayload { role, content })
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
