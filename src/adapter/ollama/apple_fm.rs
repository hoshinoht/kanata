//! `fm serve` error shapes that map to something better than an upstream failure.

use crate::adapter::diagnostics::ProviderError;
use crate::core::{
    ChatMessage, ChatResponse, ChatRole, FinishReason, GatewayError, ModelAlias, NormalizedEvent,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Failure {
    /// The on-device safety guardrails refused the prompt or reply.
    Guardrail,
    /// The transcript exceeded the on-device context window.
    ContextOverflow,
}

/// fm serve reports both failures as HTTP 500, including inside a stream.
pub(super) fn classify(status: u16, error: &ProviderError) -> Option<Failure> {
    if status != 500 {
        return None;
    }
    let message = error.message.as_deref()?.to_ascii_lowercase();
    if message.contains("guardrail") {
        Some(Failure::Guardrail)
    } else if message.contains("context size") || message.contains("context window") {
        Some(Failure::ContextOverflow)
    } else {
        None
    }
}

/// An empty reply stopped by the content filter.
pub(super) fn filtered_reply(model: ModelAlias) -> ChatResponse {
    ChatResponse {
        model,
        message: ChatMessage {
            role: ChatRole::Assistant,
            content: Vec::new(),
        },
        finish_reason: FinishReason::ContentFilter,
        usage: None,
    }
}

/// The same stop as a stream: start, then a content-filter completion.
pub(super) fn filtered_events(model: ModelAlias) -> Vec<Result<NormalizedEvent, GatewayError>> {
    vec![
        Ok(NormalizedEvent::ChatStarted { model }),
        Ok(NormalizedEvent::ChatCompleted {
            finish_reason: FinishReason::ContentFilter,
            usage: None,
        }),
    ]
}
