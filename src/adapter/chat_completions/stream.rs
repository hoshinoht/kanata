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
    records: VecDeque<SseRecord>,
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
            records: VecDeque::new(),
            pending: VecDeque::new(),
            terminal: false,
            error_event,
        }
    }

    fn fail(&mut self, error: GatewayError) -> Poll<Option<Result<NormalizedEvent, GatewayError>>> {
        self.terminate();
        self.pending.clear();
        Poll::Ready(Some(Err(error)))
    }

    fn terminate(&mut self) {
        self.body.take();
        self.records.clear();
        self.terminal = true;
    }

    /// Queues the records framed from `bytes`.
    fn feed(&mut self, bytes: &[u8]) -> Result<(), GatewayError> {
        if D::RECORD_AT_A_TIME {
            let (records, done) = self
                .framer
                .feed_until(bytes, |record| record.data == "[DONE]")?;
            self.records.extend(records);
            if done {
                self.body.take();
            }
        } else {
            self.records.extend(self.framer.feed(bytes)?);
        }
        Ok(())
    }

    /// Parses the queued records, or with `RECORD_AT_A_TIME` those up to the first that
    /// yields events.
    fn records(&mut self) -> Result<(), GatewayError> {
        let mut events = Vec::new();
        while let Some(record) = self.records.pop_front() {
            if let Some(content_filtered) = self.error_event
                && record.event.as_deref() == Some("error")
            {
                if !content_filtered(&record.data) {
                    return Err(upstream_failure());
                }
                events.extend(self.state.interrupt(FinishReason::ContentFilter)?);
                self.terminate();
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
                self.terminate();
                break;
            }
            events.extend(self.state.data(&record.data)?);
            if D::RECORD_AT_A_TIME && !events.is_empty() {
                break;
            }
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
            if !this.records.is_empty() {
                if let Err(error) = this.records() {
                    return this.fail(error);
                }
                if let Some(event) = this.pending.pop_front() {
                    return Poll::Ready(Some(Ok(event)));
                }
                if this.terminal {
                    return Poll::Ready(None);
                }
                continue;
            }
            let Some(body) = this.body.as_mut() else {
                return this.fail(upstream_failure());
            };
            match body.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    if let Err(error) = this.feed(&bytes) {
                        return this.fail(error);
                    }
                }
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
