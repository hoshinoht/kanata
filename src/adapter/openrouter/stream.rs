use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;

use crate::{
    adapter::{
        EventStream,
        transport::{
            ResponseBody,
            sse::{SseFramer, SseRecord},
        },
    },
    core::{ErrorKind, GatewayError, ModelAlias, NormalizedEvent},
};

use super::stream_state::StreamState;

pub(super) struct OpenRouterStream {
    body: Option<ResponseBody>,
    framer: SseFramer,
    records: VecDeque<SseRecord>,
    state: StreamState,
    pending: VecDeque<NormalizedEvent>,
    terminal: bool,
}

impl OpenRouterStream {
    pub(super) fn new(body: ResponseBody, public_model: ModelAlias) -> Self {
        Self {
            body: Some(body),
            framer: SseFramer::new(),
            records: VecDeque::new(),
            state: StreamState::new(public_model),
            pending: VecDeque::new(),
            terminal: false,
        }
    }

    fn fail(&mut self, error: GatewayError) -> Poll<Option<Result<NormalizedEvent, GatewayError>>> {
        self.body.take();
        self.records.clear();
        self.pending.clear();
        self.terminal = true;
        Poll::Ready(Some(Err(error)))
    }

    fn record(&mut self, record: SseRecord) -> Result<(), GatewayError> {
        if record.data == "[DONE]" {
            let event = self.state.done()?;
            self.body.take();
            self.records.clear();
            self.pending.push_back(event);
            self.terminal = true;
        } else {
            self.pending.extend(self.state.data(&record.data)?);
        }
        Ok(())
    }
}

impl Stream for OpenRouterStream {
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
            if let Some(record) = this.records.pop_front() {
                if let Err(error) = this.record(record) {
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
                    let (records, done) = match this
                        .framer
                        .feed_until(&bytes, |record| record.data == "[DONE]")
                    {
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
                    return this.fail(this.state.eof());
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

pub(super) fn event_stream(body: ResponseBody, public_model: ModelAlias) -> EventStream {
    Box::pin(OpenRouterStream::new(body, public_model))
}
