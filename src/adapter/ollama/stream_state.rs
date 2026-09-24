use std::collections::BTreeMap;

use crate::core::{ErrorKind, FinishReason, GatewayError, ModelAlias, NormalizedEvent, Usage};

use super::stream_wire::{self, StreamChunk, ToolDelta};

const MAX_TOOL_CALLS: usize = 64;
const MAX_TOOL_CALL_ID_BYTES: usize = 128;
const MAX_TOOL_NAME_BYTES: usize = 64;
const MAX_ARGUMENT_DELTA_BYTES: usize = 16 * 1024;

struct ToolState {
    id: String,
    name: String,
}

pub(super) struct StreamState {
    public_model: ModelAlias,
    started: bool,
    has_visible_text: bool,
    finished: Option<FinishReason>,
    usage: Option<Usage>,
    tools: BTreeMap<usize, ToolState>,
    done: bool,
}

impl StreamState {
    pub(super) fn new(public_model: ModelAlias) -> Self {
        Self {
            public_model,
            started: false,
            has_visible_text: false,
            finished: None,
            usage: None,
            tools: BTreeMap::new(),
            done: false,
        }
    }

    pub(super) fn data(&mut self, data: &str) -> Result<Vec<NormalizedEvent>, GatewayError> {
        if self.done {
            return Err(upstream_failure());
        }
        let chunk = stream_wire::parse(data)?;
        self.chunk(chunk)
    }

    pub(super) fn done(&mut self) -> Result<NormalizedEvent, GatewayError> {
        if self.done || self.finished.is_none() {
            return Err(upstream_failure());
        }
        self.done = true;
        Ok(NormalizedEvent::ChatCompleted {
            finish_reason: self.finished.ok_or_else(upstream_failure)?,
            usage: self.usage.take(),
        })
    }

    pub(super) fn eof(&self) -> GatewayError {
        upstream_failure()
    }

    fn chunk(&mut self, chunk: StreamChunk) -> Result<Vec<NormalizedEvent>, GatewayError> {
        if self.finished.is_some() {
            if !chunk.choices.is_empty() {
                return Err(upstream_failure());
            }
            let usage = chunk.usage.ok_or_else(upstream_failure)?;
            self.set_usage(usage.into_usage())?;
            return Ok(Vec::new());
        }
        if chunk.choices.is_empty() {
            return Err(upstream_failure());
        }
        if chunk.choices.len() != 1 {
            return Err(upstream_failure());
        }
        let choice = chunk
            .choices
            .into_iter()
            .next()
            .ok_or_else(upstream_failure)?;
        if choice.index != 0 {
            return Err(upstream_failure());
        }
        let mut events = Vec::new();
        if let Some(role) = choice.delta.role.as_deref() {
            if role != "assistant" {
                return Err(upstream_failure());
            }
            self.start(&mut events);
        }
        if choice
            .delta
            .content
            .as_deref()
            .is_some_and(|text| !text.is_empty())
            || choice
                .delta
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
        {
            self.start(&mut events);
        }
        if let Some(text) = choice.delta.content
            && !text.is_empty()
        {
            self.has_visible_text = true;
            events.push(NormalizedEvent::ChatTextDelta { text });
        }
        if let Some(calls) = choice.delta.tool_calls {
            for call in calls {
                events.push(self.tool(call)?);
            }
        }
        if let Some(reason) = choice.finish_reason {
            self.finish(reason)?;
        }
        if let Some(usage) = chunk.usage {
            if self.finished.is_none() {
                return Err(upstream_failure());
            }
            self.set_usage(usage.into_usage())?;
        }
        if !self.started {
            if choice.delta.reasoning_content.is_some() {
                return Ok(events);
            }
            if self.finished.is_some() {
                return Err(upstream_failure());
            }
        }
        Ok(events)
    }

    fn start(&mut self, events: &mut Vec<NormalizedEvent>) {
        if !self.started {
            self.started = true;
            events.push(NormalizedEvent::ChatStarted {
                model: self.public_model.clone(),
            });
        }
    }

    fn tool(&mut self, call: ToolDelta) -> Result<NormalizedEvent, GatewayError> {
        let index = usize::try_from(call.index).map_err(|_| upstream_failure())?;
        if index >= MAX_TOOL_CALLS {
            return Err(upstream_failure());
        }
        let function = call.function;
        let arguments = function
            .as_ref()
            .and_then(|function| function.arguments.clone())
            .unwrap_or_default();
        if arguments.len() > MAX_ARGUMENT_DELTA_BYTES {
            return Err(upstream_failure());
        }
        if let Some(existing) = self.tools.get(&index) {
            if call.kind.as_deref().is_some_and(|kind| kind != "function")
                || call.id.as_deref().is_some_and(|id| id != existing.id)
                || function
                    .as_ref()
                    .and_then(|function| function.name.as_deref())
                    .is_some_and(|name| name != existing.name)
            {
                return Err(upstream_failure());
            }
            if call.id.is_none() && call.kind.is_none() && function.is_none() {
                return Err(upstream_failure());
            }
            return Ok(NormalizedEvent::ChatToolCallDelta {
                call_id: existing.id.clone(),
                name: None,
                arguments_delta: arguments,
            });
        }
        let Some(id) = call.id else {
            return Err(upstream_failure());
        };
        let Some(kind) = call.kind else {
            return Err(upstream_failure());
        };
        if kind != "function" || !valid_call_id(&id) {
            return Err(upstream_failure());
        }
        if self.tools.values().any(|tool| tool.id == id) {
            return Err(upstream_failure());
        }
        let Some(function) = function else {
            return Err(upstream_failure());
        };
        let Some(name) = function.name else {
            return Err(upstream_failure());
        };
        if !valid_tool_name(&name) {
            return Err(upstream_failure());
        }
        if index != self.tools.len() {
            return Err(upstream_failure());
        }
        self.tools.insert(
            index,
            ToolState {
                id: id.clone(),
                name: name.clone(),
            },
        );
        Ok(NormalizedEvent::ChatToolCallDelta {
            call_id: id,
            name: Some(name),
            arguments_delta: arguments,
        })
    }

    fn finish(&mut self, value: String) -> Result<(), GatewayError> {
        if self.finished.is_some() {
            return Err(upstream_failure());
        }
        let reason = match value.as_str() {
            "stop" => FinishReason::Stop,
            "length" => FinishReason::Length,
            "tool_calls" => FinishReason::ToolCalls,
            "content_filter" => FinishReason::ContentFilter,
            _ => return Err(upstream_failure()),
        };
        let has_tools = !self.tools.is_empty();
        if (reason == FinishReason::Stop && has_tools)
            || (reason == FinishReason::ToolCalls && !has_tools)
        {
            return Err(upstream_failure());
        }
        if !self.started || (!has_tools && !self.has_visible_text) {
            return Err(upstream_failure());
        }
        self.finished = Some(reason);
        Ok(())
    }

    fn set_usage(&mut self, usage: Usage) -> Result<(), GatewayError> {
        if self.usage.is_some() {
            return Err(upstream_failure());
        }
        self.usage = Some(usage);
        Ok(())
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

fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}
