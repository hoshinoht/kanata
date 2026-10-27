use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;

use crate::{
    core::{FinishReason, GatewayError, ModelAlias, NormalizedEvent},
    transport::{
        ResponseBody,
        sse::{SseFramer, SseRecord},
    },
    upstream_failure,
};

use super::stream_state::{StreamDialect, StreamState};

/// Chat-completions SSE as normalized events.
pub(crate) struct ChatStream<D> {
    body: Option<ResponseBody>,
    framer: SseFramer,
    state: StreamState<D>,
    records: VecDeque<SseRecord>,
    pending: VecDeque<NormalizedEvent>,
    /// Framing error raised after the queued records.
    failure: Option<GatewayError>,
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
            failure: None,
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
        self.failure = None;
        self.terminal = true;
    }

    /// Queues the records framed from `bytes` up to `[DONE]`; a framing error waits until
    /// they are parsed.
    fn feed(&mut self, bytes: &[u8]) {
        let (records, outcome) = self
            .framer
            .feed_records(bytes, |record| record.data == "[DONE]");
        self.records.extend(records);
        match outcome {
            Ok(false) => {}
            Ok(true) => {
                self.body.take();
            }
            Err(error) => {
                self.body.take();
                self.failure = Some(error);
            }
        }
    }

    /// Parses the queued records up to the first that yields events.
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
            if !events.is_empty() {
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
            if let Some(error) = this.failure.take() {
                return this.fail(error);
            }
            let Some(body) = this.body.as_mut() else {
                return this.fail(upstream_failure());
            };
            match body.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => this.feed(&bytes),
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

#[cfg(test)]
pub(crate) mod test_support {
    use futures_util::StreamExt;

    use crate::{
        EventStream,
        core::{ErrorKind, FinishReason, ModelAlias, NormalizedEvent},
        transport::ResponseBody,
    };

    const DELTA: &str = r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"visible"},"finish_reason":null}]}"#;
    const STOP: &str = r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;

    /// `body` read whole, one byte at a time and one record at a time.
    fn deliveries(body: &str) -> Vec<ResponseBody> {
        let bytes = body.as_bytes();
        let splits: [Vec<Vec<u8>>; 3] = [
            vec![bytes.to_vec()],
            bytes.chunks(1).map(<[u8]>::to_vec).collect(),
            body.split_inclusive("\n\n")
                .map(|record| record.as_bytes().to_vec())
                .collect(),
        ];
        splits
            .into_iter()
            .map(|chunks| -> ResponseBody {
                Box::pin(futures_util::stream::iter(
                    chunks.into_iter().map(|chunk| Ok(chunk.into())),
                ))
            })
            .collect()
    }

    /// Events up to the end or the first error.
    async fn drain(mut stream: EventStream) -> (Vec<NormalizedEvent>, Option<ErrorKind>) {
        let mut events = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => events.push(event),
                Err(error) => return (events, Some(error.kind)),
            }
        }
        (events, None)
    }

    /// Asserts a parse error, a framing error and junk after `[DONE]` give the same events
    /// however the bytes are split into reads.
    pub(crate) async fn assert_read_independent(
        stream: impl Fn(ResponseBody, ModelAlias) -> EventStream,
    ) {
        let started = vec![
            NormalizedEvent::ChatStarted {
                model: ModelAlias("local-chat".into()),
            },
            NormalizedEvent::ChatTextDelta {
                text: "visible".into(),
            },
        ];
        let mut completed = started.clone();
        completed.push(NormalizedEvent::ChatCompleted {
            finish_reason: FinishReason::Stop,
            usage: None,
        });
        let failed = Some(ErrorKind::UpstreamFailure);
        let cases = [
            (
                format!("{DELTA}\n\ndata: {{not-json}}\n\n"),
                &started,
                failed,
            ),
            (
                format!("{DELTA}\n\nevent: error\ndata: {{}}\n\n"),
                &started,
                failed,
            ),
            (
                format!("{DELTA}\n\n{STOP}\n\ndata: [DONE]\n\nbogus\n\n"),
                &completed,
                None,
            ),
        ];
        for (body, events, error) in cases {
            for delivery in deliveries(&body) {
                let outcome = drain(stream(delivery, ModelAlias("local-chat".into()))).await;
                assert_eq!(outcome, (events.clone(), error), "{body}");
            }
        }
    }
}
