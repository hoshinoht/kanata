use axum::{
    Json,
    extract::{Request, State},
    http::header,
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::adapter::AdapterOutput;
use crate::core::{
    ModelAlias, Operation, Request as CoreRequest, RequestContext, Response as CoreResponse,
    RoutedRequest, TranscriptionRequest, TranscriptionResponse,
};
use crate::routing::admission::AdmissionError;
use crate::server::{Authenticated, ClientState};

use super::{
    deadline::RequestDeadline,
    errors::{
        body_too_large, forbidden, gateway_error_observed, invalid, request_id,
        server_draining_observed, unavailable, upstream_failure_observed,
    },
    multipart::{MultipartInputError, parse_multipart},
    supported, validate_extensions,
};
use crate::telemetry::Observer;

pub(super) async fn transcriptions(
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
    let request_id = match request_id(request.headers(), &state) {
        Some(value) => value,
        None => return invalid(),
    };
    let max_audio_bytes = state.max_audio_bytes();
    let wire = match deadline
        .run(move || parse_multipart(request, max_audio_bytes))
        .await
    {
        Ok(Ok(value)) => value,
        Ok(Err(MultipartInputError::TooLarge)) => return body_too_large(),
        Ok(Err(MultipartInputError::Invalid)) => return invalid(),
        Err(error) => return gateway_error_observed(error, observer.as_ref()),
    };
    let selector = crate::core::RouteSelector {
        model_alias: ModelAlias(wire.model.clone()),
        operation: Operation::Transcription,
    };
    if let (Some(observer), Some(_)) = (observer.as_ref(), state.registry().resolve(&selector)) {
        observer.annotate_route(&wire.model, Operation::Transcription, false);
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
    let extensions = wire.extensions;
    let request = CoreRequest::Transcription(TranscriptionRequest {
        model: ModelAlias(wire.model),
        file: wire.file,
        language: wire.language,
        prompt: wire.prompt,
        extensions: extensions.clone(),
    });
    if !validate_extensions(
        &extensions,
        &route.extension_allowlist,
        &route.adapter_extension_allowlist,
        state.max_extension_bytes(),
    ) {
        return invalid();
    }
    if !supported(&route.capabilities, adapter.capabilities(), &request) {
        return invalid();
    }
    let routed = match RoutedRequest::new(
        RequestContext {
            request_id,
            route: route.identity.clone(),
            trust_zone: route.trust_zone,
            extensions,
        },
        request,
    ) {
        Ok(value) => value,
        Err(_) => return invalid(),
    };
    let _permit = match deadline.run(|| state.admission().acquire(route)).await {
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
        observer.annotate_adapter(&route.adapter_id, &super::chat::provider_label(route));
    }
    match deadline.run(move || adapter.execute(routed)).await {
        Ok(Ok(AdapterOutput::Complete(CoreResponse::Transcription(TranscriptionResponse {
            text,
        })))) => match wire.response_format.as_deref() {
            None | Some("json") => Json(json!({"text": text})).into_response(),
            Some("text") => {
                ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
            }
            _ => invalid(),
        },
        Ok(Ok(_)) => upstream_failure_observed(observer.as_ref()),
        Ok(Err(error)) => gateway_error_observed(error, observer.as_ref()),
        Err(error) => gateway_error_observed(error, observer.as_ref()),
    }
}
