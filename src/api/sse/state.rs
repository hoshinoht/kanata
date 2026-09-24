use std::collections::BTreeMap;
use std::convert::Infallible;

use crate::adapter::EventStream;
use crate::core::{ErrorKind, FinishReason, GatewayError, NormalizedEvent, TimeoutPhase, Usage};
use axum::body::Bytes;

use super::super::super::telemetry::Observer;
use super::super::serialization::{finish, finish_matches_tool_calls};
use super::super::stream_timeout::StreamTimeout;
use super::encoding::{self, Metadata};

const MAX_TOOL_CALLS: usize = 64;
const MAX_TOOL_CALL_ID_BYTES: usize = 128;
const MAX_TOOL_NAME_BYTES: usize = 64;
const MAX_ARGUMENTS_DELTA_BYTES: usize = 16 * 1024;

pub(in crate::api) struct StreamState {
    events: Option<EventStream>,
    metadata: Metadata,
    include_usage: bool,
    phase: Phase,
    timeout: StreamTimeout,
    terminal_error: bool,
    tools: BTreeMap<String, ToolCall>,
    observer: Option<Observer>,
}

enum Phase {
    Start,
    Events,
    Usage(Usage),
    Done,
    End,
}

struct ToolCall {
    index: usize,
    name: String,
}

impl StreamState {
    pub(in crate::api) fn new(
        events: EventStream,
        metadata: Metadata,
        include_usage: bool,
        timeout: StreamTimeout,
        observer: Option<Observer>,
    ) -> Self {
        Self {
            events: Some(events),
            metadata,
            include_usage,
            phase: Phase::Start,
            timeout,
            terminal_error: false,
            tools: BTreeMap::new(),
            observer,
        }
    }

    pub(in crate::api) async fn next(mut self) -> Option<(Result<Bytes, Infallible>, Self)> {
        if self.timeout.expired() {
            let frame = self.fail(super::super::deadline::timeout(TimeoutPhase::Overall));
            return Some((Ok(frame), self));
        }
        let frame = match self.phase {
            Phase::Start => {
                self.phase = Phase::Events;
                encoding::start(&self.metadata)
            }
            Phase::Events => return self.next_event().await,
            Phase::Usage(ref usage) => {
                let usage = usage.clone();
                self.phase = Phase::Done;
                encoding::usage(&self.metadata, usage)
            }
            Phase::Done => {
                self.phase = Phase::End;
                encoding::done()
            }
            Phase::End => return None,
        };
        Some((Ok(frame), self))
    }

    async fn next_event(mut self) -> Option<(Result<Bytes, Infallible>, Self)> {
        let next = match self.events.as_mut() {
            Some(events) => self.timeout.next(events).await,
            None => return None,
        };
        let frame = match next {
            Ok(Some(Ok(event))) => match self.encode_event(event) {
                Ok(frame) => frame,
                Err(error) => self.fail(error),
            },
            Ok(Some(Err(error))) => self.fail(error),
            Ok(None) => self.fail(GatewayError {
                kind: ErrorKind::UpstreamFailure,
            }),
            Err(error) => self.fail(error),
        };
        Some((Ok(frame), self))
    }

    fn encode_event(&mut self, event: NormalizedEvent) -> Result<Bytes, GatewayError> {
        match event {
            NormalizedEvent::ChatTextDelta { text } => Ok(encoding::text(&self.metadata, text)),
            NormalizedEvent::ChatToolCallDelta {
                call_id,
                name,
                arguments_delta,
            } => self.tool_call(call_id, name, arguments_delta),
            NormalizedEvent::ChatCompleted {
                finish_reason,
                usage,
            } => self.completed(finish_reason, usage),
            NormalizedEvent::ChatStarted { .. } => Err(upstream_failure()),
        }
    }

    fn tool_call(
        &mut self,
        call_id: String,
        name: Option<String>,
        arguments: String,
    ) -> Result<Bytes, GatewayError> {
        if !valid_call_id(&call_id) || arguments.len() > MAX_ARGUMENTS_DELTA_BYTES {
            return Err(upstream_failure());
        }

        if let Some(call) = self.tools.get(&call_id) {
            if name
                .as_deref()
                .is_some_and(|value| value != call.name.as_str())
            {
                return Err(upstream_failure());
            }
            return Ok(encoding::tool_continuation(
                &self.metadata,
                call.index,
                arguments,
            ));
        }

        let Some(name) = name else {
            return Err(upstream_failure());
        };
        if !valid_tool_name(&name) || self.tools.len() >= MAX_TOOL_CALLS {
            return Err(upstream_failure());
        }

        let index = self.tools.len();
        self.tools.insert(
            call_id.clone(),
            ToolCall {
                index,
                name: name.clone(),
            },
        );
        Ok(encoding::tool_initial(
            &self.metadata,
            index,
            call_id,
            name,
            arguments,
        ))
    }

    fn completed(
        &mut self,
        finish_reason: FinishReason,
        usage: Option<Usage>,
    ) -> Result<Bytes, GatewayError> {
        if !finish_matches_tool_calls(finish_reason, !self.tools.is_empty()) {
            return Err(upstream_failure());
        }
        let Some(reason) = finish(finish_reason) else {
            return Err(GatewayError {
                kind: ErrorKind::Cancelled,
            });
        };
        self.events = None;
        self.phase = if self.include_usage {
            usage.map_or(Phase::Done, Phase::Usage)
        } else {
            Phase::Done
        };
        Ok(encoding::finish(&self.metadata, reason))
    }

    fn fail(&mut self, error: GatewayError) -> Bytes {
        if let Some(observer) = self.observer.as_ref() {
            observer.record_error(error);
        }
        self.events = None;
        self.phase = Phase::End;
        self.terminal_error = true;
        encoding::error(error)
    }

    pub(in crate::api) fn releases_permit_after_frame(&self) -> bool {
        self.terminal_error
    }
}

fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}

fn valid_call_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOOL_CALL_ID_BYTES
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn valid_tool_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOOL_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}
