use std::collections::VecDeque;

use futures_util::{StreamExt, stream};

use crate::{
    adapter::{
        EventStream,
        diagnostics::UpstreamLabel,
        transport::{ResponseBody, STREAM_RESPONSE_BYTES},
    },
    core::{ChatResponse, ErrorKind, GatewayError, ModelAlias, TimeoutPhase},
};

use super::protocol::ResponsesStreamParser;

pub(crate) fn event_stream(
    body: ResponseBody,
    public_model: ModelAlias,
    deadline: tokio::time::Instant,
    label: UpstreamLabel,
) -> EventStream {
    event_stream_with_parser(body, public_model, deadline, label, None)
}

pub(crate) fn event_stream_with_parser(
    body: ResponseBody,
    public_model: ModelAlias,
    deadline: tokio::time::Instant,
    label: UpstreamLabel,
    namespace: Option<&'static str>,
) -> EventStream {
    let state = StreamState {
        label,
        body: Some(body),
        parser: parser(public_model, namespace),
        pending: VecDeque::new(),
        deadline,
        finished: false,
    };

    Box::pin(stream::unfold(state, |mut state| async move {
        loop {
            let remaining = state
                .deadline
                .saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() && (!state.finished || !state.pending.is_empty()) {
                state.body.take();
                state.pending.clear();
                state.finished = true;
                return Some((Err(overall_timeout()), state));
            }
            if let Some(event) = state.pending.pop_front() {
                return Some((Ok(event), state));
            }
            if state.finished {
                return None;
            }
            let next = {
                let body = state.body.as_mut()?;
                tokio::time::timeout(remaining, body.next()).await
            };
            match next {
                Err(_) => {
                    state.body.take();
                    state.finished = true;
                    return Some((Err(overall_timeout()), state));
                }
                Ok(Some(Err(error))) => {
                    state.body.take();
                    state.finished = true;
                    return Some((Err(error), state));
                }
                Ok(Some(Ok(bytes))) => match state.parser.feed(&bytes) {
                    Ok(events) => {
                        state.pending.extend(events);
                        if state.parser.take_chat_response().is_some() {
                            state.body.take();
                            state.finished = true;
                        }
                    }
                    Err(error) => {
                        state.body.take();
                        state.finished = true;
                        report(&state.label, &state.parser);
                        return Some((Err(error.gateway_error()), state));
                    }
                },
                Ok(None) => {
                    state.body.take();
                    state.finished = true;
                    return match state.parser.finish_input() {
                        Ok(()) => None,
                        Err(error) => {
                            report(&state.label, &state.parser);
                            Some((Err(error.gateway_error()), state))
                        }
                    };
                }
            }
        }
    }))
}

pub(crate) async fn collect_response(
    body: ResponseBody,
    public_model: ModelAlias,
    deadline: tokio::time::Instant,
    label: &UpstreamLabel,
) -> Result<ChatResponse, GatewayError> {
    collect_response_with_parser(body, public_model, deadline, label, None).await
}

pub(crate) async fn collect_response_with_parser(
    mut body: ResponseBody,
    public_model: ModelAlias,
    deadline: tokio::time::Instant,
    label: &UpstreamLabel,
    namespace: Option<&'static str>,
) -> Result<ChatResponse, GatewayError> {
    let mut parser = parser(public_model, namespace);
    let mut received = 0usize;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(overall_timeout());
        }
        let next = tokio::time::timeout(remaining, body.next())
            .await
            .map_err(|_| overall_timeout())?;
        match next {
            Some(Err(error)) => return Err(error),
            Some(Ok(bytes)) => {
                received = received
                    .checked_add(bytes.len())
                    .ok_or_else(upstream_failure)?;
                if received > STREAM_RESPONSE_BYTES {
                    return Err(upstream_failure());
                }
                parser.feed(&bytes).map_err(|error| {
                    report(label, &parser);
                    error.gateway_error()
                })?;
                // response.completed is terminal; the connection may stay open.
                if let Some(response) = parser.take_chat_response() {
                    return Ok(response);
                }
            }
            None => {
                parser.finish_input().map_err(|error| {
                    report(label, &parser);
                    error.gateway_error()
                })?;
                return parser.take_chat_response().ok_or_else(upstream_failure);
            }
        }
    }
}

fn report(label: &UpstreamLabel, parser: &ResponsesStreamParser) {
    if let Some(failure) = parser.failure() {
        label.stream(&failure.event, &failure.error);
    }
}

struct StreamState {
    label: UpstreamLabel,
    body: Option<ResponseBody>,
    parser: ResponsesStreamParser,
    pending: VecDeque<crate::core::NormalizedEvent>,
    deadline: tokio::time::Instant,
    finished: bool,
}

fn overall_timeout() -> GatewayError {
    GatewayError {
        kind: ErrorKind::Timeout {
            phase: TimeoutPhase::Overall,
        },
    }
}

fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}

fn parser(model: ModelAlias, namespace: Option<&'static str>) -> ResponsesStreamParser {
    match namespace {
        Some(namespace) => ResponsesStreamParser::namespaced(model, namespace),
        None => ResponsesStreamParser::new(model),
    }
}
