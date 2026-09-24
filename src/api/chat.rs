use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, header},
    response::Response,
};

use crate::adapter::AdapterOutput;
use crate::core::{
    ChatContent, ChatRequest, Operation, Request as CoreRequest, RequestContext,
    Response as CoreResponse, RoutedRequest,
};
use crate::routing::admission::AdmissionError;
use crate::server::{Authenticated, ClientState};

use super::{
    check_supported,
    deadline::RequestDeadline,
    errors::{
        body_too_large, forbidden, gateway_error_observed, invalid, invalid_param, request_id,
        server_draining_observed, unavailable, upstream_failure_observed,
    },
    serialization,
    sse::{StreamLifetime, stream_response},
    validate_extensions,
    wire::ChatWire,
};
use crate::telemetry::Observer;

pub(super) async fn chat_completions(
    auth: Authenticated,
    State(state): State<ClientState>,
    request: Request,
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
    if !has_json_content_type(request.headers()) {
        return invalid();
    }
    let max_body_bytes = state.max_audio_chat_body_bytes();
    let bytes = match deadline
        .run(move || to_bytes(request.into_body(), max_body_bytes))
        .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return body_too_large(),
        Err(error) => return gateway_error_observed(error, observer.as_ref()),
    };
    let raw_body_len = bytes.len();
    let wire: ChatWire = match serde_json::from_slice(&bytes) {
        Ok(wire) => wire,
        Err(_) => return invalid(),
    };
    drop(bytes);
    let (request, include_usage) = match wire.into_core(state.max_audio_bytes()) {
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
    )
    .await
}

async fn dispatch_chat(
    auth: Authenticated,
    state: ClientState,
    deadline: RequestDeadline,
    request_id: String,
    chat: ChatRequest,
    include_usage: bool,
    observer: Option<Observer>,
) -> Response {
    let response_model = chat.model.clone();
    let stream = chat.stream;
    let selector = crate::core::RouteSelector {
        model_alias: response_model.clone(),
        operation: Operation::Chat,
    };
    if let (Some(observer), Some(_)) = (observer.as_ref(), state.registry().resolve(&selector)) {
        observer.annotate_route(&response_model.0, Operation::Chat, stream);
    }
    if auth.authorize(&selector).is_err() {
        return forbidden();
    }
    let Some(route) = state.registry().resolve(&selector) else {
        return invalid();
    };
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
    let request = CoreRequest::Chat(chat);
    match check_supported(&route.capabilities, adapter.capabilities(), &request) {
        Ok(()) => {}
        Err(Some(param)) => return invalid_param(param),
        Err(None) => return invalid(),
    }
    if reasoning_effort.is_some_and(|effort| !route.provider_kind.accepts_reasoning_effort(effort))
    {
        return invalid_param("reasoning_effort");
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
    let permit = match deadline.run(|| state.admission().acquire(route)).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(AdmissionError::Closed)) => {
            return server_draining_observed(observer.as_ref());
        }
        Ok(Err(error)) => {
            return gateway_error_observed(error.gateway_error(), observer.as_ref());
        }
        Err(error) => return gateway_error_observed(error, observer.as_ref()),
    };
    if let Some(observer) = observer.as_ref() {
        observer.annotate_adapter(&route.adapter_id, &provider_label(route));
    }
    match deadline.run(move || adapter.execute(routed)).await {
        Ok(Ok(AdapterOutput::Complete(CoreResponse::Chat(response)))) if !stream => {
            serialization::chat_response(response, &response_model, observer.as_ref())
        }
        Ok(Ok(AdapterOutput::Events(events))) if stream => {
            stream_response(
                events,
                response_model,
                include_usage,
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

fn has_json_content_type(headers: &HeaderMap) -> bool {
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
