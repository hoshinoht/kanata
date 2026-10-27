use crate::{
    adapter::{
        EventStream,
        chat_completions::{
            stream::ChatStream,
            stream_state::StreamDialect,
            stream_wire::{self as chat_wire, StreamChoice},
        },
        transport::ResponseBody,
        upstream_failure,
    },
    core::{GatewayError, ModelAlias, Usage},
};

use super::stream_wire::{StreamChunk, StreamDelta};

/// Chat-completions SSE from OpenRouter: strict envelopes, a required usage chunk that
/// repeats the finish choice, and no `error` member.
#[derive(Default)]
pub(super) struct OpenRouter {
    /// The finish choice's `finish_reason` and `native_finish_reason`.
    finish: Option<(String, Option<String>)>,
}

impl StreamDialect for OpenRouter {
    type Chunk = StreamChunk;
    type Usage = Usage;
    const TRAILING_EMPTY_CHOICE: bool = false;
    const REPEATED_USAGE: bool = false;
    const REQUIRES_USAGE: bool = true;
    const EXACT_TOOL_FINISH: bool = true;
    const EMPTY_TOOL_CONTINUATION: bool = true;
    const RECORD_AT_A_TIME: bool = true;

    fn chunk(&mut self, chunk: StreamChunk) -> Result<chat_wire::StreamChunk<Usage>, GatewayError> {
        if chunk.id.is_empty()
            || chunk.object != "chat.completion.chunk"
            || chunk.model.trim().is_empty()
            || chunk.provider.as_ref().is_some_and(String::is_empty)
            || chunk
                .system_fingerprint
                .as_ref()
                .is_some_and(String::is_empty)
            || chunk.service_tier.as_ref().is_some_and(String::is_empty)
            || chunk.error.is_some()
            || chunk.choices.len() != 1
        {
            return Err(upstream_failure());
        }
        let usage = chunk.usage.map(|usage| usage.normalize()).transpose()?;
        let choice = chunk
            .choices
            .into_iter()
            .next()
            .ok_or_else(upstream_failure)?;
        if choice.index != 0
            || choice
                .native_finish_reason
                .as_ref()
                .is_some_and(String::is_empty)
        {
            return Err(upstream_failure());
        }
        let delta = choice.delta;
        if let Some((value, native)) = &self.finish {
            // The usage chunk repeats the finish choice with no new output.
            if choice.finish_reason.as_ref() != Some(value)
                || choice.native_finish_reason != *native
                || delta
                    .role
                    .as_deref()
                    .is_some_and(|role| role != "assistant")
                || delta
                    .content
                    .as_deref()
                    .is_some_and(|text| !text.is_empty())
                || delta
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| !calls.is_empty())
            {
                return Err(upstream_failure());
            }
            return Ok(chat_wire::StreamChunk {
                choices: Vec::new(),
                usage,
            });
        }
        if usage.is_some()
            || (choice.finish_reason.is_none() && choice.native_finish_reason.is_some())
            || !(choice.finish_reason.is_some() || meaningful(&delta))
        {
            return Err(upstream_failure());
        }
        if let Some(value) = &choice.finish_reason {
            self.finish = Some((value.clone(), choice.native_finish_reason));
        }
        Ok(chat_wire::StreamChunk {
            choices: vec![StreamChoice {
                index: choice.index,
                delta: chat_wire::StreamDelta {
                    role: delta.role,
                    content: delta.content,
                    reasoning: delta.reasoning,
                    reasoning_content: None,
                    tool_calls: delta
                        .tool_calls
                        .map(|calls| calls.into_iter().map(Into::into).collect()),
                },
                finish_reason: choice.finish_reason,
            }],
            usage: None,
        })
    }
}

/// A role, reasoning, non-empty text or tool calls.
fn meaningful(delta: &StreamDelta) -> bool {
    delta.role.is_some()
        || delta.reasoning.is_some()
        || delta.reasoning_details.is_some()
        || delta
            .content
            .as_deref()
            .is_some_and(|text| !text.is_empty())
        || delta
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty())
}

pub(super) fn event_stream(body: ResponseBody, public_model: ModelAlias) -> EventStream {
    Box::pin(ChatStream::<OpenRouter>::new(body, public_model))
}

#[cfg(test)]
mod tests {
    use crate::{
        adapter::chat_completions::stream_state::StreamState,
        core::{FinishReason, GatewayError, ModelAlias, NormalizedEvent, Usage},
    };

    use super::OpenRouter;

    const TOOL_CALL_STREAM: &str =
        include_str!("../../../tests/fixtures/openrouter/chat-stream-tool-call.sse");

    fn run(sse: &str) -> Result<Vec<NormalizedEvent>, GatewayError> {
        let mut state = StreamState::<OpenRouter>::new(ModelAlias("voxtral".into()));
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
                    reasoning_tokens: Some(0),
                    cached_input_tokens: Some(0),
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
