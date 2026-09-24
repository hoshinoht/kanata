use std::collections::BTreeSet;

use crate::core::{
    Capabilities, ChatContent, ChatRole, ErrorKind, GatewayError, Request, RoutedRequest,
    ToolChoice, TrustZone,
};

const MAX_TOOL_CALL_ID_BYTES: usize = 128;
const MAX_TOOL_NAME_BYTES: usize = 64;

pub(super) fn validate(
    routed: &RoutedRequest,
    capabilities: &Capabilities,
    trust_zone: TrustZone,
) -> Result<(), GatewayError> {
    let request = routed.request();
    capabilities
        .check_request(request)
        .map_err(|_| unsupported_operation())?;
    if routed.context().check_request(request).is_err()
        || routed.context().trust_zone != trust_zone
        || routed.context().route.route_id.is_empty()
        || routed.context().route.upstream_id.trim().is_empty()
        || !routed.context().extensions.iter().next().is_none()
    {
        return Err(invalid_request());
    }
    match request {
        Request::Chat(chat) => {
            if !capabilities.function_tools
                && chat.messages.iter().any(|message| {
                    message.content.iter().any(|content| {
                        matches!(
                            content,
                            ChatContent::ToolCall { .. } | ChatContent::ToolResult { .. }
                        )
                    })
                })
            {
                return Err(unsupported_operation());
            }
            validate_chat(chat)
        }
        Request::Transcription(_) => Err(unsupported_operation()),
    }
}

fn validate_chat(chat: &crate::core::ChatRequest) -> Result<(), GatewayError> {
    if chat.model.0.is_empty()
        || chat.messages.is_empty()
        || !chat.extensions.iter().next().is_none()
    {
        return Err(invalid_request());
    }

    let mut tool_names = BTreeSet::new();
    for tool in &chat.tools {
        if !valid_tool_name(&tool.name)
            || !tool_names.insert(tool.name.clone())
            || !tool.parameters.is_object()
        {
            return Err(invalid_request());
        }
    }
    match &chat.tool_choice {
        ToolChoice::Required if chat.tools.is_empty() => return Err(invalid_request()),
        ToolChoice::Function { name } if !valid_tool_name(name) || !tool_names.contains(name) => {
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
        let mut text = false;
        let mut calls = 0;
        let mut result = 0;
        for content in &message.content {
            match content {
                ChatContent::Text { text: value } => text |= !value.is_empty(),
                ChatContent::ToolCall { call } => {
                    calls += 1;
                    if !valid_call_id(&call.id)
                        || !call_ids.insert(call.id.clone())
                        || !valid_tool_name(&call.name)
                    {
                        return Err(invalid_request());
                    }
                    outstanding_call_ids.insert(call.id.clone());
                }
                ChatContent::ToolResult { call_id, content } => {
                    result += 1;
                    if !valid_call_id(call_id)
                        || content.is_empty()
                        || !outstanding_call_ids.remove(call_id)
                    {
                        return Err(invalid_request());
                    }
                }
                ChatContent::InputAudio { .. } => return Err(unsupported_operation()),
            }
        }
        match message.role {
            ChatRole::Assistant if calls == 0 && !text => return Err(invalid_request()),
            ChatRole::Assistant if result != 0 => return Err(invalid_request()),
            ChatRole::Tool if calls != 0 || result != 1 || text => return Err(invalid_request()),
            ChatRole::System | ChatRole::Developer | ChatRole::User
                if calls != 0 || result != 0 || !text =>
            {
                return Err(invalid_request());
            }
            _ => {}
        }
    }
    Ok(())
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
