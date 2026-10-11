mod encoding;
pub(super) mod state;

use std::time::Duration;

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

pub(super) struct StreamOutput {
    pub(super) include_usage: bool,
    pub(super) expose_reasoning: bool,
}

/// What a committed stream body owns until its terminal frame.
pub(super) struct StreamLifetime {
    pub(super) permit: AdmissionPermit,
    pub(super) observer: Option<Observer>,
    /// Maximum body silence before a keepalive frame.
    pub(super) keepalive: Duration,
}

impl StreamLifetime {
    pub(super) fn new(
        permit: AdmissionPermit,
        observer: Option<Observer>,
        keepalive_ms: u64,
    ) -> Self {
        Self {
            permit,
            observer,
            keepalive: Duration::from_millis(keepalive_ms),
        }
    }
}

pub(super) async fn stream_response(
    mut events: EventStream,
    model: ModelAlias,
    output: StreamOutput,
    deadline: RequestDeadline,
    first_byte_ms: u64,
    idle_ms: u64,
    lifetime: StreamLifetime,
) -> Response {
    let StreamLifetime {
        permit,
        observer,
        keepalive,
    } = lifetime;
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
        output,
        timeout,
        observer,
    );
    let body = lifecycle::stream_body(state, permit, keepalive);
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}
