use std::{
    convert::Infallible,
    pin::Pin,
    task::{Context, Poll},
};

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use http_body::{Frame, SizeHint};

use super::{check_supported, dispatch, errors::*};
use crate::{
    adapter::AdapterOutput,
    core::{
        Extensions, Operation, Request as CoreRequest, Response as CoreResponse, RouteSelector,
        SpeechRequest,
    },
    routing::admission::AdmissionPermit,
    server::{Authenticated, ClientState},
};

pub(super) async fn speech(
    auth: Authenticated,
    State(state): State<ClientState>,
    request: Request,
) -> Response {
    let (inbound, request_id) = try_step!(dispatch::inbound(&state, &request));
    let max_body_bytes = state.max_body_bytes();
    let (_reservation, bytes) = try_step!(inbound.read_json(&state, request, max_body_bytes).await);
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
    let (route, adapter) = try_step!(inbound.resolve(&state, &auth, &selector));
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
    let routed = try_step!(dispatch::routed(
        route,
        request_id,
        Extensions::default(),
        request
    ));
    let execution = inbound.execute(&auth, &state, route, &*adapter, routed);
    let (permit, result) = try_step!(execution.await);
    let observer = inbound.observer();
    match result {
        Ok(AdapterOutput::Complete(CoreResponse::Speech(response)))
            if response.valid_for(&speech) =>
        {
            if let Some(observer) = observer {
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
        Ok(_) => upstream_failure_observed(observer),
        Err(error) => gateway_error_observed(error, observer),
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
