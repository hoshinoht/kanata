use crate::{
    config::ValidatedRoute,
    core::{
        Capabilities, ChatContent, ChatRole, ErrorKind, GatewayError, Operation, Request,
        RouteIdentity, RoutedRequest, ToolChoice, TranscriptionRequest, TrustZone,
    },
};

pub(super) struct RouteBinding {
    identity: RouteIdentity,
    allows_input_audio: bool,
    allows_audio_function_tools: bool,
}

impl RouteBinding {
    pub(super) fn identity(&self) -> &RouteIdentity {
        &self.identity
    }
}

pub(super) fn bind_route(
    adapter_id: &str,
    route: &ValidatedRoute,
    capabilities: &Capabilities,
) -> Result<RouteBinding, GatewayError> {
    let operation = route.identity().selector.operation;
    if route.adapter_id() != adapter_id
        || !capabilities.operations.contains(&operation)
        || route.identity().route_id.trim().is_empty()
        || route.identity().selector.model_alias.0.trim().is_empty()
        || route.identity().upstream_id.trim().is_empty()
        || (route.requires_streaming_chat() && !capabilities.streaming_chat)
        || (route.requires_function_tools() && !capabilities.function_tools)
        || (route.allows_input_audio()
            && (operation != Operation::Chat || !capabilities.input_audio))
        || (route.allows_audio_streaming_chat() && !capabilities.audio_streaming_chat)
        || (route.allows_audio_function_tools()
            && !(route.allows_input_audio() && capabilities.audio_function_tools))
    {
        return Err(internal_error());
    }
    Ok(RouteBinding {
        identity: route.identity().clone(),
        allows_input_audio: route.allows_input_audio(),
        allows_audio_function_tools: route.allows_audio_function_tools(),
    })
}

pub(super) fn validate(
    routed: &RoutedRequest,
    capabilities: &Capabilities,
    route_bindings: &[RouteBinding],
    max_audio_bytes: usize,
) -> Result<(), GatewayError> {
    let request = routed.request();
    capabilities
        .check_request(request)
        .map_err(|_| unsupported_operation())?;

    let context = routed.context();
    let binding = route_bindings
        .iter()
        .find(|binding| binding.identity == context.route);
    if context.trust_zone != TrustZone::External
        || context.extensions.iter().next().is_some()
        || context.check_request(request).is_err()
    {
        return Err(invalid_request());
    }
    let Some(binding) = binding else {
        return Err(invalid_request());
    };

    match request {
        Request::Chat(chat) => {
            validate_chat(chat, binding, capabilities.function_tools, max_audio_bytes)
        }
        Request::Transcription(transcription) => {
            validate_transcription(transcription, max_audio_bytes)
        }
    }
}

fn validate_transcription(
    transcription: &TranscriptionRequest,
    max_audio_bytes: usize,
) -> Result<(), GatewayError> {
    if transcription.model.0.trim().is_empty()
        || transcription.file.bytes().is_empty()
        || transcription.file.bytes().len() > max_audio_bytes
        || transcription.extensions.iter().next().is_some()
        || super::request::transcription_format(transcription.file.media_type()).is_none()
        || transcription.language.as_deref().is_some_and(|language| {
            language.is_empty() || language.len() > 80 || language.chars().any(char::is_control)
        })
    {
        return Err(invalid_request());
    }
    // The STT endpoint ignores prompts.
    if transcription.prompt.is_some() {
        return Err(unsupported_operation());
    }
    Ok(())
}

fn validate_chat(
    chat: &crate::core::ChatRequest,
    binding: &RouteBinding,
    function_tools: bool,
    max_audio_bytes: usize,
) -> Result<(), GatewayError> {
    if chat.model.0.trim().is_empty() || chat.messages.is_empty() {
        return Err(invalid_request());
    }
    if chat.extensions.iter().next().is_some() {
        return Err(invalid_request());
    }
    let has_audio = chat.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|content| matches!(content, ChatContent::InputAudio { .. }))
    });
    let uses_tools = !chat.tools.is_empty()
        || !matches!(&chat.tool_choice, ToolChoice::Auto | ToolChoice::None)
        || chat.messages.iter().any(|message| {
            message.content.iter().any(|content| {
                matches!(
                    content,
                    ChatContent::ToolCall { .. } | ChatContent::ToolResult { .. }
                )
            })
        });
    // Tool history needs tool support even when the request declares no tools.
    if uses_tools && (!function_tools || (has_audio && !binding.allows_audio_function_tools)) {
        return Err(unsupported_operation());
    }

    let mut total_audio_bytes = 0usize;
    for message in &chat.messages {
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
                ChatContent::ToolResult { .. } if message.role == ChatRole::Tool => {}
                ChatContent::InputAudio { audio }
                    if binding.allows_input_audio && message.role == ChatRole::User =>
                {
                    total_audio_bytes = total_audio_bytes
                        .checked_add(audio.bytes().len())
                        .filter(|total| *total <= max_audio_bytes)
                        .ok_or_else(invalid_request)?;
                }
                ChatContent::InputAudio { .. } => return Err(unsupported_operation()),
                ChatContent::ToolCall { .. } | ChatContent::ToolResult { .. } => {
                    return Err(invalid_request());
                }
            }
        }
    }
    Ok(())
}

fn internal_error() -> GatewayError {
    GatewayError {
        kind: ErrorKind::Internal,
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
