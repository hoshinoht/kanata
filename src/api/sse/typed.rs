//! Typed-event SSE bodies (Messages, Responses): `event:` frames carrying JSON, ending in
//! a completion or a single protocol error frame.

use std::{collections::VecDeque, convert::Infallible};

use axum::{
    body::{Body, Bytes},
    http::header,
    response::{IntoResponse, Response},
};
use futures_util::stream;
use serde_json::Value;

use super::{
    super::{
        deadline::RequestDeadline,
        errors::{gateway_error_observed, upstream_failure_observed},
        keepalive::keepalive,
        stream_timeout::StreamTimeout,
    },
    StreamLifetime,
};
use crate::{
    adapter::EventStream,
    core::{
        ErrorKind, FinishReason, GatewayError, ModelAlias, NormalizedEvent, TimeoutPhase, Usage,
    },
};

/// Protocol side of a typed event stream.
pub(in crate::api) trait EventOutput: Send + 'static {
    /// Frame sent during upstream silence.
    const KEEPALIVE: &'static [u8];
    fn accept(&mut self, event: NormalizedEvent) -> Result<Vec<Value>, GatewayError>;
    fn finish(
        &mut self,
        reason: FinishReason,
        usage: Option<&Usage>,
    ) -> Result<Vec<Value>, GatewayError>;
    /// Terminal frame for `error`.
    fn failed(&mut self, error: GatewayError) -> Value;
    fn frame(&mut self, value: Value) -> Bytes {
        event_frame(&value)
    }
}

/// `event:` named by the frame's `type`, then the frame as `data:`.
pub(in crate::api) fn event_frame(value: &Value) -> Bytes {
    format!(
        "event: {}\ndata: {}\n\n",
        value["type"].as_str().expect("event type"),
        value
    )
    .into()
}

/// Commits the stream once the first event names `model`; `start` then yields the output
/// state and its opening frames.
pub(in crate::api) async fn stream_response<O: EventOutput>(
    mut events: EventStream,
    model: ModelAlias,
    start: impl FnOnce(&ModelAlias) -> (O, Vec<Value>),
    deadline: RequestDeadline,
    first_byte_ms: u64,
    idle_ms: u64,
    lifetime: StreamLifetime,
) -> Response {
    let timeout = StreamTimeout::new(deadline, first_byte_ms, idle_ms);
    match timeout.first(&mut events).await {
        Ok(Some(Ok(NormalizedEvent::ChatStarted { model: actual }))) if model == actual => {}
        Err(error) | Ok(Some(Err(error))) => {
            return gateway_error_observed(error, lifetime.observer.as_ref());
        }
        _ => return upstream_failure_observed(lifetime.observer.as_ref()),
    }
    let (output, pending) = start(&model);
    let interval = lifetime.keepalive;
    let state = State {
        events: Some(events),
        output,
        pending: pending.into(),
        timeout,
        lifetime,
        ended: false,
        terminal_error: false,
    };
    let body = stream::unfold(Some(state), |state| async move {
        let mut state = state?;
        loop {
            if !state.terminal_error && state.timeout.expired() {
                state.fail(super::super::deadline::timeout(TimeoutPhase::Overall));
            }
            if let Some(frame) = state.pending.pop_front() {
                let frame = state.output.frame(frame);
                let next = (!(state.ended && state.pending.is_empty())).then_some(state);
                return Some((Ok::<_, Infallible>(frame), next));
            }
            if state.ended {
                return None;
            }
            let next = state
                .timeout
                .next(state.events.as_mut().expect("active stream"))
                .await;
            match next {
                Ok(Some(Ok(NormalizedEvent::ChatCompleted {
                    finish_reason,
                    usage,
                }))) => match state.output.finish(finish_reason, usage.as_ref()) {
                    Ok(frames) => {
                        if let Some(observer) = &state.lifetime.observer {
                            observer.record_usage(usage);
                            observer.end_upstream();
                        }
                        state.pending.extend(frames);
                        state.events = None;
                        state.ended = true;
                    }
                    Err(error) => state.fail(error),
                },
                Ok(Some(Ok(event))) => match state.output.accept(event) {
                    Ok(frames) => {
                        if !frames.is_empty()
                            && let Some(observer) = &state.lifetime.observer
                        {
                            observer.first_content();
                        }
                        state.pending.extend(frames);
                    }
                    Err(error) => state.fail(error),
                },
                Err(error) | Ok(Some(Err(error))) => state.fail(error),
                Ok(None) => state.fail(GatewayError::from(ErrorKind::UpstreamFailure)),
            }
        }
    });
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(keepalive(body, interval, O::KEEPALIVE)),
    )
        .into_response()
}

struct State<O> {
    events: Option<EventStream>,
    output: O,
    pending: VecDeque<Value>,
    timeout: StreamTimeout,
    lifetime: StreamLifetime,
    ended: bool,
    terminal_error: bool,
}

impl<O: EventOutput> State<O> {
    fn fail(&mut self, error: GatewayError) {
        if let Some(observer) = &self.lifetime.observer {
            observer.record_error(error);
            observer.end_upstream();
        }
        self.events = None;
        self.pending.clear();
        self.pending.push_back(self.output.failed(error));
        self.ended = true;
        self.terminal_error = true;
    }
}
