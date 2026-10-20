use crate::adapter::{invalid_request, unsupported_operation};
use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::config::{CodexReasoningEffort, CodexReasoningSummary, ProviderKind};
use crate::core::{
    ChatContent, ChatMessage, ChatRequest, ChatRole, GatewayError, ReasoningItem, Request,
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
    reasoning_summary: Option<CodexReasoningSummary>,
) -> Result<Value, GatewayError> {
    let mut payload = to_text_request(routed, ProviderKind::Codex)?;
    payload["reasoning"] = json!({ "effort": reasoning_effort.as_str() });
    if let Some(summary) = reasoning_summary {
        payload["reasoning"]["summary"] = json!(summary.as_str());
    }
    Ok(payload)
}

/// Whether the upstream is asked for encrypted reasoning, so its parser should keep it.
pub(crate) fn requests_encrypted_reasoning(chat: &ChatRequest, kind: ProviderKind) -> bool {
    chat.include_encrypted_reasoning && kind.replays_encrypted_reasoning()
}

/// `kind` decides whether the cache key and encrypted reasoning reach the upstream.
pub(crate) fn to_text_request(
    routed: &RoutedRequest,
    kind: ProviderKind,
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

    let replay = kind.replays_encrypted_reasoning();
    let (input, instructions) = map_messages(chat, replay);
    let mut payload = json!({
        "model": context.route.upstream_id,
        "input": input,
        "store": false,
        "stream": true,
    });
    if let Some(key) = chat
        .prompt_cache_key
        .as_ref()
        .filter(|_| kind.forwards_prompt_cache_key())
    {
        payload["prompt_cache_key"] = json!(key.as_str());
    }
    if requests_encrypted_reasoning(chat, kind) {
        payload["include"] = json!(["reasoning.encrypted_content"]);
    }
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
        let mut reasoning = 0;
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
                ChatContent::ToolResult { call_id, .. } => {
                    results += 1;
                    // Empty tool output is legal on the Responses wire.
                    if !valid_call_id(call_id) || !outstanding_call_ids.remove(call_id.as_str()) {
                        return Err(invalid_request());
                    }
                }
                ChatContent::InputAudio { .. } | ChatContent::InputImage { .. } => {
                    return Err(unsupported_operation());
                }
                ChatContent::Reasoning { .. } => reasoning += 1,
                // Codex replays only encrypted reasoning; plaintext replay is dropped.
                ChatContent::ReasoningText { .. } => {}
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
            ChatRole::Assistant
                if results != 0 || (!has_nonempty_text && calls == 0 && reasoning == 0) =>
            {
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

/// `replay` keeps encrypted reasoning as `reasoning` input items in transcript order;
/// otherwise it is skipped like summary-only reasoning.
fn map_messages(chat: &ChatRequest, replay: bool) -> (Vec<Value>, Vec<String>) {
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
                        ChatContent::ToolResult { .. }
                        | ChatContent::InputAudio { .. }
                        | ChatContent::InputImage { .. } => {
                            unreachable!("validated before mapping")
                        }
                        ChatContent::Reasoning { item } => {
                            if replay {
                                push_assistant_text(&mut input, &mut pending_text);
                                input.push(reasoning_input(item));
                            }
                        }
                        ChatContent::ReasoningText { .. } => {}
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

fn reasoning_input(item: &ReasoningItem) -> Value {
    let mut value = json!({
        "type": "reasoning",
        "summary": [],
        "encrypted_content": item.encrypted_content(),
    });
    if let Some(id) = item.id() {
        value["id"] = json!(id);
    }
    value
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
