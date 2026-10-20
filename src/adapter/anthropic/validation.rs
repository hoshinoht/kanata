use crate::adapter::{internal_error, invalid_request, unsupported_operation};
use crate::{
    config::ValidatedRoute,
    core::{
        Capabilities, ChatContent, ChatRequest, ChatRole, GatewayError, Operation, Request,
        RouteIdentity, RoutedRequest, ToolChoice, TrustZone,
    },
};

pub(super) struct RouteBinding {
    identity: RouteIdentity,
    allows_input_images: bool,
}

pub(super) fn bind_route(
    adapter_id: &str,
    route: &ValidatedRoute,
    capabilities: &Capabilities,
) -> Result<RouteBinding, GatewayError> {
    if route.adapter_id() != adapter_id
        || route.identity().selector.operation != Operation::Chat
        || route.identity().route_id.trim().is_empty()
        || route.identity().selector.model_alias.0.trim().is_empty()
        || route.identity().upstream_id.trim().is_empty()
        || (route.requires_streaming_chat() && !capabilities.streaming_chat)
        || (route.requires_function_tools() && !capabilities.function_tools)
        || (route.allows_input_images() && !capabilities.input_images)
        || route.allows_input_audio()
    {
        return Err(internal_error());
    }
    Ok(RouteBinding {
        identity: route.identity().clone(),
        allows_input_images: route.allows_input_images(),
    })
}

/// Rejects before dispatch: capability, route binding, roles and option values.
pub(super) fn validate(
    routed: &RoutedRequest,
    capabilities: &Capabilities,
    route_bindings: &[RouteBinding],
) -> Result<(), GatewayError> {
    let request = routed.request();
    capabilities
        .check_request(request)
        .map_err(|_| unsupported_operation())?;
    let context = routed.context();
    if context.trust_zone != TrustZone::External
        || context.extensions.iter().next().is_some()
        || context.check_request(request).is_err()
    {
        return Err(invalid_request());
    }
    let Some(binding) = route_bindings
        .iter()
        .find(|binding| binding.identity == context.route)
    else {
        return Err(invalid_request());
    };
    match request {
        Request::Chat(chat) => validate_chat(chat, binding, capabilities.function_tools),
        Request::Transcription(_) | Request::Embeddings(_) | Request::Speech(_) => {
            Err(unsupported_operation())
        }
    }
}

fn validate_chat(
    chat: &ChatRequest,
    binding: &RouteBinding,
    function_tools: bool,
) -> Result<(), GatewayError> {
    if chat.model.0.trim().is_empty()
        || chat.messages.is_empty()
        || chat.extensions.iter().next().is_some()
        || chat.options.enable_thinking.is_some()
    {
        return Err(invalid_request());
    }
    let uses_tools = !chat.tools.is_empty()
        || !matches!(chat.tool_choice, ToolChoice::Auto | ToolChoice::None)
        || chat.messages.iter().any(|message| {
            message.content.iter().any(|content| {
                matches!(
                    content,
                    ChatContent::ToolCall { .. } | ChatContent::ToolResult { .. }
                )
            })
        });
    // Tool history needs tool support even when the request declares no tools.
    if uses_tools && !function_tools {
        return Err(unsupported_operation());
    }
    for message in &chat.messages {
        if message.content.is_empty() {
            return Err(invalid_request());
        }
        for content in &message.content {
            match (content, message.role) {
                (ChatContent::Text { text }, _) if !text.is_empty() => {}
                // Clients send empty text beside an assistant's tool calls.
                (ChatContent::Text { .. }, ChatRole::Assistant) => {}
                (
                    ChatContent::ToolCall { .. }
                    | ChatContent::Reasoning { .. }
                    | ChatContent::ReasoningText { .. },
                    ChatRole::Assistant,
                ) => {}
                (ChatContent::ToolResult { .. }, ChatRole::Tool) => {}
                (ChatContent::InputImage { .. }, ChatRole::User) if binding.allows_input_images => {
                }
                (ChatContent::InputImage { .. } | ChatContent::InputAudio { .. }, _) => {
                    return Err(unsupported_operation());
                }
                _ => return Err(invalid_request()),
            }
        }
    }
    Ok(())
}
