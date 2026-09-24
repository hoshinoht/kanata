use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;

use crate::{
    adapter::transport::ResponseBody,
    core::{ErrorKind, GatewayError, ModelAlias, NormalizedEvent},
};

use super::{sse::SseFramer, stream_state::StreamState};

pub(super) struct OllamaStream {
    body: Option<ResponseBody>,
    framer: SseFramer,
    state: StreamState,
    pending: VecDeque<NormalizedEvent>,
    terminal: bool,
}

impl OllamaStream {
    pub(super) fn new(body: ResponseBody, public_model: ModelAlias) -> Self {
        Self {
            body: Some(body),
            framer: SseFramer::new(),
            state: StreamState::new(public_model),
            pending: VecDeque::new(),
            terminal: false,
        }
    }

    fn fail(&mut self, error: GatewayError) -> Poll<Option<Result<NormalizedEvent, GatewayError>>> {
        self.body.take();
        self.pending.clear();
        self.terminal = true;
        Poll::Ready(Some(Err(error)))
    }

    fn records(&mut self, records: Vec<super::sse::SseRecord>) -> Result<(), GatewayError> {
        let mut events = Vec::new();
        for record in records {
            if self.terminal {
                return Err(upstream_failure());
            }
            if record.data == "[DONE]" {
                events.push(self.state.done()?);
                self.body.take();
                self.terminal = true;
                break;
            } else {
                events.extend(self.state.data(&record.data)?);
            }
        }
        self.pending.extend(events);
        Ok(())
    }
}

fn upstream_failure() -> GatewayError {
    GatewayError {
        kind: ErrorKind::UpstreamFailure,
    }
}

impl Stream for OllamaStream {
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
                return this.fail(GatewayError {
                    kind: ErrorKind::UpstreamFailure,
                });
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
                    return this.fail(this.state.eof());
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
