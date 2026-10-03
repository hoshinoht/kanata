use std::{
    convert::Infallible,
    pin::Pin,
    task::{Context, Poll},
};

use axum::{
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use http_body::{Frame, SizeHint};

use super::{check_supported, deadline::RequestDeadline, errors::*};
use crate::{
    adapter::AdapterOutput,
    core::{
        Extensions, Operation, Request as CoreRequest, RequestContext, Response as CoreResponse,
        RouteSelector, RoutedRequest, SpeechRequest,
    },
    routing::admission::AdmissionPermit,
    server::{Authenticated, ClientState},
    telemetry::Observer,
};

pub(super) async fn speech(
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
    let Some(request_id) = request_id(request.headers(), &state) else {
        return invalid();
    };
    annotate_client_request_id(observer.as_ref(), request.headers(), &request_id);
    if !super::chat::has_json_content_type(request.headers()) {
        return invalid();
    }
    let (_reservation, max_body_bytes) =
        match super::upload::reserve(&state, request.headers(), state.max_body_bytes()) {
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
    let speech: SpeechRequest = match serde_json::from_slice(&bytes) {
        Ok(speech) => speech,
        Err(_) => return invalid(),
    };
    drop(bytes);
    if let Err(param) = speech.validate() {
        return invalid_param(param);
    }
    let selector = RouteSelector {
        model_alias: speech.model.clone(),
        operation: Operation::Speech,
    };
    if let (Some(observer), Some(_)) = (observer.as_ref(), state.registry().resolve(&selector)) {
        observer.annotate_route(&speech.model.0, Operation::Speech, false);
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
    let Some(policy) = route.speech.as_ref() else {
        return invalid();
    };
    if let Err(param) = speech.check_policy(policy) {
        return invalid_param(param);
    }
    let request = CoreRequest::Speech(speech.clone());
    if check_supported(&route.capabilities, adapter.capabilities(), &request).is_err() {
        return invalid();
    }
    let context = RequestContext {
        request_id,
        route: route.identity.clone(),
        trust_zone: route.trust_zone,
        extensions: Extensions::default(),
    };
    let routed = match RoutedRequest::new(context, request) {
        Ok(request) => request,
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
        observer.annotate_adapter(&route.adapter_id, route.provider_kind.label());
    }
    let result = deadline.run(move || adapter.execute(routed)).await;
    if let Some(observer) = observer.as_ref() {
        observer.end_upstream();
    }
    if let Ok(outcome) = &result {
        permit.record_outcome(outcome);
    }
    match result {
        Ok(Ok(AdapterOutput::Complete(CoreResponse::Speech(response))))
            if response.valid_for(&speech) =>
        {
            if let Some(observer) = observer.as_ref() {
                observer.first_content();
                observer.record_usage(None);
            }
            let media_type = response.format.media_type();
            let mut response = Body::new(SpeechBody {
                bytes: Some(Bytes::from(response.bytes)),
                _permit: permit,
            })
            .into_response();
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(media_type));
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response.headers_mut().insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            response
        }
        Ok(Ok(_)) => upstream_failure_observed(observer.as_ref()),
        Ok(Err(error)) | Err(error) => gateway_error_observed(error, observer.as_ref()),
    }
}

struct SpeechBody {
    bytes: Option<Bytes>,
    _permit: AdmissionPermit,
}

impl http_body::Body for SpeechBody {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        Poll::Ready(self.bytes.take().map(|bytes| Ok(Frame::data(bytes))))
    }
    fn is_end_stream(&self) -> bool {
        self.bytes.is_none()
    }
    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.bytes.as_ref().map_or(0, |bytes| bytes.len() as u64))
    }
}
