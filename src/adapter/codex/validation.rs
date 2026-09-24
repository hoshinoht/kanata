use crate::{
    config::{CodexReasoningEffort, ValidatedRoute},
    core::{
        Capabilities, ErrorKind, GatewayError, Operation, Request, RouteIdentity, RoutedRequest,
        TrustZone,
    },
};

pub(super) struct RouteBinding {
    identity: RouteIdentity,
    reasoning_effort: CodexReasoningEffort,
}

impl RouteBinding {
    pub(super) fn identity(&self) -> &RouteIdentity {
        &self.identity
    }

    pub(super) fn reasoning_effort(&self) -> CodexReasoningEffort {
        self.reasoning_effort
    }
}

/// A request effort overrides the route's alias effort.
pub(super) fn reasoning_effort(
    routed: &RoutedRequest,
    binding: &RouteBinding,
) -> Result<CodexReasoningEffort, GatewayError> {
    let Request::Chat(chat) = routed.request() else {
        return Err(unsupported_operation());
    };
    match chat.options.reasoning_effort {
        Some(effort) => CodexReasoningEffort::from_request(effort).ok_or_else(invalid_request),
        None => Ok(binding.reasoning_effort()),
    }
}

pub(super) fn bind_route(
    adapter_id: &str,
    route: &ValidatedRoute,
) -> Result<RouteBinding, GatewayError> {
    let identity = route.identity();
    let reasoning_effort = route.codex_reasoning_effort().ok_or_else(internal_error)?;
    if route.adapter_id() != adapter_id
        || identity.selector.operation != Operation::Chat
        || identity.route_id.trim().is_empty()
        || identity.selector.model_alias.0.trim().is_empty()
        || identity.upstream_id.trim().is_empty()
        || route.allows_input_audio()
        || route.allows_audio_streaming_chat()
        || route.allows_audio_function_tools()
        || !route.extension_allowlist().is_empty()
    {
        return Err(internal_error());
    }

    Ok(RouteBinding {
        identity: identity.clone(),
        reasoning_effort,
    })
}

pub(super) fn validate<'a>(
    routed: &RoutedRequest,
    capabilities: &Capabilities,
    route_bindings: &'a [RouteBinding],
) -> Result<&'a RouteBinding, GatewayError> {
    capabilities
        .check_request(routed.request())
        .map_err(|_| unsupported_operation())?;

    let context = routed.context();
    if context.trust_zone != TrustZone::External
        || context.extensions.iter().next().is_some()
        || context.check_request(routed.request()).is_err()
    {
        return Err(invalid_request());
    }

    let binding = route_bindings
        .iter()
        .find(|binding| binding.identity() == &context.route)
        .ok_or_else(invalid_request)?;

    let Request::Chat(chat) = routed.request() else {
        return Err(unsupported_operation());
    };
    if !capabilities.function_tools
        && chat.messages.iter().any(|message| {
            message.content.iter().any(|content| {
                matches!(
                    content,
                    crate::core::ChatContent::ToolCall { .. }
                        | crate::core::ChatContent::ToolResult { .. }
                )
            })
        })
    {
        return Err(unsupported_operation());
    }
    Ok(binding)
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
