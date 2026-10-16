use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;

use crate::{
    adapter::{
        EventStream,
        diagnostics::UpstreamLabel,
        transport::{
            ResponseBody,
            sse::{SseFramer, SseRecord},
        },
    },
    core::{ErrorKind, GatewayError, MAX_ENCRYPTED_REASONING_BYTES, ModelAlias, NormalizedEvent},
};

use super::stream_state::StreamState;

/// A `signature_delta` carries a whole signature in one event.
const MAX_EVENT_BYTES: usize = MAX_ENCRYPTED_REASONING_BYTES + 64 * 1024;

struct AnthropicStream {
    body: Option<ResponseBody>,
    framer: SseFramer,
    records: VecDeque<SseRecord>,
    state: StreamState,
    pending: VecDeque<NormalizedEvent>,
    terminal: bool,
}

impl AnthropicStream {
    fn fail(&mut self, error: GatewayError) -> Poll<Option<Result<NormalizedEvent, GatewayError>>> {
        self.body.take();
        self.records.clear();
        self.pending.clear();
        self.terminal = true;
        Poll::Ready(Some(Err(error)))
    }

    fn record(&mut self, record: SseRecord) -> Result<(), GatewayError> {
        self.pending.extend(self.state.data(&record.data)?);
        if self.state.is_done() {
            self.body.take();
            self.records.clear();
            self.terminal = true;
        }
        Ok(())
    }
}

impl Stream for AnthropicStream {
    type Item = Result<NormalizedEvent, GatewayError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(event) = this.pending.pop_front() {
                return Poll::Ready(Some(Ok(event)));
            }
            if this.terminal {
                return Poll::Ready(None);
            }
            if let Some(record) = this.records.pop_front() {
                if let Err(error) = this.record(record) {
                    return this.fail(error);
                }
                continue;
            }
            let Some(body) = this.body.as_mut() else {
                return this.fail(upstream_failure());
            };
            match body.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    let (records, done) = match this.framer.feed_until(&bytes, |record| {
                        record.event.as_deref() == Some("message_stop")
                    }) {
                        Ok(result) => result,
                        Err(error) => return this.fail(error),
                    };
                    this.records.extend(records);
                    if done {
                        this.body.take();
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

fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}

pub(super) fn event_stream(
    body: ResponseBody,
    public_model: ModelAlias,
    capture_reasoning: bool,
    label: UpstreamLabel,
) -> EventStream {
    Box::pin(AnthropicStream {
        body: Some(body),
        framer: SseFramer::new_with_named_events().with_limits(MAX_EVENT_BYTES, MAX_EVENT_BYTES),
        records: VecDeque::new(),
        state: StreamState::new(public_model, capture_reasoning, label),
        pending: VecDeque::new(),
        terminal: false,
    })
}
