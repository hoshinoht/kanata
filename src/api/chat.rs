use axum::{
    extract::{Request, State},
    response::Response,
};
use futures_util::StreamExt;

use crate::adapter::{Adapter, AdapterOutput, EventStream};
use crate::core::{
    ChatContent, ChatRequest, ChatResponse, ModelAlias, Operation, Request as CoreRequest,
    Response as CoreResponse,
};
use crate::routing::RouteEntry;
use crate::server::{Authenticated, ClientState};
use crate::telemetry::Observer;

use super::{
    check_supported,
    deadline::RequestDeadline,
    dispatch::{self, Inbound},
    errors::{
        body_too_large, forbidden, gateway_error_observed, invalid, invalid_param, unavailable,
        upstream_failure_observed,
    },
    serialization,
    sse::{StreamLifetime, StreamOutput, stream_response, typed},
    validate_extensions,
    wire::{ChatWireError, parse_chat},
};

pub(super) async fn chat_completions(
    auth: Authenticated,
    State(state): State<ClientState>,
    request: Request,
) -> Response {
    handle_chat(auth, state, request, OutputFormat::Chat).await
}

#[derive(Clone, Copy)]
pub(super) enum OutputFormat {
    Chat,
    Responses,
    Messages,
}

pub(super) async fn handle_chat(
    auth: Authenticated,
    state: ClientState,
    request: Request,
    output_format: OutputFormat,
) -> Response {
    let (inbound, request_id) = try_step!(dispatch::inbound(&state, &request));
    let max_body_bytes = state.max_audio_chat_body_bytes();
    let (_reservation, bytes) = try_step!(inbound.read_json(&state, request, max_body_bytes).await);
    let raw_body_len = bytes.len();
    let parsed = match output_format {
        OutputFormat::Chat => parse_chat(&bytes, state.max_audio_bytes()),
        OutputFormat::Responses => super::responses::parse(&bytes, state.max_audio_bytes()),
        OutputFormat::Messages => super::messages::parse(&bytes).map(|request| (request, false)),
    };
    drop(bytes);
    let (request, include_usage) = match parsed {
        Ok(request) => request,
        Err(ChatWireError::Malformed) => return invalid(),
        Err(ChatWireError::InvalidParam(param)) => return invalid_param(&param),
        Err(ChatWireError::TooLarge) => return body_too_large(),
    };
    let has_audio = request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|content| matches!(content, ChatContent::InputAudio { .. }))
    });
    if !has_audio && raw_body_len > state.max_body_bytes() {
        return body_too_large();
    }
    dispatch_chat(
        auth,
        state,
        inbound,
        request_id,
        request,
        include_usage,
        output_format,
    )
    .await
}

impl OutputFormat {
    /// Wire name of the reasoning-effort member.
    fn reasoning_param(self) -> &'static str {
        match self {
            Self::Chat => "reasoning_effort",
            Self::Responses => "reasoning.effort",
            Self::Messages => "output_config.effort",
        }
    }
}

/// How a chat result is published to the client.
struct Publication {
    routed_model: ModelAlias,
    response_model: ModelAlias,
    expose_reasoning: bool,
    include_usage: bool,
    response_options: Option<super::responses::Options>,
    message_id: Option<String>,
}

