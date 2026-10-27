use serde::de::DeserializeOwned;

use crate::{
    core::{FinishReason, GatewayError, ModelAlias, NormalizedEvent, Usage},
    upstream_failure,
};

use super::{
    MAX_TOOL_CALLS,
    response::parse_finish_reason,
    stream_wire::{self, StreamChoice, StreamChunk, ToolDelta},
    valid_call_id, valid_tool_name,
};

const MAX_ARGUMENT_DELTA_BYTES: usize = 16 * 1024;

/// A provider's chunk envelope and the rules its stream follows.
pub(crate) trait StreamDialect: Default + Unpin {
    /// One `data` record as sent.
    type Chunk: DeserializeOwned;
    type Usage: Into<Usage>;
    /// A trailing usage chunk may repeat one empty choice (LiteLLM).
    const TRAILING_EMPTY_CHOICE: bool;
    /// The final usage may be repeated if identical.
    const REPEATED_USAGE: bool;
    /// `[DONE]` needs a usage chunk before it.
    const REQUIRES_USAGE: bool = false;
    /// Only a `tool_calls` finish may follow tool calls.
    const EXACT_TOOL_FINISH: bool = false;
    /// A tool-call continuation may carry no id, type or function.
    const EMPTY_TOOL_CONTINUATION: bool = false;

    /// Checks one chunk's envelope and returns its choices and usage.
    fn chunk(&mut self, chunk: Self::Chunk) -> Result<StreamChunk<Self::Usage>, GatewayError>;
}

struct ToolState {
    id: String,
    name: String,
}

/// Tool calls streamed as deltas: a new call carries its id, `function` type and name at
/// the next free index; later deltas append arguments.
pub(crate) struct ToolCalls {
    tools: Vec<ToolState>,
    allow_empty_continuation: bool,
}

