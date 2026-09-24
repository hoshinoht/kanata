use std::collections::BTreeMap;

use crate::core::{ErrorKind, FinishReason, GatewayError, ModelAlias, NormalizedEvent, Usage};

use super::{
    response::{MAX_TOOL_CALLS, valid_call_id, valid_tool_name},
    stream_wire::{self, StreamChoice, StreamChunk, ToolDelta},
};

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
            // LiteLLM repeats an empty choice on the trailing usage chunk.
            if !(chunk.choices.is_empty()
                || matches!(chunk.choices.as_slice(), [choice] if is_empty_choice(choice)))
            {
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
            // A length stop may carry no visible output.
            self.start(&mut events);
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
        if reason != FinishReason::Length
            && (!self.started || (!has_tools && !self.has_visible_text))
        {
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

fn is_empty_choice(choice: &StreamChoice) -> bool {
    choice.index == 0
        && choice.finish_reason.is_none()
        && choice.delta.role.is_none()
        && choice.delta.content.as_deref().is_none_or(str::is_empty)
        && choice.delta.tool_calls.as_ref().is_none_or(Vec::is_empty)
}

fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}

#[cfg(test)]
mod tests {
    use crate::core::{FinishReason, ModelAlias, NormalizedEvent, Usage};

    use super::StreamState;

    const LITELLM_TOOL_STREAM: &str =
        include_str!("../../../tests/fixtures/vllm/stream-litellm-tool-call.sse");

    fn run(sse: &str) -> Result<Vec<NormalizedEvent>, crate::core::GatewayError> {
        let mut state = StreamState::new(ModelAlias("omni".into()));
        let mut events = Vec::new();
        for data in sse.lines().filter_map(|line| line.strip_prefix("data: ")) {
            if data == "[DONE]" {
                events.push(state.done()?);
            } else {
                events.extend(state.data(data)?);
            }
        }
        Ok(events)
    }

    #[test]
    fn litellm_tool_call_stream_with_trailing_usage_choice() {
        let events = run(LITELLM_TOOL_STREAM).expect("stream");
        let arguments: String = events
            .iter()
            .filter_map(|event| match event {
                NormalizedEvent::ChatToolCallDelta {
                    arguments_delta, ..
                } => Some(arguments_delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(arguments, r#"{"city": "Singapore"}"#);
        assert!(matches!(
            events.first(),
            Some(NormalizedEvent::ChatStarted { .. })
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            NormalizedEvent::ChatToolCallDelta { name: Some(name), .. } if name == "get_weather"
        )));
        assert_eq!(
            events.last(),
            Some(&NormalizedEvent::ChatCompleted {
                finish_reason: FinishReason::ToolCalls,
                usage: Some(Usage {
                    input_tokens: 276,
                    output_tokens: 26,
                    total_tokens: 302,
                }),
            })
        );
    }

    #[test]
    fn content_after_finish_is_rejected() {
        let late = LITELLM_TOOL_STREAM.replace(
            r#""choices":[{"index":0,"delta":{}}],"usage""#,
            r#""choices":[{"index":0,"delta":{"content":"late"}}],"usage""#,
        );
        assert!(run(&late).is_err());
    }
}