async fn dispatch_chat(
    auth: Authenticated,
    state: ClientState,
    inbound: Inbound,
    request_id: String,
    mut chat: ChatRequest,
    include_usage: bool,
    output_format: OutputFormat,
) -> Response {
    let reasoning_param = output_format.reasoning_param();
    let stream = chat.stream;
    let route = try_step!(resolve_route(
        &auth,
        &state,
        &inbound,
        &mut chat,
        output_format
    ));
    let response_model = if route.capabilities.reasoning_control {
        ModelAlias(route.model_family_alias().to_owned())
    } else {
        route.identity.selector.model_alias.clone()
    };
    let routed_model = route.identity.selector.model_alias.clone();
    // Adapters that apply their own pin keep it; others (including private-only proxies)
    // forward the request's effort, so a pinned route supplies it. Resolution already
    // rejected a client effort that differs from the pin.
    if !route.provider_kind.applies_route_effort_pin() && chat.options.reasoning_effort.is_none() {
        chat.options.reasoning_effort = route
            .pinned_reasoning_effort
            .and_then(crate::core::ReasoningEffort::parse);
    }
    if let Some(observer) = inbound.observer() {
        observer.annotate_route(&routed_model.0, Operation::Chat, stream);
    }
    let Some(adapter) = state.adapter(&route.adapter_id) else {
        return unavailable();
    };
    try_step!(apply_route_options(
        &state,
        route,
        &*adapter,
        &mut chat,
        output_format
    ));
    let extensions = chat.extensions.clone();
    let reasoning_effort = chat.options.reasoning_effort;
    let max_output_tokens = chat.options.max_output_tokens;
    let max_output_tokens_param = chat.options.max_output_tokens_param;
    try_step!(check_provider_options(route, &chat));
    chat.model = response_model.clone();
    // Reasoning stays on the private listener.
    let expose_reasoning = state.listener() == crate::telemetry::Listener::Private;
    chat.include_encrypted_reasoning &= expose_reasoning;
    let publication = Publication {
        response_options: matches!(output_format, OutputFormat::Responses).then(|| {
            super::responses::Options::new(&chat, expose_reasoning, route.reasoning_summary)
        }),
        message_id: matches!(output_format, OutputFormat::Messages)
            .then(|| format!("msg_{request_id}")),
        routed_model: routed_model.clone(),
        response_model,
        expose_reasoning,
        include_usage,
    };
    chat.model = routed_model;
    let request = CoreRequest::Chat(chat);
    try_step!(check_route_request(
        route,
        &*adapter,
        &request,
        reasoning_effort,
        (max_output_tokens, max_output_tokens_param),
        reasoning_param,
    ));
    if let (Some(observer), Some(effort)) = (
        inbound.observer(),
        reasoning_effort
            .map(|effort| effort.as_str())
            .or(route.pinned_reasoning_effort),
    ) {
        observer.annotate_reasoning_effort(effort);
    }
    let routed = try_step!(dispatch::routed(route, request_id, extensions, request));
    let execution = inbound.execute(&auth, &state, route, &*adapter, routed);
    let (permit, result) = try_step!(execution.await);
    let Inbound { observer, deadline } = inbound;
    match result {
        Ok(AdapterOutput::Complete(CoreResponse::Chat(response))) if !stream => {
            respond_complete(response, publication, observer.as_ref())
        }
        Ok(AdapterOutput::Events(events)) if stream => {
            let lifetime = StreamLifetime::new(permit, observer, state.sse_keepalive_ms());
            respond_stream(events, publication, &state, deadline, lifetime).await
        }
        Ok(_) => upstream_failure_observed(observer.as_ref()),
        Err(error) => gateway_error_observed(error, observer.as_ref()),
    }
}

/// Advisory Messages effort, route annotation and authorized chat resolution.
fn resolve_route<'a>(
    auth: &Authenticated,
    state: &'a ClientState,
    inbound: &Inbound,
    chat: &mut ChatRequest,
    output_format: OutputFormat,
) -> Result<&'a RouteEntry, Box<Response>> {
    let selector = crate::core::RouteSelector {
        model_alias: chat.model.clone(),
        operation: Operation::Chat,
    };
    // `output_config.effort` is advisory: it applies only where the alias has reasoning control.
    if matches!(output_format, OutputFormat::Messages)
        && chat.options.reasoning_effort.is_some()
        && !state.registry().routes().any(|route| {
            route.identity.selector.operation == Operation::Chat
                && route.capabilities.reasoning_control
                && route.model_family_alias() == chat.model.0
        })
    {
        chat.options.reasoning_effort = None;
    }
    if let (Some(observer), Some(route)) = (inbound.observer(), state.registry().resolve(&selector))
    {
        observer.annotate_route(
            &route.identity.selector.model_alias.0,
            Operation::Chat,
            chat.stream,
        );
    }
    state
        .registry()
        .resolve_chat(&selector, chat.options.reasoning_effort, |selector| {
            auth.authorize(selector).is_ok()
        })
        .map_err(|error| {
            Box::new(match error {
                crate::routing::ChatRouteError::Forbidden => forbidden(),
                crate::routing::ChatRouteError::Missing => invalid(),
                crate::routing::ChatRouteError::ReasoningEffort => {
                    invalid_param(output_format.reasoning_param())
                }
            })
        })
}

/// Extension allowlist check and the route's output-token limits.
fn apply_route_options(
    state: &ClientState,
    route: &RouteEntry,
    adapter: &dyn Adapter,
    chat: &mut ChatRequest,
    output_format: OutputFormat,
) -> Result<(), Box<Response>> {
    if !validate_extensions(
        &chat.extensions,
        &route.extension_allowlist,
        &route.adapter_extension_allowlist,
        state.max_extension_bytes(),
    ) {
        return Err(Box::new(invalid()));
    }
    // A route without sampling controls cannot enforce an output limit; the requested
    // limit is dropped rather than refused, on every endpoint.
    if !route.capabilities.sampling_controls || !adapter.capabilities().sampling_controls {
        chat.options.max_output_tokens = None;
    }
    // Messages requires `max_tokens`; above the route cap it is clamped there, so
    // `stop_reason: max_tokens` still reports the limit that applied.
    if let Some(cap) = route.max_output_tokens
        && chat.options.max_output_tokens.is_none_or(|requested| {
            requested > cap && matches!(output_format, OutputFormat::Messages)
        })
    {
        chat.options.max_output_tokens = Some(cap);
    }
    Ok(())
}

