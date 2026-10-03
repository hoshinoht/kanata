use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, header},
    response::Response,
};
use futures_util::StreamExt;

use crate::adapter::AdapterOutput;
use crate::core::{
    ChatContent, ChatRequest, Operation, Request as CoreRequest, RequestContext,
    Response as CoreResponse, RoutedRequest,
};
use crate::server::{Authenticated, ClientState};

use super::{
    check_supported,
    deadline::RequestDeadline,
    errors::{
        admission_rejected_observed, annotate_client_request_id, body_too_large, forbidden,
        gateway_error_observed, invalid, invalid_param, quota_rejected, request_id, reserve_usage,
        server_draining_observed, unavailable, upstream_failure_observed,
    },
    serialization,
    sse::{StreamLifetime, StreamOutput, stream_response},
    validate_extensions,
    wire::ChatWire,
};
use crate::telemetry::Observer;

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
}

pub(super) async fn handle_chat(
    auth: Authenticated,
    state: ClientState,
    request: Request,
    output_format: OutputFormat,
) -> Response {
    let observer = request.extensions().get::<Observer>().cloned();
    if state.admission().is_closed() {
        return server_draining_observed(observer.as_ref());
    }
    let deadline = match RequestDeadline::new(state.overall_ms()) {
        Ok(deadline) => deadline,
        Err(error) => return gateway_error_observed(error, observer.as_ref()),
    };
    let request_id = request_id(request.headers(), &state);
    let Some(request_id) = request_id else {
        return invalid();
    };
    annotate_client_request_id(observer.as_ref(), request.headers(), &request_id);
    if !has_json_content_type(request.headers()) {
        return invalid();
    }
    let (_reservation, max_body_bytes) = match super::upload::reserve(
        &state,
        request.headers(),
        state.max_audio_chat_body_bytes(),
    ) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let bytes = match super::upload::read(
        deadline,
        state.upload_ms(),
        to_bytes(request.into_body(), max_body_bytes),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return body_too_large(),
        Err(error) => return gateway_error_observed(error, observer.as_ref()),
    };
    let raw_body_len = bytes.len();
    let parsed = match output_format {
        OutputFormat::Chat => match serde_json::from_slice::<ChatWire>(&bytes) {
            Ok(wire) => wire.into_core(state.max_audio_bytes()),
            Err(_) => return invalid(),
        },
        OutputFormat::Responses => super::responses::parse(&bytes, state.max_audio_bytes()),
    };
    drop(bytes);
    let (request, include_usage) = match parsed {
        Ok(request) => request,
        Err(super::wire::ChatWireError::Invalid) => return invalid(),
        Err(super::wire::ChatWireError::InvalidParam(param)) => return invalid_param(param),
        Err(super::wire::ChatWireError::TooLarge) => return body_too_large(),
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
        deadline,
        request_id,
        request,
        include_usage,
        observer,
        output_format,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_chat(
    auth: Authenticated,
    state: ClientState,
    deadline: RequestDeadline,
    request_id: String,
    mut chat: ChatRequest,
    include_usage: bool,
    observer: Option<Observer>,
    output_format: OutputFormat,
) -> Response {
    let reasoning_param = if matches!(output_format, OutputFormat::Responses) {
        "reasoning.effort"
    } else {
        "reasoning_effort"
    };
    let stream = chat.stream;
    let selector = crate::core::RouteSelector {
        model_alias: chat.model.clone(),
        operation: Operation::Chat,
    };
    if let (Some(observer), Some(route)) = (observer.as_ref(), state.registry().resolve(&selector))
    {
        observer.annotate_route(
            &route.identity.selector.model_alias.0,
            Operation::Chat,
            stream,
        );
    }
    let route =
        match state
            .registry()
            .resolve_chat(&selector, chat.options.reasoning_effort, |selector| {
                auth.authorize(selector).is_ok()
            }) {
            Ok(route) => route,
            Err(crate::routing::ChatRouteError::Forbidden) => return forbidden(),
            Err(crate::routing::ChatRouteError::Missing) => return invalid(),
            Err(crate::routing::ChatRouteError::ReasoningEffort) => {
                return invalid_param(reasoning_param);
            }
        };
    let response_model = if route.capabilities.reasoning_control {
        crate::core::ModelAlias(route.model_family_alias().to_owned())
    } else {
        route.identity.selector.model_alias.clone()
    };
    let routed_model = route.identity.selector.model_alias.clone();
    if let Some(observer) = observer.as_ref() {
        observer.annotate_route(&routed_model.0, Operation::Chat, stream);
    }
    let Some(adapter) = state.adapter(&route.adapter_id) else {
        return unavailable();
    };
    if !validate_extensions(
        &chat.extensions,
        &route.extension_allowlist,
        &route.adapter_extension_allowlist,
        state.max_extension_bytes(),
    ) {
        return invalid();
    }
    let extensions = chat.extensions.clone();
    let reasoning_effort = chat.options.reasoning_effort;
    let max_output_tokens = chat.options.max_output_tokens;
    let max_output_tokens_param = chat.options.max_output_tokens_param;
    if chat.options.enable_thinking.is_some() && !route.provider_kind.accepts_enable_thinking() {
        return invalid_param("chat_template_kwargs");
    }
    if let Some(cap) = route.max_output_tokens
        && chat.options.max_output_tokens.is_none()
    {
        chat.options.max_output_tokens = Some(cap);
    }
    chat.model = response_model.clone();
    let response_options = matches!(output_format, OutputFormat::Responses)
        .then(|| super::responses::Options::new(&chat));
    chat.model = routed_model.clone();
    let request = CoreRequest::Chat(chat);
    match check_supported(&route.capabilities, adapter.capabilities(), &request) {
        Ok(()) => {}
        Err(Some(param)) => {
            return invalid_param(if param == "reasoning_effort" {
                reasoning_param
            } else {
                param
            });
        }
        Err(None) => return invalid(),
    }
    if reasoning_effort.is_some_and(|effort| !route.provider_kind.accepts_reasoning_effort(effort))
    {
        return invalid_param(reasoning_param);
    }
    if let (Some(requested), Some(cap)) = (max_output_tokens, route.max_output_tokens)
        && requested > cap
    {
        return invalid_param(max_output_tokens_param.as_str());
    }
    if let (Some(observer), Some(effort)) = (
        observer.as_ref(),
        reasoning_effort
            .map(|effort| effort.as_str())
            .or(route.pinned_reasoning_effort),
    ) {
        observer.annotate_reasoning_effort(effort);
    }
    let context = RequestContext {
        request_id,
        route: route.identity.clone(),
        trust_zone: route.trust_zone,
        extensions,
    };
    let routed = match RoutedRequest::new(context, request) {
        Ok(value) => value,
        Err(_) => return invalid(),
    };
    if let Some(observer) = observer.as_ref() {
        observer.begin_queue();
    }
    let admission = deadline
        .run(|| state.admission().acquire(route, auth.key_limits()))
        .await;
    if let Some(observer) = observer.as_ref() {
        observer.end_queue();
    }
    let mut permit = match admission {
        Ok(Ok(permit)) => permit,
        Ok(Err(error)) => {
            return admission_rejected_observed(
                error,
                state.admission().retry_after_secs(error),
                observer.as_ref(),
            );
        }
        Err(error) => return gateway_error_observed(error, observer.as_ref()),
    };
    match deadline
        .run(|| reserve_usage(&auth, observer.as_ref()))
        .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return quota_rejected(error, observer.as_ref()),
        Err(error) => return gateway_error_observed(error, observer.as_ref()),
    }
    if let Some(observer) = observer.as_ref() {
        observer.begin_upstream();
        observer.annotate_adapter(&route.adapter_id, &provider_label(route));
    }
    // Reasoning stays on the private listener.
    let expose_reasoning = state.listener() == crate::telemetry::Listener::Private;
    let result = deadline.run(move || adapter.execute(routed)).await;
    if !matches!(&result, Ok(Ok(AdapterOutput::Events(_))))
        && let Some(observer) = observer.as_ref()
    {
        observer.end_upstream();
    }
    if let Ok(outcome) = &result {
        permit.record_outcome(outcome);
    }
    match result {
        Ok(Ok(AdapterOutput::Complete(CoreResponse::Chat(mut response)))) if !stream => {
            if response.model != routed_model {
                return upstream_failure_observed(observer.as_ref());
            }
            response.model = response_model.clone();
            if let Some(options) = response_options {
                return super::responses::complete(response, options, observer.as_ref());
            }
            serialization::chat_response(
                response,
                &response_model,
                expose_reasoning,
                observer.as_ref(),
            )
        }
        Ok(Ok(AdapterOutput::Events(events))) if stream => {
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
                other => other,
            }));
            if let Some(options) = response_options {
                return super::responses::stream_response(
                    events,
                    response_model,
                    options,
                    deadline,
                    state.first_byte_ms(),
                    state.idle_ms(),
                    StreamLifetime { permit, observer },
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
                StreamLifetime { permit, observer },
            )
            .await
        }
        Ok(Ok(_)) => upstream_failure_observed(observer.as_ref()),
        Ok(Err(error)) => gateway_error_observed(error, observer.as_ref()),
        Err(error) => gateway_error_observed(error, observer.as_ref()),
    }
}

pub(super) fn has_json_content_type(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    value
        .to_str()
        .ok()
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
}

/// Lowercased config kind name; kept generic so this layer names no concrete provider.
pub(super) fn provider_label(route: &crate::routing::RouteEntry) -> String {
    route.provider_kind.label().to_owned()
}
