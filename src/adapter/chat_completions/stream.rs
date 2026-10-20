use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;

use crate::{
    adapter::{
        transport::{
            ResponseBody,
            sse::{SseFramer, SseRecord},
        },
        upstream_failure,
    },
    core::{FinishReason, GatewayError, ModelAlias, NormalizedEvent},
};

use super::stream_state::{StreamDialect, StreamState};

/// Chat-completions SSE as normalized events.
pub(crate) struct ChatStream<D> {
    body: Option<ResponseBody>,
    framer: SseFramer,
    state: StreamState<D>,
    pending: VecDeque<NormalizedEvent>,
    terminal: bool,
    /// Reads an `event: error` record's data; `true` ends the stream with a content-filter
    /// stop, `false` fails it. Without it, any named event other than `message` fails.
    error_event: Option<fn(&str) -> bool>,
}

impl<D: StreamDialect> ChatStream<D> {
    pub(crate) fn new(body: ResponseBody, public_model: ModelAlias) -> Self {
        Self::build(body, public_model, SseFramer::new(), None)
    }

    pub(crate) fn with_error_events(
        body: ResponseBody,
        public_model: ModelAlias,
        content_filtered: fn(&str) -> bool,
    ) -> Self {
        Self::build(
            body,
            public_model,
            SseFramer::new_with_named_events(),
            Some(content_filtered),
        )
    }

    fn build(
        body: ResponseBody,
        public_model: ModelAlias,
        framer: SseFramer,
        error_event: Option<fn(&str) -> bool>,
    ) -> Self {
        Self {
            body: Some(body),
            framer,
            state: StreamState::new(public_model),
            pending: VecDeque::new(),
            terminal: false,
            error_event,
        }
    }

    fn fail(&mut self, error: GatewayError) -> Poll<Option<Result<NormalizedEvent, GatewayError>>> {
        self.body.take();
        self.pending.clear();
        self.terminal = true;
        Poll::Ready(Some(Err(error)))
    }

    fn records(&mut self, records: Vec<SseRecord>) -> Result<(), GatewayError> {
        let mut events = Vec::new();
        for record in records {
            if self.terminal {
                return Err(upstream_failure());
            }
            if let Some(content_filtered) = self.error_event
                && record.event.as_deref() == Some("error")
            {
                if !content_filtered(&record.data) {
                    return Err(upstream_failure());
                }
                events.extend(self.state.interrupt(FinishReason::ContentFilter)?);
                self.body.take();
                self.terminal = true;
                break;
            }
            if record
                .event
                .as_deref()
                .is_some_and(|event| event != "message")
            {
                return Err(upstream_failure());
            }
            if record.data == "[DONE]" {
                events.push(self.state.done()?);
                self.body.take();
                self.terminal = true;
                break;
            }
            events.extend(self.state.data(&record.data)?);
        }
        self.pending.extend(events);
        Ok(())
    }
}

impl<D: StreamDialect> Stream for ChatStream<D> {
    type Item = Result<NormalizedEvent, GatewayError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(event) = this.pending.pop_front() {
            return Poll::Ready(Some(Ok(event)));
        }
        if this.terminal {
            return Poll::Ready(None);
        }
        loop {
            let Some(body) = this.body.as_mut() else {
                return this.fail(upstream_failure());
            };
            match body.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => match this.framer.feed(&bytes) {
                    Ok(records) => {
                        if let Err(error) = this.records(records) {
                            return this.fail(error);
                        }
                        if let Some(event) = this.pending.pop_front() {
                            return Poll::Ready(Some(Ok(event)));
                        }
                        if this.terminal {
                            return Poll::Ready(None);
                        }
                    }
                    Err(error) => return this.fail(error),
                },
                Poll::Ready(Some(Err(error))) => return this.fail(error),
                Poll::Ready(None) => {
                    if let Err(error) = this.framer.finish() {
                        return this.fail(error);
                    }
                    return this.fail(upstream_failure());
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
