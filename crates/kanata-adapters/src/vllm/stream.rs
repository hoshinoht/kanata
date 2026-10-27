use crate::{
    chat_completions::{
        stream_state::StreamDialect,
        stream_wire::{StreamChunk, UsagePayload},
    },
    core::GatewayError,
};

/// Chat-completions SSE from vLLM, directly or behind LiteLLM.
#[derive(Default)]
pub(super) struct Vllm;

impl StreamDialect for Vllm {
    type Chunk = StreamChunk<UsagePayload>;
    type Usage = UsagePayload;
    // LiteLLM repeats an empty choice on the trailing usage chunk.
    const TRAILING_EMPTY_CHOICE: bool = true;
    // Some gateways repeat the final usage in a trailing chunk.
    const REPEATED_USAGE: bool = true;

    fn chunk(&mut self, chunk: Self::Chunk) -> Result<Self::Chunk, GatewayError> {
        Ok(chunk)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        chat_completions::{stream::ChatStream, stream_state::StreamState},
        core::{FinishReason, ModelAlias, NormalizedEvent, Usage},
    };

    use super::Vllm;

    const LITELLM_TOOL_STREAM: &str =
        include_str!("../../../../tests/fixtures/vllm/stream-litellm-tool-call.sse");
    const REPEATED_USAGE_STREAM: &str =
        include_str!("../../../../tests/fixtures/vllm/stream-repeated-usage-tool-call.sse");

    fn run(sse: &str) -> Result<Vec<NormalizedEvent>, crate::core::GatewayError> {
        let mut state = StreamState::<Vllm>::new(ModelAlias("omni".into()));
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
                    reasoning_tokens: Some(0),
                    cached_input_tokens: None,
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

    #[test]
    fn identical_repeated_usage_is_accepted_and_a_conflicting_one_is_not() {
        let events = run(REPEATED_USAGE_STREAM).expect("stream");
        assert!(matches!(
            events.last(),
            Some(NormalizedEvent::ChatCompleted {
                finish_reason: FinishReason::ToolCalls,
                usage: Some(Usage {
                    input_tokens: 507,
                    output_tokens: 61,
                    ..
                }),
            })
        ));
        let conflicting = REPEATED_USAGE_STREAM.replacen(
            r#""choices":[],"usage":{"prompt_tokens":507"#,
            r#""choices":[],"usage":{"prompt_tokens":508"#,
            1,
        );
        assert!(run(&conflicting).is_err());
    }

    #[tokio::test]
    async fn delivery_does_not_depend_on_read_boundaries() {
        crate::chat_completions::stream::test_support::assert_read_independent(|body, model| {
            Box::pin(ChatStream::<Vllm>::new(body, model))
        })
        .await;
    }
}