/// Capability, effort and output-token checks of the routed request.
fn check_route_request(
    route: &RouteEntry,
    adapter: &dyn Adapter,
    request: &CoreRequest,
    reasoning_effort: Option<crate::core::ReasoningEffort>,
    (max_output_tokens, max_output_tokens_param): (Option<u32>, crate::core::MaxTokensParam),
    reasoning_param: &'static str,
) -> Result<(), Box<Response>> {
    match check_supported(&route.capabilities, adapter.capabilities(), request) {
        Ok(()) => {}
        Err(Some(param)) => {
            return Err(Box::new(invalid_param(if param == "reasoning_effort" {
                reasoning_param
            } else {
                param
            })));
        }
        Err(None) => return Err(Box::new(invalid())),
    }
    if reasoning_effort.is_some_and(|effort| !route.provider_kind.accepts_reasoning_effort(effort))
    {
        return Err(Box::new(invalid_param(reasoning_param)));
    }
    if let (Some(requested), Some(cap)) = (max_output_tokens, route.max_output_tokens)
        && requested > cap
    {
        return Err(Box::new(invalid_param(max_output_tokens_param.as_str())));
    }
    Ok(())
}

/// Options only some provider kinds accept.
fn check_provider_options(route: &RouteEntry, chat: &ChatRequest) -> Result<(), Box<Response>> {
    if chat.options.enable_thinking.is_some()
        && (!route.provider_kind.accepts_enable_thinking()
            || chat.options.reasoning_effort.is_some())
    {
        return Err(Box::new(invalid_param("chat_template_kwargs")));
    }
    if !route.provider_kind.accepts_stop_and_penalties() {
        let sampling = &chat.options.sampling;
        if !sampling.stop.is_empty() {
            return Err(Box::new(invalid_param(chat.options.stop_param.as_str())));
        }
        if sampling.presence_penalty.is_some() {
            return Err(Box::new(invalid_param("presence_penalty")));
        }
        if sampling.frequency_penalty.is_some() {
            return Err(Box::new(invalid_param("frequency_penalty")));
        }
    }
    Ok(())
}

fn respond_complete(
    mut response: ChatResponse,
    publication: Publication,
    observer: Option<&Observer>,
) -> Response {
    if response.model != publication.routed_model {
        return upstream_failure_observed(observer);
    }
    response.model = publication.response_model.clone();
    if !publication.expose_reasoning {
        response.usage = response
            .usage
            .map(crate::core::Usage::without_cached_input_tokens);
    }
    if let Some(options) = publication.response_options {
        return super::responses::complete(response, options, observer);
    }
    if let Some(id) = publication.message_id {
        return super::messages::complete(response, &publication.response_model, id, observer);
    }
    serialization::chat_response(
        response,
        &publication.response_model,
        publication.expose_reasoning,
        observer,
    )
}

async fn respond_stream(
    events: EventStream,
    publication: Publication,
    state: &ClientState,
    deadline: RequestDeadline,
    lifetime: StreamLifetime,
) -> Response {
    let Publication {
        routed_model,
        response_model,
        expose_reasoning,
        include_usage,
        response_options,
        message_id,
    } = publication;
    let published_model = response_model.clone();
    let events = Box::pin(events.map(move |event| match event {
        Ok(crate::core::NormalizedEvent::ChatStarted { model }) => {
            if model == routed_model {
                Ok(crate::core::NormalizedEvent::ChatStarted {
                    model: published_model.clone(),
                })
            } else {
                Err(crate::core::GatewayError {
                    kind: crate::core::ErrorKind::UpstreamFailure,
                })
            }
        }
        Ok(crate::core::NormalizedEvent::ChatCompleted {
            finish_reason,
            usage,
        }) if !expose_reasoning => Ok(crate::core::NormalizedEvent::ChatCompleted {
            finish_reason,
            usage: usage.map(crate::core::Usage::without_cached_input_tokens),
        }),
        other => other,
    }));
    if let Some(options) = response_options {
        return typed::stream_response(
            events,
            response_model,
            super::responses::stream_start(options),
            deadline,
            state.first_byte_ms(),
            state.idle_ms(),
            lifetime,
        )
        .await;
    }
    if let Some(id) = message_id {
        return typed::stream_response(
            events,
            response_model,
            super::messages::stream_start(id),
            deadline,
            state.first_byte_ms(),
            state.idle_ms(),
            lifetime,
        )
        .await;
    }
    stream_response(
        events,
        response_model,
        StreamOutput {
            include_usage,
            expose_reasoning,
        },
        deadline,
        state.first_byte_ms(),
        state.idle_ms(),
        lifetime,
    )
    .await
}
