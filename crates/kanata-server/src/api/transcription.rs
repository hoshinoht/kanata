use axum::{
    Json,
    extract::{Request, State},
    http::header,
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::adapter::AdapterOutput;
use crate::core::{
    ModelAlias, Operation, Request as CoreRequest, Response as CoreResponse, RouteSelector,
    TranscriptionRequest, TranscriptionResponse,
};
use crate::server::{Authenticated, ClientState};

use super::{
    check_supported, dispatch,
    errors::{body_too_large, gateway_error_observed, invalid, upstream_failure_observed},
    multipart::{MultipartInputError, parse_multipart},
    validate_extensions,
};

pub(super) async fn transcriptions(
    auth: Authenticated,
    State(state): State<ClientState>,
    request: Request,
) -> Response {
    let (inbound, request_id) = try_step!(dispatch::inbound(&state, &request));
    let max_audio_bytes = state.max_audio_bytes();
    let maximum = max_audio_bytes.saturating_add(crate::core::MAX_TRANSCRIPTION_OVERHEAD_BYTES);
    let (_reservation, body_limit) =
        try_step!(super::upload::reserve(&state, request.headers(), maximum));
    let wire = match super::upload::read(
        inbound.deadline,
        state.upload_ms(),
        parse_multipart(request, max_audio_bytes, body_limit),
    )
    .await
    {
        Ok(Ok(value)) => value,
        Ok(Err(MultipartInputError::TooLarge)) => return body_too_large(),
        Ok(Err(MultipartInputError::Invalid)) => return invalid(),
        Err(error) => return gateway_error_observed(error, inbound.observer()),
    };
    let selector = RouteSelector {
        model_alias: ModelAlias(wire.model.clone()),
        operation: Operation::Transcription,
    };
    let (route, adapter) = try_step!(inbound.resolve(&state, &auth, &selector));
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
    if check_supported(&route.capabilities, adapter.capabilities(), &request).is_err() {
        return invalid();
    }
    let routed = try_step!(dispatch::routed(route, request_id, extensions, request));
    let execution = inbound.execute(&auth, &state, route, &*adapter, routed);
    let (_permit, result) = try_step!(execution.await);
    let observer = inbound.observer();
    match result {
        Ok(AdapterOutput::Complete(CoreResponse::Transcription(TranscriptionResponse {
            text,
        }))) => {
            if let Some(observer) = observer {
                observer.first_content();
            }
            match wire.response_format.as_deref() {
                None | Some("json") => Json(json!({"text": text})).into_response(),
                Some("text") => {
                    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
                }
                _ => invalid(),
            }
        }
        Ok(_) => upstream_failure_observed(observer),
        Err(error) => gateway_error_observed(error, observer),
    }
}
