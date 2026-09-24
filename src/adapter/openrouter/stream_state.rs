use crate::core::{ErrorKind, FinishReason, GatewayError, ModelAlias, NormalizedEvent, Usage};

use super::{
    response,
    stream_wire::{self, StreamChoice, StreamChunk},
};

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
        if self.done || (!self.has_text && finish.reason != FinishReason::Length) {
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
        if let Some(value) = choice.finish_reason {
            meaningful = true;
            let reason = parse_finish_reason(&value)?;
            if !self.has_text {
                // A length stop may carry no visible output.
                if reason != FinishReason::Length {
                    return Err(upstream_failure());
                }
                self.start(&mut events);
            }
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
        {
            return Err(upstream_failure());
        }
        self.usage = Some(response::normalize_usage(usage)?);
        Ok(Vec::new())
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

fn parse_finish_reason(value: &str) -> Result<FinishReason, GatewayError> {
    match value {
        "stop" => Ok(FinishReason::Stop),
        "length" => Ok(FinishReason::Length),
        "content_filter" => Ok(FinishReason::ContentFilter),
        _ => Err(upstream_failure()),
    }
}

fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}
