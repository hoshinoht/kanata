use std::{
    collections::{BTreeMap, VecDeque},
    convert::Infallible,
};

use axum::{
    body::{Body, Bytes},
    http::header,
    response::{IntoResponse, Response},
};
use futures_util::stream;
use serde_json::{Value, json};

use super::{
    super::{
        deadline::RequestDeadline,
        errors::{gateway_error_observed, messages_sse_error, upstream_failure_observed},
        keepalive::keepalive,
        sse::StreamLifetime,
        stream_timeout::StreamTimeout,
    },
    output::{failure, stop_reason, tool_input, usage, valid_call},
};
use crate::{
    adapter::EventStream,
    core::{FinishReason, GatewayError, ModelAlias, NormalizedEvent, TimeoutPhase, Usage},
};

/// Keepalive frame of the Messages event stream.
const PING: &[u8] = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";
/// Bytes per streamed argument delta.
const MAX_ARGUMENT_DELTA_BYTES: usize = 16 * 1024;
/// Argument bytes kept per response to check each call's input at the end.
const MAX_ARGUMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOOL_CALLS: usize = 64;

pub(in crate::api) async fn stream_response(
    mut events: EventStream,
    model: ModelAlias,
    id: String,
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
    // Counts are known only at completion; `message_delta` carries the real usage.
    let start = json!({"type":"message_start","message":{
        "id":id,"type":"message","role":"assistant","model":model.0,"content":[],
        "stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}
    }});
    let interval = lifetime.keepalive;
    let state = State {
        events: Some(events),
        output: Output::default(),
        pending: VecDeque::from([start]),
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
                let next = (!(state.ended && state.pending.is_empty())).then_some(state);
                return Some((Ok::<_, Infallible>(frame_bytes(&frame)), next));
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
                Ok(None) => state.fail(failure()),
            }
        }
    });
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(keepalive(body, interval, PING)),
    )
        .into_response()
}

fn frame_bytes(frame: &Value) -> Bytes {
    format!(
        "event: {}\ndata: {}\n\n",
        frame["type"].as_str().expect("event type"),
        frame
    )
    .into()
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
        self.pending.push_back(messages_sse_error(error));
        self.ended = true;
        self.terminal_error = true;
    }
}

/// Content blocks are sequential: a new block closes the open one, and a closed tool
/// call cannot be resumed.
#[derive(Default)]
struct Output {
    next_index: usize,
    open: Option<Open>,
    /// Arguments per call id, checked as a JSON object when the message ends.
    calls: BTreeMap<String, String>,
    argument_bytes: usize,
}

enum Open {
    Text(usize),
    Tool(usize, String),
}

impl Output {
    fn accept(&mut self, event: NormalizedEvent) -> Result<Vec<Value>, GatewayError> {
        let mut frames = Vec::new();
        match event {
            NormalizedEvent::ChatTextDelta { text } => {
                if text.is_empty() {
                    return Ok(frames);
                }
                let index = match self.open {
                    Some(Open::Text(index)) => index,
                    _ => self.open_block(&mut frames, |index| {
                        (Open::Text(index), json!({"type":"text","text":""}))
                    }),
                };
                frames.push(json!({"type":"content_block_delta","index":index,
                    "delta":{"type":"text_delta","text":text}}));
            }
            NormalizedEvent::ChatToolCallDelta {
                call_id,
                name,
                arguments_delta,
            } => {
                if arguments_delta.len() > MAX_ARGUMENT_DELTA_BYTES {
                    return Err(failure());
                }
                let index = match &self.open {
                    Some(Open::Tool(index, open)) if *open == call_id => {
                        if name.is_some_and(|name| name.is_empty()) {
                            return Err(failure());
                        }
                        *index
                    }
                    _ => {
                        let name = name.ok_or_else(failure)?;
                        if !valid_call(&call_id, &name)
                            || self.calls.contains_key(&call_id)
                            || self.calls.len() >= MAX_TOOL_CALLS
                        {
                            return Err(failure());
                        }
                        self.calls.insert(call_id.clone(), String::new());
                        self.open_block(&mut frames, |index| {
                            (
                                Open::Tool(index, call_id.clone()),
                                json!({"type":"tool_use","id":call_id,"name":name,"input":{}}),
                            )
                        })
                    }
                };
                if !arguments_delta.is_empty() {
                    self.argument_bytes = self
                        .argument_bytes
                        .checked_add(arguments_delta.len())
                        .filter(|total| *total <= MAX_ARGUMENT_BYTES)
                        .ok_or_else(failure)?;
                    if let Some(Open::Tool(_, open)) = &self.open
                        && let Some(arguments) = self.calls.get_mut(open)
                    {
                        arguments.push_str(&arguments_delta);
                    }
                    frames.push(json!({"type":"content_block_delta","index":index,
                        "delta":{"type":"input_json_delta","partial_json":arguments_delta}}));
                }
            }
            // Neither reasoning text nor encrypted reasoning is carried by this endpoint.
            NormalizedEvent::ChatReasoningDelta { .. }
            | NormalizedEvent::ChatReasoningItem { .. } => {}
            NormalizedEvent::ChatStarted { .. } | NormalizedEvent::ChatCompleted { .. } => {
                return Err(failure());
            }
        }
        Ok(frames)
    }

    /// Closes the open block and starts the one `start` describes at the next index.
    fn open_block(
        &mut self,
        frames: &mut Vec<Value>,
        start: impl FnOnce(usize) -> (Open, Value),
    ) -> usize {
        self.close(frames);
        let index = self.next_index;
        self.next_index += 1;
        let (open, block) = start(index);
        frames.push(json!({"type":"content_block_start","index":index,"content_block":block}));
        self.open = Some(open);
        index
    }

    fn close(&mut self, frames: &mut Vec<Value>) {
        if let Some(Open::Text(index) | Open::Tool(index, _)) = self.open.take() {
            frames.push(json!({"type":"content_block_stop","index":index}));
        }
    }

    fn finish(
        &mut self,
        reason: FinishReason,
        reported: Option<&Usage>,
    ) -> Result<Vec<Value>, GatewayError> {
        let stop_reason = stop_reason(reason, !self.calls.is_empty())?;
        for arguments in self.calls.values() {
            tool_input(arguments)?;
        }
        let mut frames = Vec::new();
        self.close(&mut frames);
        frames.push(json!({"type":"message_delta",
            "delta":{"stop_reason":stop_reason,"stop_sequence":null},"usage":usage(reported)}));
        frames.push(json!({"type":"message_stop"}));
        Ok(frames)
    }
}
