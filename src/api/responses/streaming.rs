use std::{collections::VecDeque, convert::Infallible};

use axum::{
    body::Body,
    http::header,
    response::{IntoResponse, Response},
};
use futures_util::stream;
use serde_json::Value;

use super::{
    super::{
        deadline::RequestDeadline,
        errors::{gateway_error_observed, upstream_failure_observed},
        sse::StreamLifetime,
        stream_timeout::StreamTimeout,
    },
    output::{Options, Output, failure},
};
use crate::{
    adapter::EventStream,
    core::{GatewayError, ModelAlias, NormalizedEvent, TimeoutPhase},
};

pub(in crate::api) async fn stream_response(
    mut events: EventStream,
    model: ModelAlias,
    options: Options,
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
    let output = Output::new(options);
    let pending = output.start().into();
    let state = State {
        events: Some(events),
        output,
        pending,
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
                }))) => match state.output.finish(finish_reason, usage.clone()) {
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
                Ok(Some(Ok(event))) => match state.output.accept(event, true) {
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
                Ok(None) => state.fail(failure()),
            }
        }
    });
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(body),
    )
        .into_response()
}

struct State {
    events: Option<EventStream>,
    output: Output,
    pending: VecDeque<Value>,
    timeout: StreamTimeout,
    lifetime: StreamLifetime,
    ended: bool,
    terminal_error: bool,
}

impl State {
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
