use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::config::CodexReasoningEffort;
use crate::core::{
    ChatContent, ChatMessage, ChatRequest, ChatRole, ErrorKind, GatewayError, Request,
    RoutedRequest, ToolChoice, TrustZone,
};

#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod tests;

mod stream;

#[allow(unused_imports)]
pub(crate) use stream::{ResponsesStreamError, ResponsesStreamParser};

const MAX_TOOL_CALL_ID_BYTES: usize = 128;
const MAX_TOOL_NAME_BYTES: usize = 64;

pub(super) fn to_responses_request(
    routed: &RoutedRequest,
    reasoning_effort: CodexReasoningEffort,
) -> Result<Value, GatewayError> {
    let context = routed.context();
    let request = routed.request();
    if context.trust_zone != TrustZone::External {
        return Err(unsupported_operation());
    }
    let Request::Chat(chat) = request else {
        return Err(unsupported_operation());
    };
    if context.check_request(request).is_err()
        || context.route.route_id.is_empty()
        || context.route.upstream_id.trim().is_empty()
        || !context.extensions.iter().next().is_none()
    {
        return Err(invalid_request());
    }

    validate_chat(chat)?;

    let (input, instructions) = map_messages(chat);
    let mut payload = json!({
        "model": context.route.upstream_id,
        "input": input,
        "reasoning": { "effort": reasoning_effort.as_str() },
        "store": false,
        "stream": true,
    });
    if !instructions.is_empty() {
        payload["instructions"] = json!(instructions.join("\n\n"));
    }
    if !chat.tools.is_empty() {
        payload["tools"] = json!(
            chat.tools
                .iter()
                .map(|tool| {
                    let mut declaration = json!({
                        "type": "function",
                        "name": tool.name,
                        "parameters": tool.parameters,
                    });
                    if let Some(description) = &tool.description {
                        declaration["description"] = json!(description);
                    }
                    declaration
                })
                .collect::<Vec<_>>()
        );
    }
    if !matches!(chat.tool_choice, ToolChoice::Auto) || !chat.tools.is_empty() {
        payload["tool_choice"] = tool_choice_value(&chat.tool_choice);
    }
    Ok(payload)
}

fn validate_chat(chat: &ChatRequest) -> Result<(), GatewayError> {
    if chat.model.0.trim().is_empty()
        || chat.messages.is_empty()
        || !chat.extensions.iter().next().is_none()
    {
        return Err(invalid_request());
    }

    let mut tool_names = BTreeSet::new();
    for tool in &chat.tools {
        if !valid_tool_name(&tool.name)
            || !tool_names.insert(tool.name.as_str())
            || !tool.parameters.is_object()
        {
            return Err(invalid_request());
        }
    }
    match &chat.tool_choice {
        ToolChoice::Required if chat.tools.is_empty() => return Err(invalid_request()),
        ToolChoice::Function { name }
            if !valid_tool_name(name) || !tool_names.contains(name.as_str()) =>
        {
            return Err(invalid_request());
        }
        _ => {}
    }

    let mut call_ids = BTreeSet::new();
    let mut outstanding_call_ids = BTreeSet::new();
    for message in &chat.messages {
        if message.content.is_empty() {
            return Err(invalid_request());
        }
        let mut text_count = 0;
        let mut has_nonempty_text = false;
        let mut calls = 0;
        let mut results = 0;
        for content in &message.content {
            match content {
                ChatContent::Text { text } => {
                    text_count += 1;
                    has_nonempty_text |= !text.is_empty();
                }
                ChatContent::ToolCall { call } => {
                    calls += 1;
                    if !valid_call_id(&call.id)
                        || !call_ids.insert(call.id.as_str())
                        || !valid_tool_name(&call.name)
                    {
                        return Err(invalid_request());
                    }
                    outstanding_call_ids.insert(call.id.as_str());
                }
                ChatContent::ToolResult { call_id, content } => {
                    results += 1;
                    if !valid_call_id(call_id)
                        || content.is_empty()
                        || !outstanding_call_ids.remove(call_id.as_str())
                    {
                        return Err(invalid_request());
                    }
                }
                ChatContent::InputAudio { .. } => return Err(unsupported_operation()),
            }
        }

        match message.role {
            ChatRole::System | ChatRole::Developer
                if calls != 0 || results != 0 || !has_nonempty_text =>
            {
                return Err(invalid_request());
            }
            ChatRole::User if calls != 0 || results != 0 || !has_nonempty_text => {
                return Err(invalid_request());
            }
            ChatRole::Assistant if results != 0 || (!has_nonempty_text && calls == 0) => {
                return Err(invalid_request());
            }
            ChatRole::Tool if calls != 0 || results != 1 || text_count != 0 => {
                return Err(invalid_request());
            }
            _ => {}
        }
    }
    Ok(())
}

fn map_messages(chat: &ChatRequest) -> (Vec<Value>, Vec<String>) {
    let mut input = Vec::new();
    let mut instructions = Vec::new();
    for message in &chat.messages {
        match message.role {
            ChatRole::System | ChatRole::Developer => instructions.push(text_content(message)),
            ChatRole::User => input.push(json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": text_content(message) }],
            })),
            ChatRole::Assistant => {
                let mut pending_text = String::new();
                for content in &message.content {
                    match content {
                        ChatContent::Text { text } => pending_text.push_str(text),
                        ChatContent::ToolCall { call } => {
                            push_assistant_text(&mut input, &mut pending_text);
                            input.push(json!({
                                "type": "function_call",
                                "call_id": call.id,
                                "name": call.name,
                                "arguments": call.arguments,
                            }));
                        }
                        ChatContent::ToolResult { .. } | ChatContent::InputAudio { .. } => {
                            unreachable!("validated before mapping")
                        }
                    }
                }
                push_assistant_text(&mut input, &mut pending_text);
            }
            ChatRole::Tool => {
                let ChatContent::ToolResult { call_id, content } = &message.content[0] else {
                    unreachable!("validated tool result")
                };
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": content,
                }));
            }
        }
    }
    (input, instructions)
}

fn text_content(message: &ChatMessage) -> String {
    let mut text = String::new();
    for content in &message.content {
        if let ChatContent::Text { text: segment } = content {
            text.push_str(segment);
        }
    }
    text
}

fn push_assistant_text(input: &mut Vec<Value>, text: &mut String) {
    if !text.is_empty() {
        input.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }],
        }));
        text.clear();
    }
}

fn tool_choice_value(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::None => json!("none"),
        ToolChoice::Auto => json!("auto"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Function { name } => json!({ "type": "function", "name": name }),
    }
}

fn valid_call_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOOL_CALL_ID_BYTES
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn valid_tool_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOOL_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
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
