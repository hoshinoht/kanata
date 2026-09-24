use std::collections::BTreeMap;

use crate::core::{ErrorKind, FinishReason, GatewayError, ModelAlias, NormalizedEvent, Usage};

use super::{
    response::{self, MAX_TOOL_CALLS, parse_finish_reason, valid_call_id, valid_tool_name},
    stream_wire::{self, StreamChoice, StreamChunk, ToolDelta},
};

const MAX_ARGUMENT_DELTA_BYTES: usize = 16 * 1024;

struct ToolState {
    id: String,
    name: String,
}

#[derive(Clone)]
struct FinishState {
    value: String,
    reason: FinishReason,
    native_reason: Option<String>,
}

pub(super) struct StreamState {
    public_model: ModelAlias,
    started: bool,
    has_text: bool,
    tools: BTreeMap<usize, ToolState>,
    finish: Option<FinishState>,
    usage: Option<Usage>,
    done: bool,
}

impl StreamState {
    pub(super) fn new(public_model: ModelAlias) -> Self {
        Self {
            public_model,
            started: false,
            has_text: false,
            tools: BTreeMap::new(),
            finish: None,
            usage: None,
            done: false,
        }
    }

    pub(super) fn data(&mut self, data: &str) -> Result<Vec<NormalizedEvent>, GatewayError> {
        if self.done {
            return Err(upstream_failure());
        }
        let chunk = stream_wire::parse(data)?;
        self.validate_chunk(&chunk)?;
        if chunk.error.is_some() {
            return Err(upstream_failure());
        }
        self.chunk(chunk)
    }

    pub(super) fn done(&mut self) -> Result<NormalizedEvent, GatewayError> {
        let finish = self.finish.as_ref().ok_or_else(upstream_failure)?;
        if self.done || !self.has_output(finish.reason) {
            return Err(upstream_failure());
        }
        let usage = self.usage.take().ok_or_else(upstream_failure)?;
        self.done = true;
        Ok(NormalizedEvent::ChatCompleted {
            finish_reason: finish.reason,
            usage: Some(usage),
        })
    }

    pub(super) fn eof(&self) -> GatewayError {
        upstream_failure()
    }

    fn validate_chunk(&self, chunk: &StreamChunk) -> Result<(), GatewayError> {
        if chunk.id.is_empty()
            || chunk.object != "chat.completion.chunk"
            || chunk.model.trim().is_empty()
            || chunk.provider.as_ref().is_some_and(String::is_empty)
            || chunk
                .system_fingerprint
                .as_ref()
                .is_some_and(String::is_empty)
            || chunk.service_tier.as_ref().is_some_and(String::is_empty)
        {
            return Err(upstream_failure());
        }
        Ok(())
    }

    fn chunk(&mut self, chunk: StreamChunk) -> Result<Vec<NormalizedEvent>, GatewayError> {
        if let Some(finish) = self.finish.clone() {
            return self.accounting_chunk(chunk, &finish);
        }
        if chunk.usage.is_some() || chunk.choices.len() != 1 {
            return Err(upstream_failure());
        }
        let choice = chunk
            .choices
            .into_iter()
            .next()
            .ok_or_else(upstream_failure)?;
        validate_choice(&choice)?;
        if choice.finish_reason.is_none() && choice.native_finish_reason.is_some() {
            return Err(upstream_failure());
        }

        let mut events = Vec::new();
        let mut meaningful = false;
        if let Some(role) = choice.delta.role.as_deref() {
            if role != "assistant" {
                return Err(upstream_failure());
            }
            meaningful = true;
            self.start(&mut events);
        }
        if choice.delta.reasoning.is_some() || choice.delta.reasoning_details.is_some() {
            meaningful = true;
        }
        if let Some(text) = choice.delta.content.filter(|text| !text.is_empty()) {
            meaningful = true;
            self.has_text = true;
            self.start(&mut events);
            events.push(NormalizedEvent::ChatTextDelta { text });
        }
        for call in choice.delta.tool_calls.unwrap_or_default() {
            meaningful = true;
            self.start(&mut events);
            events.push(self.tool(call)?);
        }
        if let Some(value) = choice.finish_reason {
            meaningful = true;
            let reason = parse_finish_reason(&value)?;
            if (reason == FinishReason::ToolCalls) != !self.tools.is_empty()
                || !self.has_output(reason)
            {
                return Err(upstream_failure());
            }
            // A length stop may carry no visible output.
            self.start(&mut events);
            self.finish = Some(FinishState {
                value,
                reason,
                native_reason: choice.native_finish_reason,
            });
        }
        if !meaningful {
            return Err(upstream_failure());
        }
        Ok(events)
    }

