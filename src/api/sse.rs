mod encoding;
pub(super) mod state;

use crate::adapter::EventStream;
use crate::core::{ModelAlias, NormalizedEvent, TimeoutPhase};
use crate::routing::admission::AdmissionPermit;
use crate::telemetry::Observer;
use axum::{
    http::header,
    response::{IntoResponse, Response},
};

use super::deadline::RequestDeadline;
use super::errors::{gateway_error_observed, upstream_failure_observed};
use super::lifecycle;
use super::stream_timeout::StreamTimeout;

pub(super) struct StreamLifetime {
    pub(super) permit: AdmissionPermit,
    pub(super) observer: Option<Observer>,
}

pub(super) async fn stream_response(
    mut events: EventStream,
    model: ModelAlias,
    include_usage: bool,
    deadline: RequestDeadline,
    first_byte_ms: u64,
    idle_ms: u64,
    lifetime: StreamLifetime,
) -> Response {
    let StreamLifetime { permit, observer } = lifetime;
    let timeout = StreamTimeout::new(deadline, first_byte_ms, idle_ms);
    match timeout.first(&mut events).await {
        Err(error) => return gateway_error_observed(error, observer.as_ref()),
        Ok(Some(Ok(NormalizedEvent::ChatStarted { model: event_model })))
            if event_model == model => {}
        Ok(Some(Err(error))) => return gateway_error_observed(error, observer.as_ref()),
        _ => return upstream_failure_observed(observer.as_ref()),
    }

    if timeout.expired() {
        return gateway_error_observed(
            super::deadline::timeout(TimeoutPhase::Overall),
            observer.as_ref(),
        );
    }

    let state = state::StreamState::new(
        events,
        encoding::Metadata::new(model),
        include_usage,
        timeout,
        observer,
    );
    let body = lifecycle::stream_body(state, permit);
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}
