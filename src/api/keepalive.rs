use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use axum::body::Bytes;
use futures_core::Stream;
use tokio::time::{Instant, Sleep};

/// SSE comment frame; clients ignore it per the event-stream format.
pub(in crate::api) const COMMENT: &[u8] = b": keepalive\n\n";

/// Emits `frame` whenever `interval` passes without any body bytes.
///
/// Apply only to a committed body: the inner stream is polled first and its
/// pending future is never dropped or recreated, so upstream idle and overall
/// deadlines measured inside it are unaffected by keepalive frames.
pub(in crate::api) fn keepalive<S>(
    stream: S,
    interval: Duration,
    frame: &'static [u8],
) -> KeepAlive<S>
where
    S: Stream<Item = Result<Bytes, Infallible>>,
{
    KeepAlive {
        inner: Box::pin(stream),
        interval,
        frame,
        timer: Box::pin(tokio::time::sleep(interval)),
    }
}

pub(in crate::api) struct KeepAlive<S> {
    inner: Pin<Box<S>>,
    interval: Duration,
    frame: &'static [u8],
    timer: Pin<Box<Sleep>>,
}

impl<S> Stream for KeepAlive<S>
where
    S: Stream<Item = Result<Bytes, Infallible>>,
{
    type Item = Result<Bytes, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Poll::Ready(item) = this.inner.as_mut().poll_next(cx) {
            if item.is_some() {
                this.timer.as_mut().reset(Instant::now() + this.interval);
            }
            return Poll::Ready(item);
        }
        if this.timer.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        this.timer.as_mut().reset(Instant::now() + this.interval);
        Poll::Ready(Some(Ok(Bytes::from_static(this.frame))))
    }
}