    fn accounting_chunk(
        &mut self,
        chunk: StreamChunk,
        finish: &FinishState,
    ) -> Result<Vec<NormalizedEvent>, GatewayError> {
        if self.usage.is_some() || chunk.choices.len() != 1 {
            return Err(upstream_failure());
        }
        let usage = chunk.usage.ok_or_else(upstream_failure)?;
        let choice = chunk
            .choices
            .into_iter()
            .next()
            .ok_or_else(upstream_failure)?;
        validate_choice(&choice)?;
        if choice.finish_reason.as_deref() != Some(finish.value.as_str())
            || choice.native_finish_reason != finish.native_reason
            || choice
                .delta
                .role
                .as_deref()
                .is_some_and(|role| role != "assistant")
            || choice
                .delta
                .content
                .as_deref()
                .is_some_and(|content| !content.is_empty())
            || choice
                .delta
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
        {
            return Err(upstream_failure());
        }
        self.usage = Some(response::normalize_usage(usage)?);
        Ok(Vec::new())
    }

    /// A length stop may end with no output; other stops need text or tool calls.
    fn has_output(&self, reason: FinishReason) -> bool {
        reason == FinishReason::Length || self.has_text || !self.tools.is_empty()
    }

    fn tool(&mut self, call: ToolDelta) -> Result<NormalizedEvent, GatewayError> {
        let index = usize::try_from(call.index).map_err(|_| upstream_failure())?;
        let function = call.function;
        let arguments = function
            .as_ref()
            .and_then(|function| function.arguments.clone())
            .unwrap_or_default();
        if index >= MAX_TOOL_CALLS || arguments.len() > MAX_ARGUMENT_DELTA_BYTES {
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
            return Ok(NormalizedEvent::ChatToolCallDelta {
                call_id: existing.id.clone(),
                name: None,
                arguments_delta: arguments,
            });
        }
        // A new call carries its id, type and name, at the next free index.
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
            || self.tools.values().any(|tool| tool.id == id)
        {
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

    fn start(&mut self, events: &mut Vec<NormalizedEvent>) {
        if !self.started {
            self.started = true;
            events.push(NormalizedEvent::ChatStarted {
                model: self.public_model.clone(),
            });
        }
    }
}

fn validate_choice(choice: &StreamChoice) -> Result<(), GatewayError> {
    if choice.index != 0
        || choice
            .native_finish_reason
            .as_ref()
            .is_some_and(String::is_empty)
    {
        return Err(upstream_failure());
    }
    Ok(())
}

fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}

#[cfg(test)]
mod tests {
    use crate::core::{FinishReason, GatewayError, ModelAlias, NormalizedEvent, Usage};

    use super::StreamState;

    const TOOL_CALL_STREAM: &str =
        include_str!("../../../tests/fixtures/openrouter/chat-stream-tool-call.sse");

    fn run(sse: &str) -> Result<Vec<NormalizedEvent>, GatewayError> {
        let mut state = StreamState::new(ModelAlias("voxtral".into()));
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
    fn tool_call_stream_with_repeated_finish_and_usage() {
        let events = run(TOOL_CALL_STREAM).expect("stream");
        assert!(matches!(
            events.first(),
            Some(NormalizedEvent::ChatStarted { .. })
        ));
        let arguments: String = events
            .iter()
            .filter_map(|event| match event {
                NormalizedEvent::ChatToolCallDelta {
                    call_id,
                    arguments_delta,
                    ..
                } if call_id == "call-fixture" => Some(arguments_delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(arguments, r#"{"city": "Singapore"}"#);
        assert!(events.iter().any(|event| matches!(
            event,
            NormalizedEvent::ChatToolCallDelta { name: Some(name), .. } if name == "get_weather"
        )));
        assert_eq!(
            events.last(),
            Some(&NormalizedEvent::ChatCompleted {
                finish_reason: FinishReason::ToolCalls,
                usage: Some(Usage {
                    input_tokens: 71,
                    output_tokens: 23,
                    total_tokens: 94,
                }),
            })
        );
    }

    #[test]
    fn stop_after_tool_calls_is_rejected() {
        let stop = TOOL_CALL_STREAM.replace(
            r#""finish_reason":"tool_calls","native_finish_reason":"tool_calls""#,
            r#""finish_reason":"stop","native_finish_reason":"stop""#,
        );
        assert!(run(&stop).is_err());
    }
}
