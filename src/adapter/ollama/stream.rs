use crate::{
    adapter::{
        EventStream,
        chat_completions::{stream::ChatStream, stream_state::StreamDialect},
        diagnostics::ProviderError,
        transport::ResponseBody,
    },
    core::ModelAlias,
};

use super::{apple_fm, response::UsagePayload};

enum Ollama {}

impl StreamDialect for Ollama {
    type Usage = UsagePayload;
    const TRAILING_EMPTY_CHOICE: bool = false;
    const REPEATED_USAGE: bool = false;
}

pub(super) fn event_stream(
    body: ResponseBody,
    public_model: ModelAlias,
    apple_fm: bool,
) -> EventStream {
    if apple_fm {
        // fm serve sends `event: error`; other named events still fail.
        Box::pin(ChatStream::<Ollama>::with_error_events(
            body,
            public_model,
            guardrail_error,
        ))
    } else {
        Box::pin(ChatStream::<Ollama>::new(body, public_model))
    }
}

/// fm serve reports a guardrail refusal as a mid-stream error event.
fn guardrail_error(data: &str) -> bool {
    let error = serde_json::from_str::<serde_json::Value>(data)
        .map(|value| ProviderError::from_json(&value))
        .unwrap_or_default();
    apple_fm::classify(500, &error) == Some(apple_fm::Failure::Guardrail)
}
