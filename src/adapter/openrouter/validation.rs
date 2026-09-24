use crate::{
    config::ValidatedRoute,
    core::{
        Capabilities, ChatContent, ChatRole, ErrorKind, GatewayError, Operation, Request,
        RouteIdentity, RoutedRequest, ToolChoice, TrustZone,
    },
};

pub(super) struct RouteBinding {
    identity: RouteIdentity,
}

impl RouteBinding {
    pub(super) fn identity(&self) -> &RouteIdentity {
        &self.identity
    }
}

pub(super) fn bind_route(
    adapter_id: &str,
    route: &ValidatedRoute,
    streaming_chat: bool,
) -> Result<RouteBinding, GatewayError> {
    if route.adapter_id() != adapter_id
        || route.identity().selector.operation != Operation::Chat
        || route.identity().route_id.trim().is_empty()
        || route.identity().selector.model_alias.0.trim().is_empty()
        || route.identity().upstream_id.trim().is_empty()
        || (route.requires_streaming_chat() && !streaming_chat)
        || route.requires_function_tools()
        || route.allows_input_audio()
        || route.allows_audio_streaming_chat()
        || route.allows_audio_function_tools()
    {
        return Err(internal_error());
    }
    Ok(RouteBinding {
        identity: route.identity().clone(),
    })
}

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
        || !route_bindings
            .iter()
            .any(|binding| binding.identity == context.route)
    {
        return Err(invalid_request());
    }

    match request {
        Request::Chat(chat) => validate_chat(chat),
        Request::Transcription(_) => Err(unsupported_operation()),
    }
}

fn validate_chat(chat: &crate::core::ChatRequest) -> Result<(), GatewayError> {
    if chat.model.0.trim().is_empty() || chat.messages.is_empty() {
        return Err(invalid_request());
    }
    if !chat.tools.is_empty() || !matches!(&chat.tool_choice, ToolChoice::Auto | ToolChoice::None) {
        return Err(unsupported_operation());
    }
    if chat.extensions.iter().next().is_some() {
        return Err(invalid_request());
    }

    for message in &chat.messages {
        if message.content.is_empty() {
            return Err(invalid_request());
        }
        if message.role == ChatRole::Tool {
            return Err(unsupported_operation());
        }
        for content in &message.content {
            match content {
                ChatContent::Text { text } if !text.is_empty() => {}
                ChatContent::Text { .. } => return Err(invalid_request()),
                ChatContent::InputAudio { .. }
                | ChatContent::ToolCall { .. }
                | ChatContent::ToolResult { .. } => return Err(unsupported_operation()),
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
