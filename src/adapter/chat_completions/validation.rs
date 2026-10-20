use crate::{
    adapter::{invalid_request, unsupported_operation},
    core::{ChatContent, ChatRequest, ChatRole, GatewayError, ToolChoice},
};

pub(crate) fn has_input_audio(chat: &ChatRequest) -> bool {
    chat.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|content| matches!(content, ChatContent::InputAudio { .. }))
    })
}

/// Declared tools, a non-default choice, or tool history.
pub(crate) fn uses_tools(chat: &ChatRequest) -> bool {
    !chat.tools.is_empty()
        || !matches!(&chat.tool_choice, ToolChoice::Auto | ToolChoice::None)
        || chat.messages.iter().any(|message| {
            message.content.iter().any(|content| {
                matches!(
                    content,
                    ChatContent::ToolCall { .. } | ChatContent::ToolResult { .. }
                )
            })
        })
}

/// Content allowed by role; user audio up to `max_audio_bytes` in total. Audio and images
/// outside `allows_audio`/`allows_images` are unsupported.
pub(crate) fn validate_messages(
    chat: &ChatRequest,
    max_audio_bytes: usize,
    allows_audio: bool,
    allows_images: bool,
) -> Result<(), GatewayError> {
    let mut total_audio_bytes = 0usize;
    for message in chat.messages_without_reasoning() {
        if message.content.is_empty() {
            return Err(invalid_request());
        }
        for content in &message.content {
            match content {
                ChatContent::Text { text } if !text.is_empty() => {}
                // Clients send empty text beside an assistant's tool calls.
                ChatContent::Text { .. } if message.role == ChatRole::Assistant => {}
                ChatContent::Text { .. } => return Err(invalid_request()),
                ChatContent::ToolCall { .. } if message.role == ChatRole::Assistant => {}
                ChatContent::Reasoning { .. } | ChatContent::ReasoningText { .. }
                    if message.role == ChatRole::Assistant => {}
                ChatContent::ToolResult { .. } if message.role == ChatRole::Tool => {}
                ChatContent::InputAudio { audio }
                    if allows_audio && message.role == ChatRole::User =>
                {
                    total_audio_bytes = total_audio_bytes
                        .checked_add(audio.bytes().len())
                        .filter(|total| *total <= max_audio_bytes)
                        .ok_or_else(invalid_request)?;
                }
                ChatContent::InputImage { .. }
                    if allows_images && message.role == ChatRole::User => {}
                ChatContent::InputAudio { .. } | ChatContent::InputImage { .. } => {
                    return Err(unsupported_operation());
                }
                ChatContent::ToolCall { .. }
                | ChatContent::ToolResult { .. }
                | ChatContent::Reasoning { .. }
                | ChatContent::ReasoningText { .. } => {
                    return Err(invalid_request());
                }
            }
        }
    }
    Ok(())
}