impl ToolCalls {
    /// `allow_empty_continuation` accepts a continuation delta with no id, type or function.
    pub(crate) fn new(allow_empty_continuation: bool) -> Self {
        Self {
            tools: Vec::new(),
            allow_empty_continuation,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub(crate) fn delta(&mut self, call: ToolDelta) -> Result<NormalizedEvent, GatewayError> {
        let index = usize::try_from(call.index).map_err(|_| upstream_failure())?;
        let function = call.function;
        let arguments = function
            .as_ref()
            .and_then(|function| function.arguments.clone())
            .unwrap_or_default();
        if index >= MAX_TOOL_CALLS || arguments.len() > MAX_ARGUMENT_DELTA_BYTES {
            return Err(upstream_failure());
        }
        if let Some(existing) = self.tools.get(index) {
            if call.kind.as_deref().is_some_and(|kind| kind != "function")
                || call.id.as_deref().is_some_and(|id| id != existing.id)
                || function
                    .as_ref()
                    .and_then(|function| function.name.as_deref())
                    .is_some_and(|name| name != existing.name)
                || (!self.allow_empty_continuation
                    && call.id.is_none()
                    && call.kind.is_none()
                    && function.is_none())
            {
                return Err(upstream_failure());
            }
            return Ok(NormalizedEvent::ChatToolCallDelta {
                call_id: existing.id.clone(),
                name: None,
                arguments_delta: arguments,
            });
        }
        let (Some(id), Some("function"), Some(name)) = (
            call.id,
            call.kind.as_deref(),
            function.and_then(|function| function.name),
        ) else {
            return Err(upstream_failure());
        };
        if index != self.tools.len()
            || !valid_call_id(&id)
            || !valid_tool_name(&name)
            || self.tools.iter().any(|tool| tool.id == id)
        {
            return Err(upstream_failure());
        }
        self.tools.push(ToolState {
            id: id.clone(),
            name: name.clone(),
        });
        Ok(NormalizedEvent::ChatToolCallDelta {
            call_id: id,
            name: Some(name),
            arguments_delta: arguments,
        })
    }
}

/// Validates a lenient chunk stream and maps it to normalized events.
pub(crate) struct StreamState<D> {
    public_model: ModelAlias,
    started: bool,
    has_visible_text: bool,
    finished: Option<FinishReason>,
    usage: Option<Usage>,
    tools: ToolCalls,
    done: bool,
    dialect: D,
}

impl<D: StreamDialect> StreamState<D> {
    pub(crate) fn new(public_model: ModelAlias) -> Self {
        Self {
            public_model,
            started: false,
            has_visible_text: false,
            finished: None,
            usage: None,
            tools: ToolCalls::new(D::EMPTY_TOOL_CONTINUATION),
            done: false,
            dialect: D::default(),
        }
    }

    pub(crate) fn data(&mut self, data: &str) -> Result<Vec<NormalizedEvent>, GatewayError> {
        if self.done {
            return Err(upstream_failure());
        }
        let chunk = self.dialect.chunk(stream_wire::parse(data)?)?;
        self.chunk(chunk)
    }

    pub(crate) fn done(&mut self) -> Result<NormalizedEvent, GatewayError> {
        if self.done {
            return Err(upstream_failure());
        }
        let finish_reason = self.finished.ok_or_else(upstream_failure)?;
        if D::REQUIRES_USAGE && self.usage.is_none() {
            return Err(upstream_failure());
        }
        self.done = true;
        Ok(NormalizedEvent::ChatCompleted {
            finish_reason,
            usage: self.usage.take(),
        })
    }

    /// Ends the stream early with `reason`, even before any visible output.
    pub(crate) fn interrupt(
        &mut self,
        reason: FinishReason,
    ) -> Result<Vec<NormalizedEvent>, GatewayError> {
        if self.done || self.finished.is_some() {
            return Err(upstream_failure());
        }
        let mut events = Vec::new();
        self.start(&mut events);
        self.done = true;
        events.push(NormalizedEvent::ChatCompleted {
            finish_reason: reason,
            usage: None,
        });
        Ok(events)
    }

    fn chunk(
        &mut self,
        chunk: StreamChunk<D::Usage>,
    ) -> Result<Vec<NormalizedEvent>, GatewayError> {
        if self.finished.is_some() {
            if !(chunk.choices.is_empty()
                || (D::TRAILING_EMPTY_CHOICE
                    && matches!(chunk.choices.as_slice(), [choice] if is_empty_choice(choice))))
            {
                return Err(upstream_failure());
            }
            let usage = chunk.usage.ok_or_else(upstream_failure)?;
            self.set_usage(usage.into())?;
            return Ok(Vec::new());
        }
        if chunk.choices.len() != 1 {
            return Err(upstream_failure());
        }
        let mut choice = chunk
            .choices
            .into_iter()
            .next()
            .ok_or_else(upstream_failure)?;
        if choice.index != 0 {
            return Err(upstream_failure());
        }
        let has_reasoning = choice.delta.has_reasoning();
        let reasoning = choice.delta.take_reasoning();
        let mut events = Vec::new();
        if let Some(role) = choice.delta.role.as_deref() {
            if role != "assistant" {
                return Err(upstream_failure());
            }
            self.start(&mut events);
        }
        if let Some(text) = reasoning {
            self.start(&mut events);
            events.push(NormalizedEvent::ChatReasoningDelta { text });
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
                events.push(self.tools.delta(call)?);
            }
        }
        if let Some(reason) = choice.finish_reason {
            self.finish(&reason)?;
            // A stop or length stop may carry no visible output.
            self.start(&mut events);
        }
        if let Some(usage) = chunk.usage {
            if self.finished.is_none() {
                return Err(upstream_failure());
            }
            self.set_usage(usage.into())?;
        }
        if !self.started && !has_reasoning && self.finished.is_some() {
            return Err(upstream_failure());
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

    fn finish(&mut self, value: &str) -> Result<(), GatewayError> {
        if self.finished.is_some() {
            return Err(upstream_failure());
        }
        let reason = parse_finish_reason(value)?;
        let has_tools = !self.tools.is_empty();
        if (reason == FinishReason::Stop && has_tools)
            || (reason == FinishReason::ToolCalls && !has_tools)
            || (D::EXACT_TOOL_FINISH && has_tools && reason != FinishReason::ToolCalls)
        {
            return Err(upstream_failure());
        }
        if reason == FinishReason::ContentFilter
            && (!self.started || (!has_tools && !self.has_visible_text))
        {
            return Err(upstream_failure());
        }
        self.finished = Some(reason);
        Ok(())
    }

    fn set_usage(&mut self, usage: Usage) -> Result<(), GatewayError> {
        match &self.usage {
            Some(existing) if !D::REPEATED_USAGE || *existing != usage => Err(upstream_failure()),
            _ => {
                self.usage = Some(usage);
                Ok(())
            }
        }
    }
}

fn is_empty_choice(choice: &StreamChoice) -> bool {
    choice.index == 0
        && choice.finish_reason.is_none()
        && choice.delta.role.is_none()
        && choice.delta.content.as_deref().is_none_or(str::is_empty)
        && choice.delta.tool_calls.as_ref().is_none_or(Vec::is_empty)
}

#[cfg(test)]
mod tests {
    use crate::core::NormalizedEvent;

    use super::{ToolCalls, stream_wire};

    fn delta(json: &str) -> stream_wire::ToolDelta {
        stream_wire::parse(json).expect("tool delta")
    }

    #[test]
    fn empty_continuation_is_accepted_only_when_allowed() {
        let opening = r#"{"index":0,"id":"call-1","type":"function","function":{"name":"f"}}"#;
        let empty = r#"{"index":0}"#;
        for allow in [false, true] {
            let mut calls = ToolCalls::new(allow);
            assert!(matches!(
                calls.delta(delta(opening)),
                Ok(NormalizedEvent::ChatToolCallDelta { name: Some(_), .. })
            ));
            assert_eq!(calls.delta(delta(empty)).is_ok(), allow);
        }
    }
}
